// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Async job runner for long-running admin operations.
//!
//! `system gc` / `verify` / `stats` / `cloud check` / `audit *` /
//! `license` finish in milliseconds on small libraries and minutes
//! at TB scale; the operator wants live progress in either case.
//! Rather than holding a single HTTP request open for the whole run
//! (which couples the connection lifetime to job liveness), we
//! split the lifecycle into two endpoints:
//!
//!   POST /api/v1/jobs/<kind>      → spawn job, return id + start ts
//!   GET  /api/v1/jobs/{id}/events → stream NDJSON events to completion
//!
//! Each [`JobEvent`] is one JSON object (`log`, `progress`, `result`,
//! `done`) emitted on its own line. The CLI client reads the
//! stream, renders events as text, and exits with the exit code
//! from the terminal `done` event.
//!
//! Storage: one `JobInner` per job, behind `Arc`, with a
//! `Mutex<Vec<JobEvent>>` event log + a `Notify` to wake
//! subscribers. Subscribers replay the entire log from index 0 on
//! each connect, so a CLI that reconnects (rare) sees the full
//! transcript. This is not a long-term work queue — finished jobs
//! stay in the registry until the GC loop reaps them after a TTL,
//! then re-running the same job must be re-POSTed.

use chrono::{DateTime, Utc};
use shared_admin_proto::JobEvent;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Per-job mutable state. Lives behind `Arc` and is shared between
/// the worker task (which writes via [`JobEmitter`]) and any number
/// of streaming subscribers (which read under `events` lock).
struct JobInner {
    kind: String,
    started_at: DateTime<Utc>,
    events: Mutex<Vec<JobEvent>>,
    /// Wakes streamers when a new event has been appended or the job
    /// has finished. Producers call `notify_waiters()` after every
    /// `events.push(...)` and the final terminal write.
    notify: Notify,
    /// Set after the terminal `Done` event has been pushed.
    /// Streamers observe this *after* draining the event log so they
    /// always emit `Done` as the last line.
    finished: AtomicBool,
}

impl JobInner {
    fn new(kind: String, started_at: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            started_at,
            events: Mutex::new(Vec::new()),
            notify: Notify::new(),
            finished: AtomicBool::new(false),
        })
    }
}

/// Cheap clone-able handle for the worker task. Methods all take
/// `&self` so the closure can fan out events from multiple
/// callbacks.
#[derive(Clone)]
pub struct JobEmitter {
    inner: Arc<JobInner>,
}

impl JobEmitter {
    /// Append an event and notify any waiting streamers. `Done`
    /// flips the finished bit before notifying so a streamer that
    /// wakes up, drains the queue, and re-checks finished sees
    /// `true` and returns.
    pub async fn emit(&self, ev: JobEvent) {
        let terminal = ev.is_terminal();
        {
            let mut v = self.inner.events.lock().await;
            v.push(ev);
        }
        if terminal {
            self.inner.finished.store(true, Ordering::Release);
        }
        self.inner.notify.notify_waiters();
    }

    /// Convenience: emit an info log line.
    pub async fn info(&self, message: impl Into<String>) {
        self.emit(JobEvent::info(message)).await;
    }
    /// Convenience: emit a warning.
    pub async fn warn(&self, message: impl Into<String>) {
        self.emit(JobEvent::warn(message)).await;
    }
    /// Convenience: emit an error log line. (Distinct from `Done`
    /// with non-zero exit — error logs are mid-run diagnostics.)
    pub async fn error(&self, message: impl Into<String>) {
        self.emit(JobEvent::error(message)).await;
    }
}

/// Read-only handle for streaming subscribers. Wraps the same
/// `Arc<JobInner>` the emitter holds.
pub struct JobHandle {
    inner: Arc<JobInner>,
}

impl JobHandle {
    pub fn kind(&self) -> &str {
        &self.inner.kind
    }
    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.started_at
    }

    /// Replay the event log from index 0, then await new events
    /// until the terminal `Done` line is yielded. Designed to be
    /// wrapped in `async_stream::stream!` for axum's
    /// `Body::from_stream`.
    ///
    /// Cancel-safe: dropping the iterator just stops streaming; the
    /// worker task keeps running and other subscribers are
    /// unaffected.
    pub async fn next_events(&self, cursor: &mut usize) -> Vec<JobEvent> {
        // Standard Notify pattern: register interest *before*
        // reading so we can't miss a notify_waiters() that fires
        // between read and await.
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);

            let (drained, finished) = {
                let v = self.inner.events.lock().await;
                let new = v[*cursor..].to_vec();
                *cursor = v.len();
                (new, self.inner.finished.load(Ordering::Acquire))
            };

            if !drained.is_empty() {
                return drained;
            }
            if finished {
                return Vec::new();
            }

            notified.await;
        }
    }

    pub fn is_finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }
}

/// Process-wide job table. Owned by the product's daemon state;
/// admin handlers reach it via the [`crate::HasJobs`] trait.
pub struct JobRegistry {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Arc<JobInner>>>,
    /// How long to retain a finished job's transcript before the GC
    /// loop reaps it. Tuned to "long enough for the CLI to stream
    /// the last event"; finished jobs hang around briefly so a slow
    /// subscriber connecting right after Done still gets the full
    /// transcript.
    retention: Duration,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(HashMap::new()),
            retention: Duration::from_secs(300),
        }
    }

    /// Create a new job, register it, and hand the worker an
    /// emitter. Returns `(id, started_at, emitter)` so the POST
    /// handler can respond before the worker has produced anything.
    pub async fn create(&self, kind: impl Into<String>) -> (String, DateTime<Utc>, JobEmitter) {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("j{:08x}", id_num);
        let now = Utc::now();
        let inner = JobInner::new(kind.into(), now);
        let emitter = JobEmitter {
            inner: Arc::clone(&inner),
        };
        self.jobs.lock().await.insert(id.clone(), inner);
        (id, now, emitter)
    }

    pub async fn get(&self, id: &str) -> Option<JobHandle> {
        self.jobs.lock().await.get(id).map(|inner| JobHandle {
            inner: Arc::clone(inner),
        })
    }

    /// Drop finished jobs older than `retention`. Cheap to call
    /// from a periodic tick or piggy-backed on every POST.
    pub async fn reap(&self) {
        let cutoff = Utc::now() - chrono::Duration::from_std(self.retention).unwrap_or_default();
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|_, inner| {
            !inner.finished.load(Ordering::Acquire) || inner.started_at >= cutoff
        });
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_then_stream_replays_history() {
        let reg = JobRegistry::new();
        let (id, _started, emitter) = reg.create("test.echo").await;
        emitter.info("hello").await;
        emitter.info("world").await;
        emitter.emit(JobEvent::done(0)).await;

        let handle = reg.get(&id).await.expect("job exists");
        let mut cursor = 0;
        let first = handle.next_events(&mut cursor).await;
        assert_eq!(first.len(), 3);
        // Replays in order: log, log, done.
        match &first[2] {
            JobEvent::Done { exit_code, .. } => assert_eq!(*exit_code, 0),
            _ => panic!("expected Done"),
        }
        // Already-finished: subsequent next_events returns
        // immediately with an empty Vec.
        let second = handle.next_events(&mut cursor).await;
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn streamer_unblocks_when_event_arrives() {
        let reg = JobRegistry::new();
        let (id, _started, emitter) = reg.create("test.slow").await;
        let handle = reg.get(&id).await.unwrap();

        let h_for_task = JobHandle {
            inner: Arc::clone(&handle.inner),
        };
        let stream_task = tokio::spawn(async move {
            let mut cursor = 0;
            let mut all = Vec::new();
            loop {
                let evs = h_for_task.next_events(&mut cursor).await;
                if evs.is_empty() {
                    break;
                }
                let terminal = evs.iter().any(|e| e.is_terminal());
                all.extend(evs);
                if terminal {
                    break;
                }
            }
            all
        });

        // Yield once so the streamer parks on Notify before we
        // emit.
        tokio::task::yield_now().await;
        emitter.info("step 1").await;
        emitter.info("step 2").await;
        emitter.emit(JobEvent::done(0)).await;

        let collected = stream_task.await.unwrap();
        assert_eq!(collected.len(), 3);
    }

    #[tokio::test]
    async fn reap_drops_old_finished_jobs() {
        let mut reg = JobRegistry::new();
        reg.retention = Duration::from_millis(0);
        let (id, _started, emitter) = reg.create("test.short").await;
        emitter.emit(JobEvent::done(0)).await;
        // Sleep one tick to push started_at into the past relative
        // to Utc::now() comparisons.
        tokio::time::sleep(Duration::from_millis(5)).await;
        reg.reap().await;
        assert!(reg.get(&id).await.is_none());
    }
}
