# Web UI

This document explains the read-only Web UI that ships embedded in both
daemons (issue #5) — what it is, why it is shaped the way it is, and
where the pieces live. It is the authoritative reference on the subject;
other docs link here rather than repeat it.

## What it is

Each daemon's TCP HTTP listener — the same one that already answers
`/health`, `/metrics`, `/sessions`, and `/info` — also serves a small
operator console at `/ui/` and a read-only slice of its `/api/v1` JSON
surface. Point a browser at `https://<host>:9090/ui/`, authenticate with
the web-admin password, and you get a live dashboard: a strip of KPI
cards (drives, cartridges, sessions, pool cache, backend
upload/download throughput, and a **lifetime dedup ratio**), the
application's inventory — a schematic **library map** on VTL (a drives row
over a storage-slot grid, each filled slot showing its barcode) or a
**volume list** on VSA — plus a **storage backends** table (which also
carries a per-backend dedup column) and an **audit log** tail. It polls
every few seconds; there is no streaming and no mutation.

The dedup figures come straight from the monitor snapshot's per-backend
`dedup` array (`logical_bytes / unique_bytes`, summed for the headline
KPI). They are **cumulative since daemon restart** — append-only
counters that ignore eviction and deletion — so the KPI is labelled
"Dedup (lifetime)" to set it apart from the exact current-on-disk
breakdown, which stays on the `system stats` scan. Both applications feed it
identically (tape and block both record logical/unique at chunk seal). Everything you can
do through it you could already do read-only through the CLI — it is a
window onto the same state, not a new control plane.

Mutations are explicitly out of scope for v1. Creating a cartridge,
moving a changer element, making a volume, taking a snapshot — all of
that stays on the CLI and the peer-cred admin socket. The mutating Web
UI is tracked separately as issue #91.

## Why embedded, not a separate daemon

The obvious alternative — a standalone `thurweb` service that proxies to
both daemons — was rejected. A VTL or VSA host already runs exactly one
daemon that owns all the state worth showing; a second process would add
a deployment unit, a second port to secure, a second thing to package
and supervise, and an inter-process hop, all to display data the daemon
already holds in memory. Embedding the UI in the daemon that owns the
data is simpler on every axis that matters to a self-hosted operator.
There is therefore one UI per application, on that application's existing
listener, with no new daemon, no new port, and no new auth surface.

## The no-build stack

The bundle is three hand-written files — `index.html`, `app.css`, and
`app.js` — and nothing else. No Node, no npm, no bundler, no transpile
step, no framework. `app.js` is vanilla ES, fetches the read-only API
with the browser's own HTTP Basic session, and repaints on a timer. It
branches on the `product` field returned by `/info` to render the tape
library view or the storage-array view from the same code.

This is a deliberate constraint, not a limitation we regret. A
build-free bundle has no supply chain to audit, no lockfile to keep
current, no `node_modules` to ship, and it is readable end to end by
anyone who can read HTML. The cost — that you write DOM code by hand
instead of leaning on a framework — is small for a read-only dashboard
of a handful of panels.

All visual styling lives in CSS custom properties under `:root` in
`app.css`. That block is the single restyle surface: colors, spacing,
typography, the per-application accent, and density are all tokens there.
The markup never hard-codes a color and the JavaScript never encodes
layout, so an operator who wants to rebrand the console edits tokens in
one place and touches nothing else.

## How the assets are served

The bundle is embedded into the binary at compile time via
`include_dir!`, so a bare binary with no package assets installed still
serves a working UI. On top of that, `http.webui.asset_dir` is an
optional on-disk override: when it names a directory, a requested file
found there wins, and a file missing there falls back to the embedded
copy. The package installs the same three files at
`/usr/share/<application>/webui/` precisely so an operator can set
`asset_dir` to that path and restyle (edit `app.css`) without rebuilding
the daemon.

Path safety is handled before either lookup. A requested sub-path is
normalized and any traversal segment (`..`), absolute path, or empty
segment is rejected outright, so neither the disk read nor the embedded
read can escape the asset root.

## Read-only, and read-only on the wire

The rich `/api/v1` surface — including every mutating verb — lives on
the peer-cred-authed Unix admin socket. The Web UI needs a subset of it
reachable over TCP, so each daemon mounts a **GET-only** slice of that
surface on the protected TCP listener. The exact routes are enumerated
in [`SPEC.md`](SPEC.md) § HTTP Endpoints, and the same read-only surface
has a machine-readable OpenAPI 3.0 contract in
[`openapi.yaml`](openapi.yaml) (issue #12) for downstream consumers —
web UI, third-party automation — to build against instead of re-deriving
the wire format from source. That spec is kept honest by a sync guard:
`vtl/daemon/tests/openapi_sync.rs` and `vsa/daemon/tests/openapi_sync.rs`
parse each daemon's TCP router and fail if a mounted route is missing
from the spec. The guarantee is structural,
not a runtime check: only GET handlers are registered on the TCP router,
and every mutating handler takes a `PeerCred` extractor that the TCP
transport cannot satisfy, so the TCP surface cannot mutate state no
matter what credentials are presented.

Three of the read-only handlers are genuinely identical across both
applications — the monitor snapshot, the recent-jobs list, and the
audit-log tail — and live in the shared crate. (The v1 dashboard
surfaces the monitor and audit data prominently and uses the monitor
snapshot's per-backend byte counters for the throughput KPI and the
storage-backends table; the recent-jobs endpoint is still served for
API consumers but no longer has its own panel.) The rest are
per-application inventory reads and stay in each daemon, since they are
typed on that application's own `AdminState`.

One read handler is deliberately left off the TCP surface: VTL's
`legal_hold_status`. It is the single read endpoint that performs
network backend I/O (it queries the object store's lock state), and
keeping the TCP surface local-state-only means a Web UI poll can never
fan out into storage calls.

## The auth dependency

The Web UI does not own its own authentication. It hangs on the
web-admin password gate (issue #4), documented in [`NETWORK_SECURITY.md`](../admin/NETWORK_SECURITY.md) §
Admin password. That gate splits the listener into an **open** group —
`/health` and `/metrics`, left unauthenticated so Prometheus scrapes and
liveness probes keep working — and a **protected** group that carries
`/sessions`, `/info`, the Web UI's `/ui/*` bundle, and its read-only
`/api/v1` routes. The Web UI joins the protected group through the very
same middleware and in-process verifier that guards `/sessions` and
`/info`; there is no second password and no Web UI login form.

Whether that protected group is actually gated is the operator's choice,
via `http.auth.method` (see [`NETWORK_SECURITY.md`](../admin/NETWORK_SECURITY.md) § _The gate is
optional_). The **default is `None`**: the read-only console is served
open, on the assumption that the management listener sits on an isolated
or trusted network — the same posture the iSCSI data plane defaults to,
and a smaller exposure since the UI is read-only metadata. Setting
`http.auth.method: Password` turns the gate on: the browser prompts for
the `webadmin` password on the first `401`, and the daemon's
`503`-vs-`401` split lets the UI tell "no password has been set yet"
apart from "wrong password."

Because HTTP Basic ships credentials base64-encoded rather than
encrypted, when you do enable the gate the standing recommendation from
[`NETWORK_SECURITY.md`](../admin/NETWORK_SECURITY.md) applies: enable the admin HTTP TLS listener
(`http.tls.*`) before relying on the password over anything but loopback.

## Where the code lives

The shared half is the crate `shared-admin-webui`
(`shared/admin-webui/`): it owns the static-serving logic (`ServeDir`-style
asset resolution with the embedded fallback and the traversal guard),
the `WebuiConfig` type, the `webui_router` assembly, and the three
cross-application read-only handlers. It depends only one way — on
`shared-admin-auth` (for the gate it reuses), `shared-admin-monitor` (for
the snapshot payload), `shared-admin-server` (for the job registry), and
`shared-audit` (for the log read) — and nothing depends back on it
except the two daemons. It is kept out of the deliberately tiny
transport crate `shared-admin-http` for the same reason
`shared-admin-iscsi` and `shared-admin-audit` are: to avoid bloating the
transport layer and to keep the dependency arrows pointing one way.

Each daemon's `http` module merges three things into its existing
protected route group: its own per-application read-only GETs, the shared
read-only handlers (mounted against the daemon's `AdminState`, which
already implements the monitor and jobs traits), and
`shared_admin_webui::webui_router` for the static bundle. The
`http.webui.enabled` flag gates the whole webui surface; turning it off
returns the listener to the `/health` `/metrics` `/sessions` `/info`
posture it had before #5.

## Out of scope for v1

No mutations (issue #91). No single-page-app framework, Node, or
bundler. No graphical config editor. No real-time streaming — the
monitor snapshot is one-shot per request, the audit tail is "last N
once", and recent-jobs is a rolling 5-minute window because finished
jobs are reaped 300 s after they end. Mobile is best-effort. There is no
per-request telemetry counter yet.
