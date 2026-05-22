// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thin client over a daemon's admin Unix socket.
//!
//! Connects to the socket path advertised by `shared_naming::ProductIdentity`
//! (override via the `{NAME}_ADMIN_SOCKET` env var) and speaks
//! HTTP/1.1 to the `/api/v1/*` surface served by
//! `shared-admin-server`. The kernel authenticates the connection
//! via SO_PEERCRED — any user in the daemon's group has access;
//! everyone else gets `EPERM` at `connect()` time.
//!
//! Daemon-down fallback: callers that have a sane offline path
//! (e.g. `volume create` writing the manifest directly) probe with
//! [`AdminClient::ping`] first and fall back when the socket can't
//! be reached. Daemon-routed operations refuse with a clear
//! "is the daemon running?" error.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Empty, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use shared_admin_proto::{JobAccepted, JobEvent};
use shared_naming::ProductIdentity;
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

/// Client handle for the daemon's admin Unix socket.
///
/// Stateless — every request opens a fresh connection. That's fine
/// for human-paced CLI invocations and it keeps connection-pool
/// management out of scope. Long-running streaming endpoints (jobs,
/// NDJSON) hold the connection for their own lifetime and bypass
/// the simple `get_json` helper.
pub struct AdminClient {
    socket_path: PathBuf,
    host_header: &'static str,
}

impl AdminClient {
    /// Construct from an explicit socket path. `host_header` is the
    /// product's short name (`thurvtl` / `thurvsa`) — `hyper`
    /// requires a Host header on HTTP/1.1 requests even over Unix
    /// sockets; the daemon doesn't actually route on it.
    pub fn new(socket_path: PathBuf, host_header: &'static str) -> Self {
        Self {
            socket_path,
            host_header,
        }
    }

    /// Construct from a [`ProductIdentity`] using the canonical
    /// admin socket path (`identity.admin_socket`). Reads
    /// `{NAME}_ADMIN_SOCKET` (uppercased product name) as an
    /// optional override — same env var the daemon honors at bind
    /// time.
    pub fn auto_discover(identity: &'static ProductIdentity) -> Self {
        let env_var = format!("{}_ADMIN_SOCKET", identity.name.to_ascii_uppercase());
        let path = match std::env::var(&env_var) {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => PathBuf::from(identity.admin_socket),
        };
        Self::new(path, identity.name)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Probe the socket: returns `true` if a UnixStream connect
    /// succeeds (daemon is running and the socket is bound),
    /// `false` otherwise. Used by the CLI to decide whether to
    /// fall back to daemon-down paths.
    pub async fn ping(&self) -> bool {
        UnixStream::connect(&self.socket_path).await.is_ok()
    }

    /// `GET <path>` over the admin socket; deserialize the JSON body.
    ///
    /// Errors if the socket isn't reachable, the daemon returns a
    /// non-2xx status, or the body isn't valid JSON. The error
    /// message includes the socket path so operators with multiple
    /// `data_dir`s can tell which daemon failed.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let bytes = self.get_bytes(path).await?;
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decoding JSON response from {} {}",
                self.socket_path.display(),
                path
            )
        })
    }

    /// `POST <path>` with a JSON body; deserialize the JSON response.
    pub async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.method_json("POST", path, Some(body)).await
    }

    /// `POST <path>` with a JSON body; discard the response. Use this
    /// for handlers that reply with `204 No Content` (empty body) —
    /// `post_json` would fail trying to JSON-decode the empty body.
    pub async fn post_unit<B: serde::Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let payload = serde_json::to_vec(body).context("encoding request body")?;
        let _ = self.send_with_body("POST", path, payload).await?;
        Ok(())
    }

    /// `PUT <path>` with a JSON body; deserialize the JSON response.
    pub async fn put_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.method_json("PUT", path, Some(body)).await
    }

    /// `DELETE <path>` with an optional JSON body; deserialize the
    /// JSON response. Most DELETE callers pass `None`; pass
    /// `Some(&body)` when the daemon endpoint expects parameters in
    /// the request body.
    pub async fn delete_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        self.method_json("DELETE", path, body).await
    }

    async fn method_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let payload = match body {
            Some(b) => serde_json::to_vec(b).context("encoding request body")?,
            None => Vec::new(),
        };
        let bytes = self.send_with_body(method, path, payload).await?;
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decoding JSON response from {} {} {}",
                method,
                self.socket_path.display(),
                path
            )
        })
    }

    async fn send_with_body(
        &self,
        method: &'static str,
        path: &str,
        payload: Vec<u8>,
    ) -> Result<Bytes> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "connecting to admin socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .with_context(|| format!("HTTP handshake on {}", self.socket_path.display()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", self.host_header)
            .header("Content-Type", "application/json")
            .body(Full::<Bytes>::new(payload.into()))
            .context("building HTTP request")?;

        let res = sender
            .send_request(req)
            .await
            .with_context(|| format!("sending {} {}", method, path))?;

        let status = res.status();
        let body = res
            .collect()
            .await
            .with_context(|| format!("reading response body from {} {}", method, path))?
            .to_bytes();

        if !status.is_success() {
            return Err(anyhow!(
                "{} {} returned HTTP {}: {}",
                method,
                path,
                status.as_u16(),
                friendly_error_snippet(&body),
            ));
        }

        Ok(body)
    }

    /// Submit a long-running job and stream its NDJSON event log to
    /// `on_event` until the worker emits a terminal `Done` event.
    /// Returns the exit code from that `Done`. The CLI side renders
    /// log/progress events as they arrive; the structured `result`
    /// payload (if any) is captured in `on_event` for post-stream
    /// pretty-printing.
    ///
    /// Two HTTP round-trips: `POST /api/v1/jobs/<kind>` to register
    /// the job (response carries the id), then `GET
    /// /api/v1/jobs/{id}/events` to consume the stream. The split
    /// matches the spec — the worker can outlive the connection if
    /// we ever need that, and a CLI reconnect always replays the
    /// full transcript.
    pub async fn run_job<B, F>(&self, kind: &str, body: &B, mut on_event: F) -> Result<i32>
    where
        B: Serialize,
        F: FnMut(JobEvent),
    {
        let accepted: JobAccepted = self
            .post_json(&format!("/api/v1/jobs/{}", kind), body)
            .await?;

        self.consume_job_stream(&accepted.job_id, &mut on_event)
            .await
    }

    async fn consume_job_stream(
        &self,
        job_id: &str,
        on_event: &mut dyn FnMut(JobEvent),
    ) -> Result<i32> {
        let path = format!("/api/v1/jobs/{}/events", job_id);
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "connecting to admin socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .with_context(|| format!("HTTP handshake on {}", self.socket_path.display()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method("GET")
            .uri(&path)
            .header("Host", self.host_header)
            .body(Empty::<Bytes>::new())
            .context("building HTTP request")?;
        let res = sender
            .send_request(req)
            .await
            .with_context(|| format!("sending GET {}", path))?;

        let status = res.status();
        if !status.is_success() {
            let body = res
                .collect()
                .await
                .context("reading error-response body")?
                .to_bytes();
            return Err(anyhow!(
                "GET {} returned HTTP {}: {}",
                path,
                status.as_u16(),
                friendly_error_snippet(&body),
            ));
        }

        let mut body = res.into_body();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let frame = match body.frame().await {
                Some(f) => f.context("reading stream frame")?,
                None => break,
            };
            if let Some(data) = frame.data_ref() {
                buf.extend_from_slice(data);
                for parsed in drain_lines(&mut buf) {
                    let event = parsed?;
                    let done_code = match &event {
                        JobEvent::Done { exit_code, .. } => Some(*exit_code),
                        _ => None,
                    };
                    on_event(event);
                    if let Some(code) = done_code {
                        return Ok(code);
                    }
                }
            }
        }

        Err(anyhow!(
            "job stream {} ended before terminal Done event",
            path
        ))
    }

    async fn get_bytes(&self, path: &str) -> Result<Bytes> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "connecting to admin socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .with_context(|| format!("HTTP handshake on {}", self.socket_path.display()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method("GET")
            .uri(path)
            .header("Host", self.host_header)
            .body(Empty::<Bytes>::new())
            .context("building HTTP request")?;

        let res = sender
            .send_request(req)
            .await
            .with_context(|| format!("sending GET {}", path))?;

        let status = res.status();
        let body = res
            .collect()
            .await
            .with_context(|| format!("reading response body from GET {}", path))?
            .to_bytes();

        if !status.is_success() {
            return Err(anyhow!(
                "GET {} returned HTTP {}: {}",
                path,
                status.as_u16(),
                friendly_error_snippet(&body),
            ));
        }

        Ok(body)
    }
}

/// Render a non-2xx response body as the friendliest available
/// string. Prefers `{ "error": "..." }` from the daemon's standard
/// error envelope; falls back to the raw bytes as UTF-8.
fn friendly_error_snippet(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_string)))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// Percent-encode an arbitrary string for use in a URL path
/// segment. RFC 3986 unreserved set passes through; everything
/// else is `%HH`-escaped.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.bytes() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(c as char)
            }
            _ => out.push_str(&format!("%{:02X}", c)),
        }
    }
    out
}

/// Pull complete `\n`-terminated lines out of `buf`, parsing each as
/// a `JobEvent`. Partial trailing data is left in `buf` for the next
/// frame. Empty lines are skipped.
fn drain_lines(buf: &mut Vec<u8>) -> Vec<Result<JobEvent>> {
    let mut out = Vec::new();
    while let Some(idx) = buf.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = buf.drain(..=idx).collect();
        let line_no_nl = &line[..idx];
        if line_no_nl.is_empty() {
            continue;
        }
        let parsed = serde_json::from_slice::<JobEvent>(line_no_nl).with_context(|| {
            format!(
                "decoding job event line: {}",
                String::from_utf8_lossy(line_no_nl)
            )
        });
        out.push(parsed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_unreserved_passthrough() {
        let s = "abcXYZ0189-_.~";
        assert_eq!(urlencode(s), s);
    }

    #[test]
    fn urlencode_escapes_space_slash_questionmark() {
        assert_eq!(urlencode(" "), "%20");
        assert_eq!(urlencode("/"), "%2F");
        assert_eq!(urlencode("?"), "%3F");
        assert_eq!(urlencode("a b/c?d"), "a%20b%2Fc%3Fd");
    }

    #[test]
    fn urlencode_multibyte_utf8_emits_percent_per_byte() {
        // U+00E9 'é' = 0xC3 0xA9 in UTF-8 → "%C3%A9"
        assert_eq!(urlencode("é"), "%C3%A9");
        // U+1F4A9 = F0 9F 92 A9 → four percent groups
        assert_eq!(urlencode("\u{1F4A9}"), "%F0%9F%92%A9");
    }

    #[test]
    fn urlencode_empty_string() {
        assert_eq!(urlencode(""), "");
    }

    #[test]
    fn friendly_error_snippet_extracts_error_field() {
        let body = Bytes::from(r#"{"error":"bad foo"}"#);
        assert_eq!(friendly_error_snippet(&body), "bad foo");
    }

    #[test]
    fn friendly_error_snippet_falls_back_to_raw_when_not_envelope() {
        let body = Bytes::from("plain text 5xx page");
        assert_eq!(friendly_error_snippet(&body), "plain text 5xx page");
    }

    #[test]
    fn friendly_error_snippet_falls_back_for_json_without_error_field() {
        let body = Bytes::from(r#"{"detail":"oops"}"#);
        // No top-level `error` key → render raw JSON text.
        assert_eq!(friendly_error_snippet(&body), r#"{"detail":"oops"}"#);
    }

    #[test]
    fn friendly_error_snippet_handles_invalid_utf8_without_panic() {
        let body = Bytes::from_static(&[0xff, 0xfe, 0xfd]);
        let snip = friendly_error_snippet(&body);
        // String::from_utf8_lossy substitutes replacement chars.
        assert!(snip.contains('\u{FFFD}'));
    }

    #[test]
    fn drain_lines_splits_on_newline() {
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"{\"type\":\"log\",\"level\":\"info\",\"message\":\"a\"}\n\
              {\"type\":\"done\",\"exit_code\":0}\n",
        );
        let events: Vec<JobEvent> = drain_lines(&mut buf)
            .into_iter()
            .map(|r| r.expect("valid"))
            .collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], JobEvent::Log { .. }));
        assert!(matches!(events[1], JobEvent::Done { exit_code: 0, .. }));
        assert!(buf.is_empty(), "buffer should be drained");
    }

    #[test]
    fn drain_lines_partial_line_held_across_calls() {
        let mut buf = Vec::new();
        // First frame: complete first event + partial second.
        buf.extend_from_slice(
            b"{\"type\":\"log\",\"level\":\"info\",\"message\":\"a\"}\n\
              {\"type\":\"do",
        );
        let first = drain_lines(&mut buf);
        assert_eq!(first.len(), 1, "only the first complete line drains");
        assert!(first[0].is_ok());
        assert!(!buf.is_empty(), "partial line buffered");

        // Second frame: complete the second event.
        buf.extend_from_slice(b"ne\",\"exit_code\":7}\n");
        let second = drain_lines(&mut buf);
        assert_eq!(second.len(), 1);
        match second.into_iter().next().unwrap().unwrap() {
            JobEvent::Done { exit_code, .. } => assert_eq!(exit_code, 7),
            other => panic!("expected Done, got {:?}", other),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_lines_invalid_json_surfaces_error_without_panic() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"not json at all\n");
        let results = drain_lines(&mut buf);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(buf.is_empty(), "errored line still consumed from buffer");
    }

    #[test]
    fn drain_lines_skips_empty_lines() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\n\n{\"type\":\"done\",\"exit_code\":0}\n\n");
        let results = drain_lines(&mut buf);
        assert_eq!(results.len(), 1, "empty lines must not surface");
        assert!(results[0].is_ok());
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_lines_no_terminator_returns_nothing() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"{\"type\":\"log\",\"level\":\"info\",\"message\":\"x\"}");
        let results = drain_lines(&mut buf);
        assert!(results.is_empty(), "no newline -> nothing to drain");
        assert!(!buf.is_empty(), "incomplete line stays buffered");
    }
}
