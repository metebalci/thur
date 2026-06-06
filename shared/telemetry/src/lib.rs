// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Telemetry — single instrumentation surface, two readers off the same
//! OpenTelemetry `MeterProvider`.
//!
//! Source of truth: the OTel SDK's metric registry, populated by the
//! instrument handles on [`Telemetry`]. Two output readers attach to
//! the same provider:
//!
//! 1. **PrometheusExporter** (always on): serves `/metrics` on the
//!    daemon's HTTP server in Prometheus text format. Same surface
//!    Prometheus / Grafana / VictoriaMetrics / Mimir / Cortex already
//!    scrape today.
//! 2. **PeriodicReader → OtlpExporter** (opt-in via
//!    `telemetry.otlp.enabled`): pushes the same instruments over OTLP
//!    on a configurable interval to a Collector or any OTLP-compatible
//!    backend (Datadog / Honeycomb / Grafana Cloud / New Relic /
//!    self-hosted Tempo / Loki for the future logs layer / …).
//!
//! Both readers walk the same in-memory state, so a counter incremented
//! once shows up in both surfaces — no manual sync, no double-counting.

#[cfg(feature = "http")]
pub mod http;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, MeterProvider};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

/// Fallback instrument-name prefix when `TelemetryConfig::instrument_prefix`
/// is unset. Matches the historical name space so legacy callers and
/// `Telemetry::noop()` keep emitting the same series they always did.
const DEFAULT_INSTRUMENT_PREFIX: &str = "thur";

/// OTLP transport selection. gRPC is the default in OTel deployments;
/// http/protobuf is the fallback for environments that can't egress
/// gRPC (proxies, restrictive corporate networks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// gRPC over HTTP/2, OTLP default. Endpoint typically `:4317`.
    Grpc,
    /// HTTP/1.1 + protobuf body. Endpoint typically `:4318`.
    HttpProtobuf,
}

/// OTLP push exporter configuration. When `None`, only the Prometheus
/// `/metrics` endpoint is wired.
#[derive(Debug, Clone)]
pub struct OtlpExporterConfig {
    /// Collector / SaaS endpoint (e.g. `http://localhost:4317`).
    pub endpoint: String,
    /// Wire protocol.
    pub protocol: OtlpProtocol,
    /// How often the periodic reader pushes the current snapshot.
    /// Smaller = more network, fresher data; larger = the opposite.
    /// 30s is a sensible default mirroring most managed backends.
    pub interval: Duration,
    /// Optional headers (e.g. `authorization: Bearer …` for SaaS).
    /// Empty for unauthenticated Collectors.
    pub headers: Vec<(String, String)>,
}

/// Full telemetry config block (mirrors the daemon's YAML).
#[derive(Debug, Clone, Default)]
pub struct TelemetryConfig {
    /// `service.name` resource attribute. Identifies the emitting
    /// product on shared OTLP backends and surfaces as a label on
    /// the Prometheus `target_info` series. Defaults to `thurvtl`
    /// when unset (so existing thurvtl call sites stay identical);
    /// thurvsad sets this to `thurvsa`. Each daemon sources
    /// this from `shared_naming::PRODUCT.name` at boot.
    pub service_name: Option<String>,
    /// `service.instance.id` resource attribute (per-host id when
    /// running multiple daemons sharing the same `service.name`).
    pub service_instance_id: Option<String>,
    /// OpenTelemetry / Prometheus instrument-name prefix. Every
    /// instrument is built as `<prefix>_<subsystem>_<name>` — so
    /// `thurvtl_pool_used_bytes` for thurvtld and
    /// `thurvsa_pool_used_bytes` for thurvsad (both sourced
    /// from `shared_naming::PRODUCT.metric_prefix`). Defaults to
    /// `thur` when unset so legacy callers / `Telemetry::noop()`
    /// keep the historical `thur_*` namespace.
    pub instrument_prefix: Option<String>,
    /// OTLP push reader. `None` ⇒ only the Prometheus reader is wired.
    pub otlp: Option<OtlpExporterConfig>,
}

/// In-process counters mirrored alongside the OTel instruments so the
/// `system monitor` job handler can read them back without scraping
/// `/metrics`. OTel's SDK is write-only from the producer side, so for
/// the windowed views the monitor verb computes (cloud ops/bytes/errors
/// per backend, pool backpressure waits/waiters, audit entries) we keep
/// a parallel set of atomics here.
///
/// Updated alongside the OTel calls in the [`Telemetry`] recorder
/// methods — one extra `fetch_add` per call site, paid only on the
/// recorder hot path (no contention because each backend gets its own
/// counter struct under an `Arc`).
#[derive(Default)]
pub struct LiveStats {
    /// Per-cloud-backend cumulative counters.
    storage: Mutex<HashMap<String, Arc<StorageCounters>>>,
    /// Per-pool-backend cumulative counters + an instantaneous waiter
    /// gauge.
    pool: Mutex<HashMap<String, Arc<PoolCounters>>>,
    /// Per-backend cumulative chunk dedup byte totals. The `scope`
    /// (local/global) dimension is folded away here — the monitor's
    /// dedup ratio is per-backend, so both scopes sum into one pair.
    chunk: Mutex<HashMap<String, Arc<ChunkCounters>>>,
    /// Audit entries appended since daemon start.
    pub audit_entries_total: AtomicU64,
}

#[derive(Default)]
pub struct StorageCounters {
    pub put_ops: AtomicU64,
    pub get_ops: AtomicU64,
    pub put_bytes: AtomicU64,
    pub get_bytes: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Default)]
pub struct PoolCounters {
    pub waits_total: AtomicU64,
    /// Signed because the inc/dec around the backpressure wait happens
    /// across multiple threads; a transient drop below zero from a
    /// reorder would be benign but i64 makes the invariant explicit.
    pub waiters_now: AtomicI64,
}

/// Per-backend chunk dedup byte totals. `logical` is every byte sealed
/// (pre-dedup); `unique` is only the bytes that actually grew the pool
/// (first-time-ever seals). `logical / unique` is the lifetime dedup
/// ratio. Both are append-only since daemon start — they never
/// decrement on eviction / delete, so this is a cumulative trend, not
/// a current-on-disk figure (that is the on-demand `system stats`
/// scan).
#[derive(Default)]
pub struct ChunkCounters {
    pub logical_bytes: AtomicU64,
    pub unique_bytes: AtomicU64,
}

/// Plain-data snapshot of [`LiveStats`] for emission in the monitor
/// JSON payload. All numeric; no atomics.
#[derive(Default, Clone, Debug)]
pub struct LiveStatsSnapshot {
    pub storage: HashMap<String, StorageSnapshot>,
    pub pool: HashMap<String, PoolSnapshot>,
    pub chunk: HashMap<String, ChunkSnapshot>,
    pub audit_entries_total: u64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct StorageSnapshot {
    pub put_ops: u64,
    pub get_ops: u64,
    pub put_bytes: u64,
    pub get_bytes: u64,
    pub errors: u64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PoolSnapshot {
    pub waits_total: u64,
    pub waiters_now: i64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ChunkSnapshot {
    pub logical_bytes: u64,
    pub unique_bytes: u64,
}

impl LiveStats {
    fn storage_for(&self, backend: &str) -> Arc<StorageCounters> {
        let mut map = self.storage.lock().expect("LiveStats cloud mutex poisoned");
        Arc::clone(map.entry(backend.to_string()).or_default())
    }

    fn pool_for(&self, backend: &str) -> Arc<PoolCounters> {
        let mut map = self.pool.lock().expect("LiveStats pool mutex poisoned");
        Arc::clone(map.entry(backend.to_string()).or_default())
    }

    fn chunk_for(&self, backend: &str) -> Arc<ChunkCounters> {
        let mut map = self.chunk.lock().expect("LiveStats chunk mutex poisoned");
        Arc::clone(map.entry(backend.to_string()).or_default())
    }

    /// Mirror of [`Telemetry::storage_record_request`]. `op` is one of
    /// `put`/`get`/`head`/`delete`; bytes are credited only on the
    /// successful put/get paths so `put_bytes` + `get_bytes` track the
    /// `outcome="ok"` slice of `storage_bytes_total`.
    pub fn record_storage_op(&self, backend: &str, op: &str, outcome: &str, bytes: u64) {
        let c = self.storage_for(backend);
        if outcome == "error" {
            c.errors.fetch_add(1, Ordering::Relaxed);
        }
        match op {
            "put" => {
                c.put_ops.fetch_add(1, Ordering::Relaxed);
                if outcome == "ok" {
                    c.put_bytes.fetch_add(bytes, Ordering::Relaxed);
                }
            }
            "get" => {
                c.get_ops.fetch_add(1, Ordering::Relaxed);
                if outcome == "ok" {
                    c.get_bytes.fetch_add(bytes, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }

    /// Mirror of [`Telemetry::pool_record_backpressure_wait`] — one
    /// increment per completed wait, success or timeout.
    pub fn record_pool_wait(&self, backend: &str) {
        self.pool_for(backend)
            .waits_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Mirror of [`Telemetry::chunk_add_logical_bytes`]. Scope is
    /// folded away — per-backend logical bytes is what the monitor's
    /// dedup ratio needs.
    pub fn record_chunk_logical_bytes(&self, backend: &str, bytes: u64) {
        self.chunk_for(backend)
            .logical_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Mirror of [`Telemetry::chunk_add_unique_bytes`].
    pub fn record_chunk_unique_bytes(&self, backend: &str, bytes: u64) {
        self.chunk_for(backend)
            .unique_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Called by `PoolBudget::try_reserve` when it enters the slow
    /// path. Matched by exactly one [`LiveStats::dec_waiters`] on
    /// every exit (success, timeout, error).
    pub fn inc_waiters(&self, backend: &str) {
        self.pool_for(backend)
            .waiters_now
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_waiters(&self, backend: &str) {
        self.pool_for(backend)
            .waiters_now
            .fetch_sub(1, Ordering::Relaxed);
    }

    /// Mirror of [`Telemetry::audit_inc_entry`].
    pub fn record_audit_entry(&self) {
        self.audit_entries_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot all counters for the monitor JSON payload. Holds the
    /// two backend-keyed mutexes briefly to clone the per-backend
    /// `Arc`s, then drops the locks and reads atomics outside.
    pub fn snapshot(&self) -> LiveStatsSnapshot {
        let storage_pairs: Vec<(String, Arc<StorageCounters>)> = self
            .storage
            .lock()
            .expect("LiveStats storage mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        let pool_pairs: Vec<(String, Arc<PoolCounters>)> = self
            .pool
            .lock()
            .expect("LiveStats pool mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        let chunk_pairs: Vec<(String, Arc<ChunkCounters>)> = self
            .chunk
            .lock()
            .expect("LiveStats chunk mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        LiveStatsSnapshot {
            storage: storage_pairs
                .into_iter()
                .map(|(k, v)| (k, v.snapshot()))
                .collect(),
            pool: pool_pairs
                .into_iter()
                .map(|(k, v)| (k, v.snapshot()))
                .collect(),
            chunk: chunk_pairs
                .into_iter()
                .map(|(k, v)| (k, v.snapshot()))
                .collect(),
            audit_entries_total: self.audit_entries_total.load(Ordering::Relaxed),
        }
    }
}

impl StorageCounters {
    fn snapshot(&self) -> StorageSnapshot {
        StorageSnapshot {
            put_ops: self.put_ops.load(Ordering::Relaxed),
            get_ops: self.get_ops.load(Ordering::Relaxed),
            put_bytes: self.put_bytes.load(Ordering::Relaxed),
            get_bytes: self.get_bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

impl PoolCounters {
    fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            waits_total: self.waits_total.load(Ordering::Relaxed),
            waiters_now: self.waiters_now.load(Ordering::Relaxed),
        }
    }
}

impl ChunkCounters {
    fn snapshot(&self) -> ChunkSnapshot {
        ChunkSnapshot {
            logical_bytes: self.logical_bytes.load(Ordering::Relaxed),
            unique_bytes: self.unique_bytes.load(Ordering::Relaxed),
        }
    }
}

/// All Thur instruments, grouped by subsystem. Held in an `Arc` so
/// the daemon can clone cheaply into worker tasks.
///
/// The naming follows the OTel/Prometheus convention
/// `<prefix>_<subsystem>_<name>_<unit>`, where `<prefix>` is sourced
/// from `TelemetryConfig::instrument_prefix` (per-product —
/// `thurvtl_*` for thurvtld, `thurvsa_*` for thurvsad).
/// Attributes (OTel) become labels (Prometheus); both readers emit
/// the same dimensional set.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<TelemetryInner>,
}

#[allow(dead_code)] // some instruments are wired incrementally
struct TelemetryInner {
    provider: SdkMeterProvider,
    prom_registry: prometheus::Registry,

    // ── pool (per-backend disk-cache budget) ──
    pool_used_bytes: Gauge<u64>,
    pool_cap_bytes: Gauge<u64>,
    pool_evictions_total: Counter<u64>,
    pool_backpressure_waits_total: Counter<u64>,
    pool_backpressure_wait_seconds: Histogram<f64>,

    // ── cache (VSA per-volume page cache) ──
    cache_evictions_total: Counter<u64>,
    /// Distribution of `now - evicted_at` for cache misses whose chunk
    /// had been recently evicted from this backend's pool. Mass in the
    /// low buckets → cache undersized by that window; mass past 256 s
    /// → organic miss, bigger cache won't help. Bucket boundaries are
    /// explicit (log-uniform) rather than OTel's defaults.
    cache_miss_after_eviction_seconds: Histogram<f64>,

    // ── SCSI EXTENDED COPY (VSA VAAI XCOPY) ──
    /// One increment per EXTENDED COPY command, labeled
    /// `outcome ∈ {success, reject, error}`.
    scsi_xcopy_total: Counter<u64>,
    /// Host-visible bytes copied by EXTENDED COPY, labeled
    /// `path ∈ {fast, slow}`. Fast = same-volume page-aligned
    /// chunk-hash clone; slow = bytes-copy (cross-volume,
    /// unaligned, or overlapping ranges).
    scsi_xcopy_bytes_total: Counter<u64>,
    /// One increment per executed segment descriptor, labeled
    /// `path ∈ {fast, slow}`. Lets operators see how often the
    /// fast path is actually taken vs the bytes-copy fallback.
    scsi_xcopy_segments_total: Counter<u64>,

    // ── cloud (per-backend op latency / bytes / errors) ──
    storage_requests_total: Counter<u64>,
    storage_request_seconds: Histogram<f64>,
    storage_bytes_total: Counter<u64>,
    storage_retries_total: Counter<u64>,
    storage_permanent_errors_total: Counter<u64>,

    // ── chunk lifecycle ──
    chunk_seals_total: Counter<u64>,
    chunk_dedup_hits_total: Counter<u64>,
    chunk_logical_bytes_total: Counter<u64>,
    chunk_unique_bytes_total: Counter<u64>,
    chunk_bytes_uploaded_total: Counter<u64>,
    chunk_storage_head_probes_total: Counter<u64>,
    chunk_storage_head_hits_total: Counter<u64>,
    chunk_storage_cache_hits_total: Counter<u64>,
    chunk_storage_cache_inflight_coalesced_total: Counter<u64>,
    chunk_storage_cache_warmup_seeded_total: Counter<u64>,
    /// Pages/chunks left `LocalOnly` because the async upload worker
    /// could not resolve a backend or owning entity. Labeled
    /// `reason ∈ {backend_unknown, entity_unknown}`. A non-zero rate
    /// means uploads are silently stranded until the next write or the
    /// daemon-restart recovery scan re-enqueues them.
    chunk_upload_stranded_total: Counter<u64>,
    /// Chunks (VTL) / pages (VSA) sealed into the local pool but not yet
    /// confirmed uploaded to the backend — the current upload backlog.
    /// Daemon-wide, no attributes. The owner pushes the absolute depth on
    /// every change (mirrors `iscsi_sessions_active` / `prefetch_queue_depth`).
    upload_queue_depth: Gauge<i64>,

    // ── iSCSI ──
    iscsi_sessions_active: Gauge<i64>,
    iscsi_commands_total: Counter<u64>,
    iscsi_command_seconds: Histogram<f64>,
    iscsi_data_in_bytes_total: Counter<u64>,
    iscsi_data_out_bytes_total: Counter<u64>,

    // ── tape (per-cartridge buffers) ──
    tape_write_buffer_used_bytes: Gauge<u64>,
    tape_read_buffer_used_bytes: Gauge<u64>,

    // ── prefetch ──
    prefetch_queue_depth: Gauge<i64>,
    prefetch_hits_total: Counter<u64>,
    prefetch_misses_total: Counter<u64>,

    // ── audit ──
    audit_entries_total: Counter<u64>,
    audit_chain_resets_total: Counter<u64>,
    /// Channel-full drops on the producer side of [`AuditChannel`].
    /// Each increment is one audit entry the writer task never saw.
    audit_queue_drops_total: Counter<u64>,

    // ── orphan-upload recovery (boot-time scan for sealed-but-
    //    not-uploaded chunks left by a mid-PUT crash) ──
    orphan_scan_chunks_found_total: Counter<u64>,
    orphan_scan_duration_seconds: Histogram<f64>,

    // ── alerting (first-party Email + webhook sinks) ──
    /// One increment per alert delivery attempt. Outcome ∈
    /// `success` / `failure` / `suppressed`. `suppressed` is the
    /// rate-limiter saying "we tried to fire but the dedup window
    /// is open"; `failure` is sink-side (SMTP timeout, webhook
    /// non-2xx, render error). Pair with the audit log for a full
    /// picture: every emit (suppressed or not) shows up here, but
    /// only successful emits show up downstream.
    alerts_fired_total: Counter<u64>,

    // ── daemon info ──
    daemon_start_time_seconds: Gauge<i64>,

    // ── monitor live-stats sidecar (in-process readable) ──
    live_stats: Arc<LiveStats>,
}

impl Telemetry {
    /// Build a `Telemetry` with the Prometheus reader always attached
    /// and the OTLP reader attached iff `cfg.otlp` is `Some`.
    pub fn new(cfg: &TelemetryConfig) -> Result<Self, TelemetryError> {
        let prom_registry = prometheus::Registry::new();
        let prom_exporter = opentelemetry_prometheus::exporter()
            .with_registry(prom_registry.clone())
            .build()
            .map_err(TelemetryError::Prometheus)?;

        let mut builder = SdkMeterProvider::builder()
            .with_resource(build_resource(cfg))
            .with_reader(prom_exporter);

        if let Some(ref otlp) = cfg.otlp {
            let reader = build_otlp_reader(otlp)?;
            builder = builder.with_reader(reader);
        }

        let provider = builder.build();
        // OTel meter name = instrumentation-library scope. Doesn't
        // appear in Prometheus instrument names; product distinction
        // is via the `service.name` resource attribute below. Keep it
        // a constant so the call stays `&'static str`.
        let meter = provider.meter("thur");
        let prefix = cfg
            .instrument_prefix
            .as_deref()
            .unwrap_or(DEFAULT_INSTRUMENT_PREFIX);
        let inner = TelemetryInner::build(provider, prom_registry, &meter, prefix);

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Build a no-op `Telemetry` for tests / `--test` smoke runs that
    /// don't need a live registry. All instruments still record into
    /// the SDK; nothing is exported.
    pub fn noop() -> Self {
        Self::new(&TelemetryConfig::default()).expect("noop telemetry never fails to construct")
    }

    /// Render the current Prometheus snapshot in text format.
    /// Wired from the daemon's `GET /metrics` handler.
    pub fn export_prometheus(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.inner.prom_registry.gather();
        let mut buffer = Vec::new();
        if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
            tracing::warn!("metrics: prometheus encode failed: {e}");
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_else(|e| {
            tracing::warn!("metrics: prometheus utf8 conversion failed: {e}");
            String::new()
        })
    }

    /// Flush + shut down the OTel SDK on daemon stop. Idempotent.
    pub fn shutdown(&self) {
        if let Err(e) = self.inner.provider.shutdown() {
            tracing::warn!("metrics: SDK shutdown failed: {e}");
        }
    }

    // ── subsystem accessors (kept narrow on purpose: callers go
    //    through these typed helpers so we control the attribute
    //    schema in one place) ──

    /// pool.used_bytes / pool.cap_bytes for one backend.
    pub fn pool_set_used(&self, backend: &str, used: u64) {
        self.inner
            .pool_used_bytes
            .record(used, &[KeyValue::new("backend", backend.to_string())]);
    }

    pub fn pool_set_cap(&self, backend: &str, cap: u64) {
        self.inner
            .pool_cap_bytes
            .record(cap, &[KeyValue::new("backend", backend.to_string())]);
    }

    pub fn pool_inc_eviction(&self, backend: &str) {
        self.inner
            .pool_evictions_total
            .add(1, &[KeyValue::new("backend", backend.to_string())]);
    }

    pub fn pool_record_backpressure_wait(&self, backend: &str, seconds: f64) {
        let kv = [KeyValue::new("backend", backend.to_string())];
        self.inner.pool_backpressure_waits_total.add(1, &kv);
        self.inner
            .pool_backpressure_wait_seconds
            .record(seconds, &kv);
        self.inner.live_stats.record_pool_wait(backend);
    }

    /// VSA per-volume page-cache eviction. `outcome` is `clean` (LRU
    /// page dropped without a cloud upload) or `dirty` (LRU page
    /// flushed through `VolumeWriter::write_page` before drop —
    /// pathological-pressure tail; if this counter grows non-trivially
    /// relative to host writes the cache budget is undersized).
    pub fn cache_inc_eviction(&self, volume: &str, outcome: &str) {
        self.inner.cache_evictions_total.add(
            1,
            &[
                KeyValue::new("volume", volume.to_string()),
                KeyValue::new("outcome", outcome.to_string()),
            ],
        );
    }

    /// Record a cache miss whose chunk had been recently evicted from
    /// `backend`'s pool. `seconds` is `now - evicted_at` from the
    /// ghost list. A miss with no ghost-list hit is silent — it
    /// either predates the ring or is a chunk we never cached at all.
    pub fn cache_record_miss_after_eviction(&self, backend: &str, seconds: f64) {
        self.inner
            .cache_miss_after_eviction_seconds
            .record(seconds, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// scsi_xcopy.* — one increment per EXTENDED COPY command.
    /// `outcome` is `success` (returned GOOD), `reject` (parameter
    /// list failed validation; nothing copied) or `error` (a segment
    /// failed mid-execution; partial commit possible).
    pub fn scsi_xcopy_inc(&self, outcome: &str) {
        self.inner
            .scsi_xcopy_total
            .add(1, &[KeyValue::new("outcome", outcome.to_string())]);
    }

    /// scsi_xcopy.* — host-visible bytes copied. `path` is `fast`
    /// (page-index clone) or `slow` (bytes copy).
    pub fn scsi_xcopy_add_bytes(&self, path: &str, bytes: u64) {
        self.inner
            .scsi_xcopy_bytes_total
            .add(bytes, &[KeyValue::new("path", path.to_string())]);
    }

    /// scsi_xcopy.* — segment count by path. Paired with
    /// `scsi_xcopy_add_bytes`; the bytes/segment quotient lets
    /// operators see the average segment size ESXi issues.
    pub fn scsi_xcopy_inc_segment(&self, path: &str) {
        self.inner
            .scsi_xcopy_segments_total
            .add(1, &[KeyValue::new("path", path.to_string())]);
    }

    /// cloud.* — `op` is one of `put`/`get`/`head`/`delete`,
    /// `outcome` is `ok`/`error`.
    pub fn storage_record_request(
        &self,
        backend: &str,
        op: &str,
        outcome: &str,
        bytes: u64,
        seconds: f64,
    ) {
        let kv = [
            KeyValue::new("backend", backend.to_string()),
            KeyValue::new("op", op.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ];
        self.inner.storage_requests_total.add(1, &kv);
        self.inner.storage_request_seconds.record(seconds, &kv);
        if bytes > 0 {
            self.inner.storage_bytes_total.add(bytes, &kv);
        }
        self.inner
            .live_stats
            .record_storage_op(backend, op, outcome, bytes);
    }

    pub fn storage_inc_retry(&self, backend: &str, class: &str) {
        self.inner.storage_retries_total.add(
            1,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("class", class.to_string()),
            ],
        );
    }

    pub fn storage_inc_permanent_error(&self, backend: &str, class: &str) {
        self.inner.storage_permanent_errors_total.add(
            1,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("class", class.to_string()),
            ],
        );
    }

    /// chunk lifecycle. `scope` is `local`/`global` (DedupScope).
    pub fn chunk_inc_seal(&self, backend: &str, scope: &str) {
        self.inner.chunk_seals_total.add(
            1,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("scope", scope.to_string()),
            ],
        );
    }

    pub fn chunk_inc_dedup_hit(&self, backend: &str, scope: &str) {
        self.inner.chunk_dedup_hits_total.add(
            1,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("scope", scope.to_string()),
            ],
        );
    }

    pub fn chunk_add_uploaded_bytes(&self, backend: &str, bytes: u64) {
        self.inner
            .chunk_bytes_uploaded_total
            .add(bytes, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// Logical (pre-dedup) bytes sealed: every seal contributes its
    /// chunk size, regardless of whether the local pool already had
    /// the hash. Paired with `chunk_unique_bytes_total` to derive the
    /// dedup ratio (logical / unique).
    pub fn chunk_add_logical_bytes(&self, backend: &str, scope: &str, bytes: u64) {
        self.inner.chunk_logical_bytes_total.add(
            bytes,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("scope", scope.to_string()),
            ],
        );
        self.inner
            .live_stats
            .record_chunk_logical_bytes(backend, bytes);
    }

    /// Unique (post-dedup) bytes sealed: first-time-ever seals only
    /// (i.e. seals where the local pool didn't already have the hash).
    /// This is the bytes that actually consumed pool space.
    pub fn chunk_add_unique_bytes(&self, backend: &str, scope: &str, bytes: u64) {
        self.inner.chunk_unique_bytes_total.add(
            bytes,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("scope", scope.to_string()),
            ],
        );
        self.inner
            .live_stats
            .record_chunk_unique_bytes(backend, bytes);
    }

    /// Cloud-side HEAD-before-PUT probes (Global scope only — Local
    /// scope namespaces by barcode so the HEAD is guaranteed to miss
    /// and is skipped). Pair with `chunk_storage_head_hits_total` for
    /// the upload-skip rate.
    pub fn chunk_inc_storage_head_probe(&self, backend: &str) {
        self.inner
            .chunk_storage_head_probes_total
            .add(1, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// Cloud-side HEAD probes that found the object already present —
    /// upload was skipped. The complement of (probes - hits) is the
    /// PUTs the daemon actually issued.
    pub fn chunk_inc_storage_head_hit(&self, backend: &str) {
        self.inner
            .chunk_storage_head_hits_total
            .add(1, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// Meta-cache served the lookup from `Probed` or `Uploaded` state
    /// — no backend round-trip. Pair with the `head_probes` /
    /// `head_hits` counters above for a full breakdown of how the
    /// daemon's "is X already in cloud?" decisions get answered.
    pub fn chunk_inc_storage_cache_hit(&self, backend: &str) {
        self.inner
            .chunk_storage_cache_hits_total
            .add(1, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// Meta-cache `InFlight` coalesce — caller joined an in-flight
    /// singleflight future instead of issuing its own PUT. This is
    /// the GCS-mkfs zero-page-burst collapse counter; should track
    /// the number of duplicate writes that the cache absorbed.
    pub fn chunk_inc_storage_cache_inflight_coalesced(&self, backend: &str) {
        self.inner
            .chunk_storage_cache_inflight_coalesced_total
            .add(1, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// Meta-cache warmup populated this many `Probed` entries from a
    /// LIST at boot / first registry insertion. One per key inserted
    /// (not per LIST call) so an operator can see cache fill rate.
    pub fn chunk_add_storage_cache_warmup_seeded(&self, backend: &str, n: u64) {
        self.inner
            .chunk_storage_cache_warmup_seeded_total
            .add(n, &[KeyValue::new("backend", backend.to_string())]);
    }

    /// One increment each time the async upload worker strands a
    /// page/chunk `LocalOnly` because it could not resolve a backend
    /// (`reason="backend_unknown"`) or the owning entity
    /// (`reason="entity_unknown"`).
    pub fn chunk_inc_upload_stranded(&self, backend: &str, reason: &str) {
        self.inner.chunk_upload_stranded_total.add(
            1,
            &[
                KeyValue::new("backend", backend.to_string()),
                KeyValue::new("reason", reason.to_string()),
            ],
        );
    }

    /// Current upload backlog (UpDownCounter via Gauge<i64>). The owner
    /// pushes the absolute daemon-wide depth on every change.
    pub fn upload_set_queue_depth(&self, n: i64) {
        self.inner.upload_queue_depth.record(n, &[]);
    }

    /// iSCSI session counter (UpDownCounter via Gauge<i64>).
    pub fn iscsi_set_sessions_active(&self, n: i64) {
        self.inner.iscsi_sessions_active.record(n, &[]);
    }

    pub fn iscsi_record_command(&self, opcode: &str, outcome: &str, seconds: f64) {
        let kv = [
            KeyValue::new("opcode", opcode.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ];
        self.inner.iscsi_commands_total.add(1, &kv);
        self.inner.iscsi_command_seconds.record(seconds, &kv);
    }

    pub fn iscsi_add_data_in(&self, bytes: u64) {
        self.inner.iscsi_data_in_bytes_total.add(bytes, &[]);
    }

    pub fn iscsi_add_data_out(&self, bytes: u64) {
        self.inner.iscsi_data_out_bytes_total.add(bytes, &[]);
    }

    /// Per-cartridge memory-buffer fill (write side).
    pub fn tape_set_write_buffer(&self, cartridge: &str, bytes: u64) {
        self.inner
            .tape_write_buffer_used_bytes
            .record(bytes, &[KeyValue::new("cartridge", cartridge.to_string())]);
    }

    pub fn tape_set_read_buffer(&self, cartridge: &str, bytes: u64) {
        self.inner
            .tape_read_buffer_used_bytes
            .record(bytes, &[KeyValue::new("cartridge", cartridge.to_string())]);
    }

    pub fn prefetch_set_queue_depth(&self, n: i64) {
        self.inner.prefetch_queue_depth.record(n, &[]);
    }

    pub fn prefetch_inc_hit(&self) {
        self.inner.prefetch_hits_total.add(1, &[]);
    }

    pub fn prefetch_inc_miss(&self) {
        self.inner.prefetch_misses_total.add(1, &[]);
    }

    pub fn audit_inc_entry(&self, kind: &str) {
        self.inner
            .audit_entries_total
            .add(1, &[KeyValue::new("kind", kind.to_string())]);
        self.inner.live_stats.record_audit_entry();
    }

    /// Borrow the in-process [`LiveStats`] sidecar. Cloned into the
    /// `system monitor` job handler at daemon boot so the handler can
    /// snapshot counters per tick without going through `/metrics`.
    pub fn live_stats(&self) -> Arc<LiveStats> {
        Arc::clone(&self.inner.live_stats)
    }

    pub fn audit_inc_chain_reset(&self) {
        self.inner.audit_chain_resets_total.add(1, &[]);
    }

    pub fn audit_inc_queue_drop(&self) {
        self.inner.audit_queue_drops_total.add(1, &[]);
    }

    /// One first-party alert delivery attempt. `outcome` is
    /// `success` / `failure` / `suppressed` (see [`TelemetryInner::alerts_fired_total`]).
    pub fn alerts_record(&self, class: &str, severity: &str, sink: &str, outcome: &str) {
        self.inner.alerts_fired_total.add(
            1,
            &[
                KeyValue::new("class", class.to_string()),
                KeyValue::new("severity", severity.to_string()),
                KeyValue::new("sink", sink.to_string()),
                KeyValue::new("outcome", outcome.to_string()),
            ],
        );
    }

    /// One observation of the boot-time orphan-upload scan: how many
    /// sealed-but-not-uploaded chunks it found and how long the pass
    /// took. `chunks_found` is added to the lifetime counter; an empty
    /// scan still records the duration so dashboards see the heartbeat.
    pub fn orphan_scan_record(&self, chunks_found: u64, duration_seconds: f64) {
        if chunks_found > 0 {
            self.inner
                .orphan_scan_chunks_found_total
                .add(chunks_found, &[]);
        }
        self.inner
            .orphan_scan_duration_seconds
            .record(duration_seconds, &[]);
    }

    pub fn daemon_set_start_time(&self, unix_seconds: i64) {
        self.inner
            .daemon_start_time_seconds
            .record(unix_seconds, &[]);
    }
}

impl TelemetryInner {
    fn build(
        provider: SdkMeterProvider,
        prom_registry: prometheus::Registry,
        meter: &Meter,
        prefix: &str,
    ) -> Self {
        // Each instrument's full name is `<prefix>_<subsystem>_<name>`.
        // Building once-per-instrument with `format!` keeps the call
        // sites readable; this only runs at daemon boot.
        let name = |suffix: &str| format!("{prefix}_{suffix}");
        Self {
            // pool
            pool_used_bytes: meter
                .u64_gauge(name("pool_used_bytes"))
                .with_description("Per-backend disk-cache pool bytes in use")
                .build(),
            pool_cap_bytes: meter
                .u64_gauge(name("pool_cap_bytes"))
                .with_description("Per-backend disk-cache pool cap (hard ceiling)")
                .build(),
            pool_evictions_total: meter
                .u64_counter(name("pool_evictions"))
                .with_description("Per-backend pool eviction events")
                .build(),
            pool_backpressure_waits_total: meter
                .u64_counter(name("pool_backpressure_waits"))
                .with_description("Times chunk-seal blocked on the pool budget")
                .build(),
            pool_backpressure_wait_seconds: meter
                .f64_histogram(name("pool_backpressure_wait"))
                .with_description("Wall-clock waited on the pool budget")
                .with_unit("s")
                .build(),

            // cache (VSA page cache)
            cache_evictions_total: meter
                .u64_counter(name("cache_evictions"))
                .with_description(
                    "VSA per-volume page-cache evictions, labeled clean vs dirty (dirty = required a cloud flush)",
                )
                .build(),
            cache_miss_after_eviction_seconds: meter
                .f64_histogram(name("cache_miss_after_eviction"))
                .with_description(
                    "Age (now - evicted_at) of cache misses whose chunk was recently evicted from this backend's pool. Drives operator sizing of disk_cache.size_gb.",
                )
                .with_unit("s")
                .with_boundaries(vec![
                    1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0,
                ])
                .build(),

            // SCSI EXTENDED COPY (VAAI XCOPY)
            scsi_xcopy_total: meter
                .u64_counter(name("scsi_xcopy"))
                .with_description("EXTENDED COPY commands by outcome (success/reject/error)")
                .build(),
            scsi_xcopy_bytes_total: meter
                .u64_counter(name("scsi_xcopy_copied"))
                .with_description("EXTENDED COPY host-visible bytes by path (fast/slow)")
                .with_unit("By")
                .build(),
            scsi_xcopy_segments_total: meter
                .u64_counter(name("scsi_xcopy_segments"))
                .with_description("EXTENDED COPY segments executed by path (fast/slow)")
                .build(),

            // cloud
            storage_requests_total: meter
                .u64_counter(name("storage_requests"))
                .with_description("Cloud requests by backend/op/outcome")
                .build(),
            storage_request_seconds: meter
                .f64_histogram(name("storage_request"))
                .with_description("Cloud request latency by backend/op/outcome")
                .with_unit("s")
                .build(),
            storage_bytes_total: meter
                .u64_counter(name("storage_transferred"))
                .with_description("Cloud bytes transferred by backend/op/outcome")
                .with_unit("By")
                .build(),
            storage_retries_total: meter
                .u64_counter(name("storage_retries"))
                .with_description("Cloud retry attempts by backend/error class")
                .build(),
            storage_permanent_errors_total: meter
                .u64_counter(name("storage_permanent_errors"))
                .with_description("Permanent cloud errors that bypassed retry")
                .build(),

            // chunk
            chunk_seals_total: meter
                .u64_counter(name("chunk_seals"))
                .with_description("Chunks sealed into the pool")
                .build(),
            chunk_dedup_hits_total: meter
                .u64_counter(name("chunk_dedup_hits"))
                .with_description("Chunks skipped because the hash already existed")
                .build(),
            chunk_logical_bytes_total: meter
                .u64_counter(name("chunk_logical"))
                .with_description("Logical (pre-dedup) bytes sealed across all chunks")
                .with_unit("By")
                .build(),
            chunk_unique_bytes_total: meter
                .u64_counter(name("chunk_unique"))
                .with_description(
                    "Unique (post-dedup) bytes sealed — bytes that actually grew the pool",
                )
                .with_unit("By")
                .build(),
            chunk_bytes_uploaded_total: meter
                .u64_counter(name("chunk_uploaded"))
                .with_description("Chunk bytes successfully PUT to cloud")
                .with_unit("By")
                .build(),
            chunk_storage_head_probes_total: meter
                .u64_counter(name("chunk_storage_head_probes"))
                .with_description("Cloud-side HEAD-before-PUT probes (Global scope)")
                .build(),
            chunk_storage_head_hits_total: meter
                .u64_counter(name("chunk_storage_head_hits"))
                .with_description("Cloud HEAD probes that found the object already present")
                .build(),
            chunk_storage_cache_hits_total: meter
                .u64_counter(name("chunk_storage_cache_hits"))
                .with_description(
                    "Meta-cache hits — lookup served from Probed/Uploaded entry without backend round-trip",
                )
                .build(),
            chunk_storage_cache_inflight_coalesced_total: meter
                .u64_counter(name("chunk_storage_cache_inflight_coalesced"))
                .with_description(
                    "Concurrent uploads that joined an in-flight singleflight instead of issuing a duplicate PUT",
                )
                .build(),
            chunk_storage_cache_warmup_seeded_total: meter
                .u64_counter(name("chunk_storage_cache_warmup_seeded"))
                .with_description(
                    "Cache entries seeded from a LIST at boot / first registry insertion",
                )
                .build(),
            chunk_upload_stranded_total: meter
                .u64_counter(name("chunk_upload_stranded"))
                .with_description(
                    "Pages/chunks left LocalOnly because the upload worker could not resolve a backend or owning entity",
                )
                .build(),
            upload_queue_depth: meter
                .i64_gauge(name("upload_queue_depth"))
                .with_description("Chunks/pages sealed locally but not yet uploaded to the backend")
                .build(),

            // iSCSI
            iscsi_sessions_active: meter
                .i64_gauge(name("iscsi_sessions_active"))
                .with_description("iSCSI sessions currently logged in")
                .build(),
            iscsi_commands_total: meter
                .u64_counter(name("iscsi_commands"))
                .with_description("iSCSI SCSI commands by opcode/outcome")
                .build(),
            iscsi_command_seconds: meter
                .f64_histogram(name("iscsi_command"))
                .with_description("iSCSI SCSI command service time")
                .with_unit("s")
                .build(),
            iscsi_data_in_bytes_total: meter
                .u64_counter(name("iscsi_data_in"))
                .with_description("Bytes received from iSCSI initiators (host→target)")
                .with_unit("By")
                .build(),
            iscsi_data_out_bytes_total: meter
                .u64_counter(name("iscsi_data_out"))
                .with_description("Bytes sent to iSCSI initiators (target→host)")
                .with_unit("By")
                .build(),

            // tape
            tape_write_buffer_used_bytes: meter
                .u64_gauge(name("tape_write_buffer_used"))
                .with_description("Per-cartridge write-staging buffer occupancy")
                .with_unit("By")
                .build(),
            tape_read_buffer_used_bytes: meter
                .u64_gauge(name("tape_read_buffer_used"))
                .with_description("Per-cartridge read-prefetch buffer occupancy")
                .with_unit("By")
                .build(),

            // prefetch
            prefetch_queue_depth: meter
                .i64_gauge(name("prefetch_queue_depth"))
                .with_description("Outstanding prefetch tasks")
                .build(),
            prefetch_hits_total: meter
                .u64_counter(name("prefetch_hits"))
                .with_description("Reads served from a prefetched chunk")
                .build(),
            prefetch_misses_total: meter
                .u64_counter(name("prefetch_misses"))
                .with_description("Reads that found nothing prefetched and waited on cloud")
                .build(),

            // audit
            audit_entries_total: meter
                .u64_counter(name("audit_entries"))
                .with_description("Audit log entries appended, by event kind")
                .build(),
            audit_chain_resets_total: meter
                .u64_counter(name("audit_chain_resets"))
                .with_description("Operator-acknowledged audit chain breaks")
                .build(),
            audit_queue_drops_total: meter
                .u64_counter(name("audit_queue_drops"))
                .with_description("Audit entries dropped because the writer-task mpsc was full")
                .build(),

            // orphan-upload recovery
            orphan_scan_chunks_found_total: meter
                .u64_counter(name("orphan_scan_chunks_found"))
                .with_description(
                    "Sealed-but-not-uploaded chunks discovered by the boot-time orphan scan",
                )
                .build(),
            orphan_scan_duration_seconds: meter
                .f64_histogram(name("orphan_scan_duration"))
                .with_description("Wall-clock the boot-time orphan-upload scan took")
                .with_unit("s")
                .build(),

            // alerting
            alerts_fired_total: meter
                .u64_counter(name("alerts_fired"))
                .with_description(
                    "First-party alerts attempted, by class/severity/sink/outcome",
                )
                .build(),

            // daemon info
            daemon_start_time_seconds: meter
                .i64_gauge(name("daemon_start_time"))
                .with_description("Daemon start time as Unix epoch seconds")
                .with_unit("s")
                .build(),

            live_stats: Arc::new(LiveStats::default()),

            provider,
            prom_registry,
        }
    }
}

fn build_resource(cfg: &TelemetryConfig) -> Resource {
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_VERSION,
    };
    let service_name = cfg.service_name.clone().unwrap_or_else(|| "thurvtl".into());
    let mut builder = Resource::builder()
        .with_attribute(KeyValue::new(SERVICE_NAME, service_name))
        .with_attribute(KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")));
    if let Some(ref id) = cfg.service_instance_id {
        builder = builder.with_attribute(KeyValue::new(SERVICE_INSTANCE_ID, id.clone()));
    }
    builder.build()
}

fn build_otlp_reader(
    cfg: &OtlpExporterConfig,
) -> Result<PeriodicReader<opentelemetry_otlp::MetricExporter>, TelemetryError> {
    use opentelemetry_otlp::{
        MetricExporterBuilder, WithExportConfig, WithHttpConfig, WithTonicConfig,
    };

    let exporter = match cfg.protocol {
        OtlpProtocol::Grpc => {
            let mut metadata = tonic::metadata::MetadataMap::new();
            for (k, v) in &cfg.headers {
                let key: tonic::metadata::MetadataKey<_> = k.parse().map_err(|e| {
                    TelemetryError::OtlpHeader(format!("invalid header key '{k}': {e}"))
                })?;
                let val: tonic::metadata::MetadataValue<_> = v.parse().map_err(|e| {
                    TelemetryError::OtlpHeader(format!("invalid header value for '{k}': {e}"))
                })?;
                metadata.insert(key, val);
            }
            MetricExporterBuilder::new()
                .with_tonic()
                .with_endpoint(cfg.endpoint.clone())
                .with_metadata(metadata)
                .build()
                .map_err(TelemetryError::OtlpBuild)?
        }
        OtlpProtocol::HttpProtobuf => {
            let headers: std::collections::HashMap<String, String> = cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            MetricExporterBuilder::new()
                .with_http()
                .with_endpoint(cfg.endpoint.clone())
                .with_headers(headers)
                .build()
                .map_err(TelemetryError::OtlpBuild)?
        }
    };

    Ok(PeriodicReader::builder(exporter)
        .with_interval(cfg.interval)
        .build())
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("prometheus exporter init: {0}")]
    Prometheus(#[from] opentelemetry_sdk::error::OTelSdkError),
    #[error("otlp exporter build: {0}")]
    OtlpBuild(opentelemetry_otlp::ExporterBuildError),
    #[error("otlp header config: {0}")]
    OtlpHeader(String),
}

/// Process-global telemetry handle. Set once at daemon boot via
/// [`set_global`]. Core call sites (cartridge, audit, cloud backends)
/// record through the [`record`] free functions below, which no-op
/// when the global is unset (CLI / unit tests / `--test` smoke runs).
///
/// This avoids plumbing a `Telemetry` argument through every
/// internal API.
static GLOBAL: std::sync::OnceLock<Telemetry> = std::sync::OnceLock::new();

/// Install the process-global telemetry instance. Idempotent: a
/// second call is a no-op (returns Err) so the daemon's normal path
/// and `--test` smoke runs don't fight over the slot.
pub fn set_global(t: Telemetry) -> Result<(), Telemetry> {
    GLOBAL.set(t)
}

/// Borrow the global if installed. Returns `None` for non-daemon
/// callers; record-helpers below treat that as "drop the sample."
pub fn global() -> Option<&'static Telemetry> {
    GLOBAL.get()
}

/// Free-function recording helpers. Each is a thin wrapper that
/// looks up the global and forwards if set. Designed so call sites
/// stay one line and don't need to handle the `Option`.
pub mod record {
    use super::global;

    pub fn pool_used(backend: &str, used: u64) {
        if let Some(t) = global() {
            t.pool_set_used(backend, used);
        }
    }
    pub fn pool_cap(backend: &str, cap: u64) {
        if let Some(t) = global() {
            t.pool_set_cap(backend, cap);
        }
    }
    pub fn pool_eviction(backend: &str) {
        if let Some(t) = global() {
            t.pool_inc_eviction(backend);
        }
    }
    pub fn pool_backpressure_wait(backend: &str, seconds: f64) {
        if let Some(t) = global() {
            t.pool_record_backpressure_wait(backend, seconds);
        }
    }
    /// `system monitor`'s "waiters now" gauge: increment when
    /// `PoolBudget::try_reserve` enters its wait loop, paired with one
    /// `pool_waiters_dec` on every exit path. No-op when the global is
    /// unset (unit tests, CLI).
    pub fn pool_waiters_inc(backend: &str) {
        if let Some(t) = global() {
            t.live_stats().inc_waiters(backend);
        }
    }
    pub fn pool_waiters_dec(backend: &str) {
        if let Some(t) = global() {
            t.live_stats().dec_waiters(backend);
        }
    }
    pub fn cache_eviction(volume: &str, outcome: &str) {
        if let Some(t) = global() {
            t.cache_inc_eviction(volume, outcome);
        }
    }
    pub fn cache_miss_after_eviction(backend: &str, seconds: f64) {
        if let Some(t) = global() {
            t.cache_record_miss_after_eviction(backend, seconds);
        }
    }
    pub fn scsi_xcopy(outcome: &str) {
        if let Some(t) = global() {
            t.scsi_xcopy_inc(outcome);
        }
    }
    pub fn scsi_xcopy_bytes(path: &str, bytes: u64) {
        if let Some(t) = global() {
            t.scsi_xcopy_add_bytes(path, bytes);
        }
    }
    pub fn scsi_xcopy_segment(path: &str) {
        if let Some(t) = global() {
            t.scsi_xcopy_inc_segment(path);
        }
    }
    pub fn storage_request(backend: &str, op: &str, outcome: &str, bytes: u64, seconds: f64) {
        if let Some(t) = global() {
            t.storage_record_request(backend, op, outcome, bytes, seconds);
        }
    }
    pub fn storage_retry(backend: &str, class: &str) {
        if let Some(t) = global() {
            t.storage_inc_retry(backend, class);
        }
    }
    pub fn storage_permanent_error(backend: &str, class: &str) {
        if let Some(t) = global() {
            t.storage_inc_permanent_error(backend, class);
        }
    }
    pub fn chunk_seal(backend: &str, scope: &str) {
        if let Some(t) = global() {
            t.chunk_inc_seal(backend, scope);
        }
    }
    pub fn chunk_dedup_hit(backend: &str, scope: &str) {
        if let Some(t) = global() {
            t.chunk_inc_dedup_hit(backend, scope);
        }
    }
    pub fn chunk_uploaded_bytes(backend: &str, bytes: u64) {
        if let Some(t) = global() {
            t.chunk_add_uploaded_bytes(backend, bytes);
        }
    }
    pub fn chunk_logical_bytes(backend: &str, scope: &str, bytes: u64) {
        if let Some(t) = global() {
            t.chunk_add_logical_bytes(backend, scope, bytes);
        }
    }
    pub fn chunk_unique_bytes(backend: &str, scope: &str, bytes: u64) {
        if let Some(t) = global() {
            t.chunk_add_unique_bytes(backend, scope, bytes);
        }
    }
    pub fn chunk_storage_head_probe(backend: &str) {
        if let Some(t) = global() {
            t.chunk_inc_storage_head_probe(backend);
        }
    }
    pub fn chunk_storage_head_hit(backend: &str) {
        if let Some(t) = global() {
            t.chunk_inc_storage_head_hit(backend);
        }
    }
    pub fn chunk_storage_cache_hit(backend: &str) {
        if let Some(t) = global() {
            t.chunk_inc_storage_cache_hit(backend);
        }
    }
    pub fn chunk_storage_cache_inflight_coalesced(backend: &str) {
        if let Some(t) = global() {
            t.chunk_inc_storage_cache_inflight_coalesced(backend);
        }
    }
    pub fn chunk_storage_cache_warmup_seeded(backend: &str, n: u64) {
        if let Some(t) = global() {
            t.chunk_add_storage_cache_warmup_seeded(backend, n);
        }
    }
    pub fn chunk_upload_stranded(backend: &str, reason: &str) {
        if let Some(t) = global() {
            t.chunk_inc_upload_stranded(backend, reason);
        }
    }
    pub fn upload_queue_depth(n: i64) {
        if let Some(t) = global() {
            t.upload_set_queue_depth(n);
        }
    }
    pub fn iscsi_sessions_active(n: i64) {
        if let Some(t) = global() {
            t.iscsi_set_sessions_active(n);
        }
    }
    pub fn iscsi_command(opcode: &str, outcome: &str, seconds: f64) {
        if let Some(t) = global() {
            t.iscsi_record_command(opcode, outcome, seconds);
        }
    }
    pub fn iscsi_data_in(bytes: u64) {
        if let Some(t) = global() {
            t.iscsi_add_data_in(bytes);
        }
    }
    pub fn iscsi_data_out(bytes: u64) {
        if let Some(t) = global() {
            t.iscsi_add_data_out(bytes);
        }
    }
    pub fn audit_entry(kind: &str) {
        if let Some(t) = global() {
            t.audit_inc_entry(kind);
        }
    }
    pub fn audit_chain_reset() {
        if let Some(t) = global() {
            t.audit_inc_chain_reset();
        }
    }
    pub fn audit_queue_drop() {
        if let Some(t) = global() {
            t.audit_inc_queue_drop();
        }
    }
    pub fn alerts(class: &str, severity: &str, sink: &str, outcome: &str) {
        if let Some(t) = global() {
            t.alerts_record(class, severity, sink, outcome);
        }
    }
    pub fn orphan_scan_completed(chunks_found: u64, duration_seconds: f64) {
        if let Some(t) = global() {
            t.orphan_scan_record(chunks_found, duration_seconds);
        }
    }
}

/// Backwards-compat alias so existing call sites that say
/// `core_mediachanger::Metrics` keep compiling. Prefer `Telemetry` in
/// new code; this alias will go away once the daemon migrates fully.
pub type Metrics = Telemetry;

impl Telemetry {
    /// Drop-in replacement for the old `Metrics::new()` so the daemon
    /// boot path keeps working with no config change.
    #[deprecated(note = "use Telemetry::new(&TelemetryConfig) directly")]
    pub fn new_default() -> Result<Self, TelemetryError> {
        Self::new(&TelemetryConfig::default())
    }

    /// Compat shim for the old `metrics.export()` callsite in the
    /// HTTP handler. Forwards to `export_prometheus`.
    pub fn export(&self) -> String {
        self.export_prometheus()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_default_telemetry() {
        let t = Telemetry::new(&TelemetryConfig::default()).expect("build");
        // Smoke: every instrument should appear in the prometheus
        // dump after at least one observation.
        t.pool_set_used("primary", 0);
        t.pool_set_cap("primary", 1024);
        t.storage_record_request("primary", "put", "ok", 1024, 0.05);
        t.chunk_inc_seal("primary", "global");
        t.chunk_add_logical_bytes("primary", "global", 8 * 1024 * 1024);
        t.chunk_add_unique_bytes("primary", "global", 8 * 1024 * 1024);
        t.chunk_inc_storage_head_probe("primary");
        t.chunk_inc_storage_head_hit("primary");
        t.chunk_inc_storage_cache_hit("primary");
        t.chunk_inc_storage_cache_inflight_coalesced("primary");
        t.chunk_add_storage_cache_warmup_seeded("primary", 42);
        t.upload_set_queue_depth(0);
        t.iscsi_set_sessions_active(0);
        t.iscsi_record_command("write_16", "good", 0.001);
        t.audit_inc_entry("daemon.start");
        t.audit_inc_chain_reset();
        t.audit_inc_queue_drop();
        t.cache_inc_eviction("vol1", "dirty");
        t.cache_record_miss_after_eviction("primary", 12.5);
        // Record the rest of the counter surface so the no-double-suffix
        // assertion below actually exercises the renamed instruments.
        t.pool_inc_eviction("primary");
        t.pool_record_backpressure_wait("primary", 0.01);
        t.storage_inc_retry("primary", "Network");
        t.storage_inc_permanent_error("primary", "Auth");
        t.chunk_inc_dedup_hit("primary", "global");
        t.chunk_inc_upload_stranded("primary", "backend_unknown");
        t.prefetch_inc_hit();
        t.prefetch_inc_miss();
        t.alerts_record("disk_cache", "warning", "log", "fired");
        t.scsi_xcopy_inc("success");
        t.scsi_xcopy_add_bytes("fast", 4096);
        t.scsi_xcopy_inc_segment("fast");
        t.daemon_set_start_time(1);
        t.orphan_scan_record(0, 0.0);
        let dump = t.export_prometheus();
        // Regression guard for the OTel->Prometheus suffix doubling
        // (issue #93: instrument names that bake in `_total` / `_bytes`
        // get the exporter's suffix appended a second time). Instrument names
        // must omit the unit + counter suffix and let the exporter add
        // them exactly once.
        assert!(
            !dump.contains("_total_total"),
            "counter name baked in `_total`; exporter doubled it:\n{dump}"
        );
        assert!(
            !dump.contains("_bytes_bytes"),
            "byte metric baked in `_bytes`; exporter doubled it:\n{dump}"
        );
        for exact in [
            "# TYPE thur_pool_evictions_total counter",
            "# TYPE thur_storage_requests_total counter",
            "# TYPE thur_scsi_xcopy_total counter",
            "# TYPE thur_scsi_xcopy_copied_bytes_total counter",
            "# TYPE thur_audit_entries_total counter",
        ] {
            assert!(dump.contains(exact), "missing exact `{exact}` in:\n{dump}");
        }
        for needle in [
            "thur_pool_used_bytes",
            "thur_pool_cap_bytes",
            "thur_storage_requests_total",
            "thur_chunk_seals_total",
            "thur_chunk_logical_bytes",
            "thur_chunk_unique_bytes",
            "thur_chunk_storage_head_probes_total",
            "thur_chunk_storage_head_hits_total",
            "thur_chunk_storage_cache_hits_total",
            "thur_chunk_storage_cache_inflight_coalesced_total",
            "thur_chunk_storage_cache_warmup_seeded_total",
            "thur_cache_evictions_total",
            "thur_cache_miss_after_eviction_seconds",
            "thur_upload_queue_depth",
            "thur_iscsi_sessions_active",
            "thur_audit_entries_total",
        ] {
            assert!(
                dump.contains(needle),
                "missing metric `{needle}` in:\n{dump}"
            );
        }
    }

    #[test]
    fn telemetry_export_compat_shim() {
        let t = Telemetry::noop();
        t.storage_inc_retry("primary", "Network");
        let s = t.export();
        assert!(s.contains("thur_storage_retries_total"));
    }

    #[test]
    fn label_values_distinguish_series() {
        let t = Telemetry::noop();
        t.pool_set_used("primary", 100);
        t.pool_set_used("archive", 200);
        let dump = t.export_prometheus();
        assert!(dump.contains("backend=\"primary\""));
        assert!(dump.contains("backend=\"archive\""));
    }

    #[test]
    fn live_stats_chunk_mirror_folds_scope_per_backend() {
        let t = Telemetry::noop();
        // Two scopes on the same backend must sum into one per-backend
        // pair; a second backend stays separate.
        t.chunk_add_logical_bytes("primary", "global", 1000);
        t.chunk_add_unique_bytes("primary", "global", 400);
        t.chunk_add_logical_bytes("primary", "local", 500);
        t.chunk_add_unique_bytes("primary", "local", 500);
        t.chunk_add_logical_bytes("archive", "global", 200);
        t.chunk_add_unique_bytes("archive", "global", 50);

        let snap = t.live_stats().snapshot();
        let primary = snap.chunk.get("primary").expect("primary chunk row");
        assert_eq!(primary.logical_bytes, 1500);
        assert_eq!(primary.unique_bytes, 900);
        let archive = snap.chunk.get("archive").expect("archive chunk row");
        assert_eq!(archive.logical_bytes, 200);
        assert_eq!(archive.unique_bytes, 50);
    }
}
