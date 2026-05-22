// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Unix-socket transport + job HTTP layer for the admin API.
//!
//! The transport (bind, chmod 0660, accept loop, peer-cred
//! injection) is product-agnostic — every consumer hands in a
//! pre-built `axum::Router` with its product-specific routes +
//! state. The job HTTP layer is also product-agnostic by way of
//! the [`HasJobs`] trait: products plug in their dispatch closure
//! and the shared router does the rest.

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use shared_admin_proto::{JobAccepted, JobEvent};
use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tokio::net::UnixListener;
use tower_service::Service;
use tracing::{debug, info, warn};

use crate::jobs::{JobEmitter, JobRegistry};
use crate::peer::PeerCred;

/// Default file mode for the admin socket. `0o660` = owner + group
/// rw, world none. Group is whatever group owns the daemon process
/// at startup; the systemd unit's `Group=` pins the per-product
/// group so the CLI can talk to the socket via group membership.
const ADMIN_SOCKET_MODE: u32 = 0o660;

/// Bind the admin Unix socket and serve the caller-built router.
///
/// Removes any stale socket file at `socket_path` before binding —
/// covers daemon-crash leftovers. On clean shutdown the socket is
/// left in place; the next startup reclaims it.
///
/// The accept loop captures `SO_PEERCRED` per connection and
/// injects a [`PeerCred`] into every request's extensions so
/// handlers can pull it via the extractor and stamp it on audit
/// entries.
pub async fn run_admin_server(socket_path: PathBuf, router: Router) -> Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale admin socket {}", socket_path.display()))?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating admin socket parent dir {}", parent.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding admin socket {}", socket_path.display()))?;

    let mut perms = std::fs::metadata(&socket_path)
        .with_context(|| format!("statting admin socket {}", socket_path.display()))?
        .permissions();
    perms.set_mode(ADMIN_SOCKET_MODE);
    std::fs::set_permissions(&socket_path, perms).with_context(|| {
        format!(
            "chmod {:#o} on admin socket {}",
            ADMIN_SOCKET_MODE,
            socket_path.display()
        )
    })?;

    info!(
        "admin Unix socket listening on {} (mode {:#o})",
        socket_path.display(),
        ADMIN_SOCKET_MODE
    );

    // axum::serve(...) only accepts TcpListener, so drive hyper over
    // the UnixListener directly. Each accepted stream is wrapped in
    // hyper-util's TokioIo and handed to a hyper auto-builder; the
    // caller's Router is the tower service.
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("admin accept failed: {e}");
                continue;
            }
        };

        // Capture peer credentials at accept time; the same
        // uid/gid/pid is then injected into every request extension
        // on this connection so handlers can pull it via the
        // PeerCred extractor for audit logging.
        let cred = stream.peer_cred().ok().map(|c| PeerCred {
            uid: c.uid(),
            gid: c.gid(),
            pid: c.pid(),
        });

        let io = TokioIo::new(stream);
        let app = router.clone();
        let svc = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
            if let Some(c) = cred.clone() {
                req.extensions_mut().insert(c);
            }
            let mut app = app.clone();
            async move {
                let response = app.call(req).await?;
                Ok::<_, Infallible>(response)
            }
        });

        tokio::spawn(async move {
            if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                debug!("admin connection error: {e}");
            }
        });
    }
}

/// Product state has a [`JobRegistry`] the shared job handlers can
/// reach. Each daemon's `AdminState` impls this so `jobs_router`
/// can pull the registry out of `State<S>` generically.
pub trait HasJobs: Clone + Send + Sync + 'static {
    fn jobs(&self) -> &JobRegistry;
}

/// Build the `/api/v1/jobs/*` sub-router. Caller `.merge()`s the
/// result into their product router and passes the merged thing to
/// [`run_admin_server`].
///
/// `dispatch` is the per-product router that spawns the right
/// worker task for `kind`. Returns `Err(reason)` for an unknown
/// kind so the HTTP handler can return 400 before any job is
/// registered.
pub fn jobs_router<S, F>(state: S, dispatch: F) -> Router
where
    S: HasJobs,
    F: Fn(&str, serde_json::Value, JobEmitter, S) -> Result<(), String>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let api_state = JobsApiState { state, dispatch };
    Router::new()
        .route("/api/v1/jobs/:kind", post(jobs_create::<S, F>))
        .route("/api/v1/jobs/:id/events", get(jobs_events::<S, F>))
        .with_state(api_state)
}

#[derive(Clone)]
struct JobsApiState<S, F> {
    state: S,
    dispatch: F,
}

/// `POST /api/v1/jobs/<kind>`. Body is JSON, shape varies per
/// kind. Returns 202 Accepted with the job id, or 400 if the kind
/// is unknown.
async fn jobs_create<S, F>(
    State(api): State<JobsApiState<S, F>>,
    cred: PeerCred,
    AxumPath(kind): AxumPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse
where
    S: HasJobs,
    F: Fn(&str, serde_json::Value, JobEmitter, S) -> Result<(), String>
        + Clone
        + Send
        + Sync
        + 'static,
{
    // Reap finished jobs opportunistically. The retention TTL keeps
    // recently-finished jobs reachable for late stream subscribers.
    api.state.jobs().reap().await;

    let body_value = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);

    let (job_id, started_at, emitter) = api.state.jobs().create(&kind).await;

    info!(
        "admin: job {} dispatched (kind={}, peer=uid:{} pid:{:?})",
        job_id, kind, cred.uid, cred.pid
    );

    if let Err(e) = (api.dispatch)(&kind, body_value, emitter.clone(), api.state.clone()) {
        // Unknown kind — emit a synthetic Done so any subscriber
        // that raced the response gets a clean terminal event,
        // then 400.
        emitter.emit(JobEvent::done_with_error(2, e.clone())).await;
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response();
    }

    let resp = JobAccepted {
        job_id,
        kind,
        started_at: started_at.to_rfc3339(),
    };
    (StatusCode::ACCEPTED, Json(resp)).into_response()
}

/// `GET /api/v1/jobs/{id}/events`. Streams NDJSON events: one JSON
/// object per line, terminated by `\n`. The terminal `Done` event
/// is always the last line; the connection closes immediately
/// after.
///
/// Replays the entire event log from index 0, so a client that
/// connects after the worker has already produced output still
/// sees the full transcript. This is what makes the two-step
/// POST-then-GET handshake safe — the worker can't outrun the
/// subscriber.
async fn jobs_events<S, F>(
    State(api): State<JobsApiState<S, F>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body>
where
    S: HasJobs,
    F: Fn(&str, serde_json::Value, JobEmitter, S) -> Result<(), String>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let handle = match api.state.jobs().get(&id).await {
        Some(h) => h,
        None => {
            let body = serde_json::json!({
                "error": format!("job '{}' not found (may have been reaped)", id),
            });
            let bytes = serde_json::to_vec(&body).unwrap_or_default();
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes))
                .expect("static response builder");
        }
    };

    // Build a Stream<Item = Result<Bytes, Infallible>> that walks
    // the event log to completion. async-stream's stream! macro
    // keeps the loop readable; the cursor + Notify dance lives in
    // JobHandle::next_events.
    let stream = async_stream::stream! {
        let mut cursor = 0usize;
        loop {
            let evs = handle.next_events(&mut cursor).await;
            if evs.is_empty() {
                break;
            }
            for ev in evs {
                let mut line = match serde_json::to_vec(&ev) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("job event serialize failed: {e}");
                        continue;
                    }
                };
                line.push(b'\n');
                yield Ok::<Bytes, Infallible>(Bytes::from(line));
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        // Disable any naive proxy buffering. (We don't use a proxy
        // on the unix socket, but the header is cheap and documents
        // the intent for any future TCP exposure.)
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .expect("stream response builder")
}
