# Alerting

Both daemons (VTL and VSA) support first-party alerting via email and generic
webhooks. Alerting is off by default and enabled by adding an `alerting:`
block to the YAML configuration.

## Motivation

The daemon already emits Prometheus metrics and maintains a tamper-evident
audit log. For operators running Prometheus, Alertmanager, and a log
aggregator, those surfaces are sufficient. This subsystem exists for
operators who want incident notifications without standing up Alertmanager: by
editing a single YAML block they can wire email or a PagerDuty / Slack /
Discord / ntfy.sh / ServiceNow webhook directly into the daemon.

## Event classes

Four classes of event are supported, each individually toggled via
`alerting.events.*`. The defaults reflect signal-to-noise tradeoffs:
`audit_failure` and `chap_failures` are on by default (high-signal, low
frequency), while `backend_reachability` and `disk_cache_backpressure` are
off by default (useful, but potentially noisier in practice).

| Class | Severity | Source | Dedup key |
|---|---|---|---|
| `backend_reachability` | error / info | `system storage check` job (VTL; VSA: future periodic ticker) | `<backend>:<failure\|recovery>` |
| `audit_failure` | error | `shared/audit/src/audit_channel.rs` writer task on `AuditLog::append` Err (disk write / fsync / chain-state), via a function-pointer hook installed at boot (avoids the shared-alerting → shared-audit dep cycle) | `<op>` |
| `disk_cache_backpressure` | warn / error | Watermark crossing in the per-product disk-cache eviction worker (VTL + VSA); backpressure-timeout error construction in `shared/pool/src/budget.rs::try_reserve`; VSA `lru.idx` sidecar persistently unwritable (eviction degrades to first-seen), latched once per volume in `core/block/src/uploader.rs` | `<backend>:watermark` / `<backend>:backpressure` / `<volume>:lru_index` |
| `chap_failures` | warn | `shared/iscsi/src/transport.rs` CHAP path, surfaced by the daemon's `LoginAuditSink` adapter (VTL `IscsiLibraryLoginAudit`, VSA `IscsiDiskLoginAudit`). Per-user counter in the dispatcher; WARN fires once the user crosses `alerting.chap_failures_threshold` (default 3) inside one window | `chap:<user>` |

The dispatcher special-cases `backend_reachability`: alerts fire
only on status transitions (healthy → failing or failing → healthy),
so a `storage check` invoked twice against an already-failed backend
doesn't double-page.

## Rate-limiting

The dispatcher wraps [`shared_audit::audit_ratelimit::AuditRateLimiter`]
to collapse repeated events. Within a given `(class, dedup_key)` window,
the first event is sent through; subsequent events are counted rather than
dispatched. The count accumulates as
`<product>_alerts_fired_total{outcome="suppressed"}` so operators have
visibility into how many events were absorbed.

`alerting.dedup_window_seconds` (default 300) controls the window
duration. The same value resets the per-user CHAP failure counter.

## Failure policy

The dispatcher does not retry failed sink sends. When a send fails, it logs
the sink name, the event class, and the raw error at WARN level, increments
`<product>_alerts_fired_total{outcome="failure"}`, and drops the alert.
Operators can observe gaps through that counter. The dedup window already
absorbs short network flaps, and persistent retry queues were deliberately
deferred to a post-v1 iteration.

## Sinks

### `email` — SMTP via `lettre`

```yaml
sinks:
  - name: ops
    type: email
    host: smtp.example.com
    port: 587
    starttls: true              # alternative: false for plaintext + PLAIN
    username: alerts@example.com
    password: ${SMTP_PASSWORD}  # ${ENV_VAR} resolved at boot
    from: thurvtl@example.com
    to:
      - oncall@example.com
    # subject_prefix: "[prod-east thurvtl]"   # default "[<product> ALERT]"
```

- Body is plain-text: a fixed-format header (product, class,
  severity, timestamp) followed by `message` and the alert's
  `fields` map.
- `username` empty disables AUTH (relay accepts the host's IP).
- TLS via rustls (lettre's `tokio1-rustls-tls`).

### `webhook` — generic HTTP POST with Tera template

```yaml
sinks:
  - name: pagerduty
    type: webhook
    url: https://events.pagerduty.com/v2/enqueue
    method: POST                   # default; override for PUT
    headers:                       # values support ${ENV_VAR}
      X-Routing-Key: ${PAGERDUTY_ROUTING_KEY}
    body_template: |
      {
        "routing_key": "${PAGERDUTY_ROUTING_KEY}",
        "event_action": "trigger",
        "payload": {
          "summary": "{{message}}",
          "severity": "{{severity}}",
          "source": "{{product}}"
        }
      }
    timeout_seconds: 10            # default
```

Template variables:

- `class` — `backend_reachability` / `audit_failure` / …
- `severity` — `info` / `warn` / `error`
- `message` — operator-readable message
- `timestamp` — RFC 3339
- `product` — `thurvtl` / `thurvsa`
- `version` — daemon version
- `fields.*` — every key from the alert's `fields` map

Empty `body_template` sends the canonical Alert JSON unchanged:

```json
{
  "product": "thurvtl",
  "version": "0.1.0-alpha.1",
  "class": "backend_reachability",
  "severity": "error",
  "message": "Storage backend 'primary' unreachable",
  "fields": { "backend": "primary", "outcome": "failure" },
  "timestamp": "2026-05-17T14:22:01.123Z"
}
```

Content-Type defaults to `application/json` when the operator
doesn't supply one in `headers`.

## Worked examples

### Slack incoming webhook

```yaml
sinks:
  - name: slack
    type: webhook
    url: https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXX
    body_template: |
      {"text": "[{{severity}}] {{product}} {{class}}: {{message}}"}
```

### Discord webhook

```yaml
sinks:
  - name: discord
    type: webhook
    url: https://discord.com/api/webhooks/000000000000000000/xxxxxxxxxxxxxxxx
    body_template: |
      {"content": "**[{{severity}}]** `{{product}}` *{{class}}*: {{message}}"}
```

### ntfy.sh (self-hosted or public)

```yaml
sinks:
  - name: ntfy
    type: webhook
    url: https://ntfy.sh/your-private-topic
    headers:
      Title: "thurvtl alert"
      Priority: "high"
      Tags: "warning,storage"
    body_template: |
      [{{severity}}] {{class}}: {{message}}
```

ntfy expects `text/plain` when `Title` lives in headers; the
operator-supplied `headers:` map overrides the daemon's default
`Content-Type: application/json`.

### Multiple sinks

The daemon fans every alert out to every configured sink in
parallel. One slow sink doesn't backpressure the others.

## CLI

Both products mount the same verbs:

```
thurvtl system alerting list                   # show sinks + dedup window
thurvtl system alerting test <SINK_NAME> [--severity warn]
```

`test` fires a synthetic alert through one sink only, **bypassing
the rate limiter and the per-class event gate**, so you can verify
SMTP creds / webhook templates without waiting for a real event.

Both verbs are daemon-routed only — alerting state lives only in the
running daemon, with no daemon-down fallback.

## Telemetry

One counter, four labels:

```
<product>_alerts_fired_total{class, severity, sink, outcome}
  outcome ∈ {success, failure, suppressed}
```

Pair with the existing `audit_*` series for a full picture.

## Architecture

```
producers                       ┌─────────────────────────────┐
─────────                       │ shared-alerting             │
  audit_chan ─┐                 │  AlertingDispatcher         │
  iscsi xport ┼─► record::*  ──►│   ├─ AlertRateLimiter       │
  pool budget ┤                 │   ├─ ChapState (per-user)   │
  storage check ┘               │   ├─ BackendStatus (txns)   │
                                │   └─ Vec<Arc<dyn AlertSink>>│
                                │        ├ EmailSink (lettre) │
                                │        └ WebhookSink (Tera) │
                                └─────────────────────────────┘
                                            │ fan out
                                            ▼
                                  ┌───────────────────┐
                                  │  outbound network │
                                  └───────────────────┘
```

`AlertingDispatcher` is process-global (`shared_alerting::set_global`),
mirroring `shared_telemetry::set_global`. Producers call `record::*` free
functions rather than carrying a per-call-site `Option<AlertChannel>` handle,
so wiring a new event source requires no plumbing changes deeper in the call
stack. The single struct holds the rate limiter, per-user CHAP counters, and
the per-backend last-status map, giving v1 one cohesive locking surface.
Tests and `--test` smoke runs skip installing the global, so producers no-op
cleanly.

### Boot order

Both daemons:

1. Build `Telemetry` and install as process-global.
2. If `alerting.enabled: true`, build `AlertingDispatcher` from the
   YAML block (synchronous; SMTP relay construction +
   `${ENV_VAR}` interpolation fail the daemon at boot, not at first
   alert) and install as process-global.
3. Install `shared_alerting::record::audit_append_failed` as the
   audit `AppendFailureHook` (bridges the writer-task failure path
   into alerting without forming a shared-audit → shared-alerting
   dep cycle).

## Open items (post-v1)

1. **Periodic storage-check ticker** — today `backend_reachability`
   fires only on operator-invoked `system storage check`. A
   `storage.check_interval_seconds` knob (default off) would let
   overnight-failure detection ship without an operator at the
   console.
2. **VSA `system.cloud_check` job** — VTL has the job (in-code job
   kind is still `system.cloud_check`; CLI verb is `system storage
   check`), VSA doesn't. Lift VTL's `cloud_check.rs` into a shared
   crate, mount on both job_dispatch tables.
3. **AlertingDispatcher::sink_specs()** — the
   `/api/v1/system/alerting` handler today labels every sink as
   `type: configured`. A sink-type round-trip would let the CLI's
   `list` verb show the real type without re-reading YAML.
