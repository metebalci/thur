# Telemetry

Both daemons emit operational metrics, and this document explains how
that machinery is put together and how an operator turns the raw
numbers into alerts. It is the operator-facing picture. If you need the
exact byte-level surface — every instrument with its type, unit, and
attribute keys, plus the `telemetry.otlp.*` config block — that lives
in [`SPEC.md`](SPEC.md) § Telemetry.

## How metrics leave the daemon

Everything is built on a single OpenTelemetry `MeterProvider`, set up
once in `shared/telemetry/src/lib.rs`. The design choice worth
understanding is what hangs off it: **two readers, both attached to
that same provider**. The code is instrumented exactly once, but the
numbers can leave the process two different ways.

The first reader is a **Prometheus pull endpoint**. It is always
wired, and it is served at `GET /metrics` on the HTTP listener the
daemon already runs for `/health`, `/sessions`, and `/info` — so there
is no second port to open and no on/off switch. If you want metrics
off entirely, you drop the whole `http:` block from the config and the
listener never comes up. The output is rendered in the usual
Prometheus text format by `opentelemetry-prometheus`.

The second reader is an **OTLP push exporter**, and unlike the first
it is opt-in: it only runs when `telemetry.otlp.enabled` is `true`.
When enabled, a periodic reader takes the same instruments and pushes
them out over OTLP — gRPC on `:4317` or HTTP/protobuf on `:4318` — to
a Collector or any OTLP-compatible backend such as Datadog, Honeycomb,
or Grafana Cloud.

Because both readers walk the *same* in-memory state, there is no
double counting and no way for the two surfaces to disagree: a call
site records a sample once, and whichever reader fires next simply
observes it.

## Why bother with two readers

The two readers exist because real deployments fall into two camps. An
operator running self-hosted or on-prem almost always already has a
Prometheus stack, and what they want from the daemon is a scrape
endpoint to point it at. An operator on a managed observability
backend wants the opposite — the daemon should dial *out*, pushing
metrics through the corporate proxy, with no Collector sitting in
between as one more thing to run.

Supporting both is cheap precisely because they share the
`MeterProvider`: roughly 50 lines of code and one config block. The
alternative — instrumenting the two code paths separately — is the
kind of thing that quietly rots, because nobody notices when only one
of the two surfaces gets updated.

## The process-global handle

Metrics get recorded from deep inside the core — the cartridge code,
the cloud backends, the iSCSI layer, the pool-budget logic. Threading
a `Telemetry` handle as an argument through all of those call paths
would be invasive, so instead each daemon installs a **process-global
handle** at boot, via `shared_telemetry::set_global`. Core call sites
then record through the `shared_telemetry::record::*` free functions
(also re-exported as `core_mediachanger::metrics::record::*`), which
locate that global on their own.

This has a deliberate side effect worth knowing about. A CLI
invocation or a unit test never installs the global. So when test code
or the CLI runs through those same core paths, the `record::*` calls
simply no-op at the entry point. You get no telemetry plumbing in test
code and no test-only samples polluting real metrics — for free, just
from whether the global was installed.

## How instruments are named

Every instrument name follows the shape `<prefix>_<subsystem>_<name>`.
The prefix is what keeps the two products apart, so that a single
shared backend can scrape both daemons without their instrument names
colliding:

| Product | Prefix | Source |
|---|---|---|
| Thur VTL (`thurvtld`) | `thurvtl_*` | `shared_naming::TAPE_LIBRARY.metric_prefix` |
| Thur VSA (`thurvsad`) | `thurvsa_*` | `shared_naming::DISK.metric_prefix` |

One naming subtlety trips people up. When an instrument declares a
unit — `s` for seconds, `By` for bytes — the OpenTelemetry-to-Prometheus
exporter automatically appends the conventional suffix (`_seconds`,
`_bytes`) on the way out. So you must **not** bake that suffix into the
instrument name yourself, or you will get it twice. The internal name
is `pool_backpressure_wait` with unit `s`; what Prometheus actually
exposes is `thurvtl_pool_backpressure_wait_seconds` (or `thurvsa_*`).

There is also a `service.name` resource attribute (`thurvtl` or
`thurvsa`) carried on the `target_info` series. It encodes the same
product distinction a second time, which is intentional: dashboards
can group by either the name prefix or the attribute. The prefix is
the one that really has to be there, because it survives flat scrape
concatenation — where every series lands in one namespace —
while `service.name` is just the convenient knob for relabeling at an
OTLP Collector hop.

## What each subsystem watches

The full instrument table is in [`SPEC.md`](SPEC.md) § Telemetry. At
the subsystem level, the metrics break down like this — each row is a
coherent area of the daemon and the files that instrument it:

| Subsystem | What you're watching | Source files |
|---|---|---|
| `pool` | Disk-cache budget, eviction pressure, backpressure waits | `chunk_store.rs`, `disk_cache.rs` |
| `cloud` | Per-backend request rates, latencies, error classes | `s3.rs` / `gcs.rs` / `azure.rs` / `local.rs` via `cloud_backend.rs` |
| `chunk` | Seal rate, dedup hit rate (local + cloud-side via HEAD), upload bytes | `cartridge.rs`, upload worker in daemon |
| `iscsi` | Active sessions, per-opcode throughput, data-in/out bytes | `vtl/daemon/src/iscsi/*` |
| `tape` | Per-cartridge memory-buffer occupancy | `memory_buffer_manager.rs` |
| `prefetch` | Queue depth, hit/miss counts | `prefetch.rs` |
| `audit` | Append rate by entry kind, chain resets, queue drops | `audit.rs` |
| `recovery` | Orphan-upload scan: chunks found by boot-time sweep + scan duration | `vtl/daemon/src/upload_recovery.rs` (thurvtl only) |
| `fetb` | Latest FETB sample (bytes) + sample count in the trailing 4-week window | `shared/audit/src/fetb.rs` |
| `daemon` | Process start time | `vtl/daemon/src/main.rs` |

## Metrics meant to be combined

A handful of instruments are not very interesting on their own — they
are designed to be divided into ratios that tell you something an
operator actually cares about. Substitute the product prefix as needed
(`thurvtl_*` for tape, `thurvsa_*` for block):

- **Dedup ratio** = `thurvtl_chunk_logical_bytes_total /
  thurvtl_chunk_unique_bytes_total`. This is how much the local pool
  saves relative to the host's logical write volume — the bytes the
  host thinks it wrote, over the unique bytes that actually had to be
  stored.
- **Cloud upload-skip rate** = `thurvtl_chunk_cloud_head_hits_total /
  thurvtl_chunk_cloud_head_probes_total`. Before uploading a chunk the
  daemon issues a HEAD request; if the object is already in the bucket
  the PUT is skipped. This ratio is that cloud-side dedup signal — how
  often a HEAD said "already there."
- **Pool fill** = `thurvtl_pool_used_bytes / thurvtl_pool_cap_bytes`.
  This is the primary backpressure trigger. The closer it sits to
  `1.0`, the more often a chunk-seal has to block waiting for room.
- **FETB** = `thurvtl_fetb_latest_bytes`. The most recent front-end
  TiB sample — bytes the host wrote, measured before dedup and before
  compression. It is operational telemetry only: no cap, no gate. Both
  products emit the `*_fetb_*` series.

The dedup analytics CLI (`thurvtl system stats`) walks
`chunks.idx` directly when you want per-cartridge breakdowns. The
cloud HEAD-skip rate, by contrast, is exposed *only* through these
counters — it is a runtime signal, not state that exists anywhere on
disk to be walked.

## Alerts worth setting (PromQL)

The point of the examples below is that each one maps to a concrete
operator action — an alert you cannot act on is just noise. They use
the `thurvtl_*` prefix; swap in `thurvsa_*` for the block product,
since the `pool`, `cloud`, and `audit` instrument bodies exist on both.

```promql
# Pool consistently > 90% full — operator should raise
# disk_cache.max_size_gb (on `auto`) or disk_cache.size_gb (explicit),
# or investigate why uploads aren't draining.
thurvtl_pool_used_bytes / thurvtl_pool_cap_bytes > 0.9

# Backpressure waits firing for 5 minutes — host writes are outpacing
# cloud uploads; backups will see SCSI NOT READY soon if not already.
rate(thurvtl_pool_backpressure_waits_total[5m]) > 0

# Permanent cloud errors (Auth / Authz / NotFound / RegionMismatch) —
# the retry layer already short-circuits these, so any non-zero rate
# means broken credentials or config drift.
rate(thurvtl_cloud_permanent_errors_total[5m]) > 0
```

## A note on the Prometheus exporter crate

`opentelemetry-prometheus = 0.31` is flagged "discontinued" upstream —
the OTel project's official recommendation is to route Prometheus
output through the Collector instead. For a self-hosted appliance,
though, the in-process bridge is still the right call: it is one fewer
moving part for the operator to run and keep alive. If the crate
eventually bit-rots against a newer `opentelemetry` release, the
escape hatch is a custom registry-walker of roughly 200 lines. Either
way the instrument inventory and the naming convention above are
stable — they do not depend on which exporter is in use.
