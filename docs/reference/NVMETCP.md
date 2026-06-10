# NVMe/TCP transport

VSA's data path is wire-protocol-agnostic above the `core-block` boundary.
Volumes always reduce to a per-volume `PageCache` over a content-addressed
`ChunkPool`; what changes between iSCSI and NVMe/TCP is only the framing and
the dispatcher that translates incoming commands into `PageCache` calls. That
clean separation is what makes it possible to add a second transport without
touching the storage engine at all.

This document is the NVMe/TCP design walkthrough: crate split, the
per-connection state machine, write data flow, fused Compare+Write, TLS-PSK,
DH-HMAC-CHAP in-band auth, testing, and the out-of-scope list. The per-opcode and per-feature compliance
table is in [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md); the iSCSI and SBC-3
path is in [`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md).

## Selecting the transport

`thurvsa.yaml` carries a top-level `transports:` list (a bare scalar is also
accepted):

```yaml
transports: [iscsi]              # default
# transports: [nvmetcp]
# transports: [iscsi, nvmetcp]   # both at once
```

List one or both. Each listed transport binds its own listener concurrently
against the shared volume set and the shared reservation state, so a single
volume is reachable as a SCSI LUN **and** an NVMe namespace (`nsid = lun + 1`)
at the same time (issue #66). The default ports don't clash — iSCSI keeps
`0.0.0.0:3260`, NVMe/TCP the IANA-registered `0.0.0.0:4420` — so no override is
needed to run both; each is still tunable via its own `listen:` field:

```yaml
transports: [iscsi, nvmetcp]
iscsi:
  listen: 0.0.0.0:3260
nvmetcp:
  listen: 0.0.0.0:4420
```

### Cross-protocol reservation coherence

Both data paths share one `scsi_spc::reservations::ReservationManager` keyed by
LUN, so a reservation taken over one transport fences initiators on the other:
an iSCSI initiator port `(IQN, ISID)` and an NVMe host (HOSTID) are distinct
registrant identities that never compare equal, and the SCSI↔NVMe reservation
type mapping is 1:1. A Write Exclusive reservation held by a SCSI host therefore
denies an NVMe host's writes (and vice-versa); `nvme resv-report` and
`sg_persist --read-reservation` report the same holder. The two admission
fences stay independent — see [admission](#admission-is-per-transport).

**Proactive cross-transport notification (#67):** beyond the pull-side
coherence above, a fence taken over one transport now also *pushes* a
notification to hosts on the other. The `ReservationManager` owns a
transport-neutral change diff and fires registered observers with the
issuer-excluded affected set on every mutating reservation op. Two sinks consume
it: `shared_iscsi::IscsiReservationSink` raises RESERVATIONS PREEMPTED /
RESERVATIONS RELEASED Unit Attentions on the affected iSCSI sessions, and
`nvme_nvm::AerReservationSink` raises an NVMe AER plus a LID 0x80 Reservation
Notification on the affected controllers. So an NVMe-originated preempt reaches
SCSI hosts and an iSCSI-originated release reaches NVMe hosts — not just the
reactive Reservation Conflict on the next I/O or a poll of resv-report /
read-reservation.

### Admission is per-transport

A host reachable over both transports must be admitted on **both**: a CHAP user
entry in `iscsi-users.json` (per-CHAP-user `volumes:`) for the iSCSI path and a
host-NQN entry in `nvmetcp-psks.json` (per-hostNQN `volumes:`) for the NVMe
path. The two admission fences are independent — a grant on one does not imply
the other.

## Crate split

The NVMe stack is organized in three crates layered in the same way as the
SCSI stack, with each layer having a clear SCSI analogue:

| SCSI side               | NVMe side                  | Role                          |
| ----------------------- | -------------------------- | ----------------------------- |
| `scsi-spc`              | `nvme-base`                | Common wire structs + status  |
| `scsi-sbc`              | `nvme-nvm`                 | Data-path command set         |
| `shared-iscsi` (transport bits) | `nvme-tcp`         | Network framing               |

- **`nvme-base`** — NVMe Base Spec primitives: 64-byte SQE, 16-byte
  CQE with status field (SCT / SC / DNR / M / CRD), Admin opcode
  enum, SQE CDW0 sub-fields (`Fuse`, `Psdt`), Identify Controller /
  Identify Namespace / Active NS list builders, Fabrics command
  shapes (`ConnectData`, `FabricsType`), controller register state
  (`ControllerRegs`: CC / CSTS / VS / CAP), log-page builders
  (SMART / Error Info / FW Slot).
- **`nvme-nvm`** — NVM Command Set dispatcher (`NvmeNvmDispatcher`)
  routing Read / Write / Flush / Compare / DSM / Write Zeroes /
  Verify into `core_block::PageCache`. Also handles the admin
  commands a host relies on: Identify (CNS 0x00 / 0x01 / 0x02 /
  0x03 / 0x06), Keep Alive, Get / Set Features (Number of Queues,
  Keep Alive Timer), Get Log Page (Error / SMART / FW Slot), Abort,
  and fused Compare+Write via `handle_fused_compare_write`. Resolves
  NSID via the `NamespaceLookup` trait — mirror of
  `scsi_sbc::VolumeLookup`, so the dispatcher doesn't depend on the
  daemon crate.
- **`nvme-tcp`** — NVMe/TCP transport. PDU codec
  (ICReq / ICResp / CapsuleCmd / CapsuleResp / H2CData / C2HData /
  R2T / C2HTermReq), per-connection state machine, fabrics command
  interception (Property Get / Set, Disconnect), fused-pair
  tracking, R2T flow control with partial-ICD + tail stitching,
  C2HData SUCCESS-bit optimization. Hands decoded SQEs through the
  `NvmeCommandHandler` trait so the transport is
  command-set-agnostic.

The design principle here — pushing concrete-type knowledge across a trait
boundary so the transport layer does not depend on how namespaces are owned —
mirrors the `core_stream::DriveTopology` lift on the tape side. The
dispatcher knows nothing about how namespaces are owned; the daemon's
`VolumeRegistry` impls `NamespaceLookup`.

## NSID convention

NVMe Base §6 reserves NSID 0 for the no-namespace and broadcast cases. VSA
maps `nsid = lun + 1` one-to-one with the SCSI LUN space:

| LUN | NSID |
| --- | ---- |
| 0   | 1    |
| 1   | 2    |
| ... | ...  |

`VolumeRegistry`'s `NamespaceLookup` impl in
`vsa/daemon/src/registry.rs` is the only place that knows about this offset.
Admin sockets, manifests, and the CLI continue to refer to LUNs.

## Per-connection state machine

The connection state machine is implemented in `nvme-tcp::server::serve_connection`.
There are three states that must be traversed in strict order; any deviation
closes the connection with a C2HTermReq carrying a Fatal Error Status:

1. **Initialization — ICReq → ICResp.** The first PDU on every accepted
   TCP connection must be ICReq (`PduType::ICReq`, 128 bytes — 8-byte
   common header + 120-byte payload). A `PFV != 0` is rejected with FES 0x01
   (Invalid PDU Header Field). The server honors whatever header / data
   digests the host requests in `ICReq.dgst` and echoes the agreed set in
   `ICResp.dgst` (see *Header / data digests* below); it never *requires*
   digests, so a host sending `dgst=0` keeps the no-digest fast path. It
   advertises `MAXH2CDATA = 128 KiB`. The host's MAXR2T is captured
   per connection and clamped to ≥ 1 per §3.6.1.
2. **Admission — Connect.** The first CapsuleCmd must be Admin Fabrics
   (`AdminOpcode::Fabrics`, opcode 0x7F) with FCTYPE at SQE byte 4 set to
   `FabricsType::Connect`. The in-capsule data is a 1024-byte
   `ConnectData` carrying HOSTID / requested CNTLID / SUBNQN / HOSTNQN.
   A SUBNQN mismatch causes a CapsuleResp with `connect_invalid_parameters`
   (SCT=CommandSpecific, SC=0x82, DNR=1) followed by close. On a match,
   the server binds the connection to a controller: QID=0 (admin queue,
   from CDW10[31:16]) creates a controller in the shared
   `ControllerRegistry` and is assigned a fresh CNTLID; QID>0 (I/O queue)
   attaches to the controller named in `ConnectData` CNTLID (the value
   the host got from its admin Connect), refused with
   `connect_invalid_parameters` if that CNTLID is unknown or owned by
   another HOSTID. The assigned CNTLID is echoed in the success
   CapsuleResp DW0[15:0].
3. **Steady state — command loop.** Every subsequent CapsuleCmd is
   routed by opcode class:
     - **Admin Fabrics (OPC=0x7F):** intercepted at the transport —
       Property Get / Set touch the shared `ControllerRegs`;
       Disconnect sends success and closes the TCP connection.
     - **Other opcodes:** direction derived from `OPC[1:0]` (NVMe
       Base §4.2.1) — `00` no transfer (dispatch immediately), `01`
       host-to-controller (see *Write data flow*), `10`
       controller-to-host (dispatch immediately; non-empty `data_in`
       sent as one C2HData PDU, SUCCESS bit folds the CQE into the
       C2HData when the CQE is success with `DW0 = DW1 = 0`), `11`
       bidirectional (refused with C2HTermReq).
   H2CTermReq closes silently; an unknown PDU type or protocol
   violation triggers C2HTermReq with FES (0x01 Invalid PDU Header
   Field / 0x02 PDU Sequence Error / 0x07 Invalid PDU Header Type).

   Every completion CapsuleResp the writer emits in this phase stamps the
   connection's QID into the CQE SQID field, so the data path, AER, and
   the intercepted Property / Identify / probe completions all match the
   Connect Response and the auth phase (issue #72). The command-set layers
   (`nvme-nvm`, the fabrics handler) build CQEs with `SQID = 0` because a
   queue id is a transport concern they don't model; the per-connection
   `writer_task` — the one chokepoint every steady-state completion funnels
   through — overrides it. Cosmetic uniformity only: on NVMe/TCP each queue
   is its own connection and the host correlates by CID, never by SQID.

## Fabrics commands (post-Connect)

Property Get and Property Set operate against a per-controller
`Arc<ControllerRegs>` shared by every connection bound to the same subsystem.
The register coverage is:

| Offset | Register | Width | Direction | Notes                              |
| ------ | -------- | ----- | --------- | ---------------------------------- |
| 0x00   | CAP      | 8     | R         | Static (MQES=1024, TO=15s, CSS=NVM)|
| 0x08   | VS       | 4     | R         | 1.4.0 = `0x0001_0400`              |
| 0x14   | CC       | 4     | RW        | Host sets EN/SHN; we toggle CSTS   |
| 0x1C   | CSTS     | 4     | R         | RDY mirrors CC.EN; SHST mirrors SHN|
| 0x0C, 0x10, 0x20, 0x24..0x30 | INTMS/INTMC/NSSR/AQA/ASQ/ACQ | 4 | RW | Accepted as no-op (PCIe-only relevance) |

Disconnect emits a success CapsuleResp and then closes the TCP connection
from the controller side. Any in-flight pending fused half is reported as
`aborted_due_to_missing_fused` before the close. A second Connect on an
established queue is refused. Authentication Send / Receive (FCTYPE 0x05 /
0x06) drive the DH-HMAC-CHAP exchange in the pre-steady-state auth phase
(see *DH-HMAC-CHAP* below) and are refused here, post-Connect, in steady
state.

## Write data flow (ICD + R2T)

The total transfer length comes from the SGL Data Block descriptor at
`SQE.DPTR[8..12]` (every NVMe-oF SGL type carries the length there). The
command loop handles four shapes depending on how much data arrived in-capsule
versus how much the server must request via R2T:

| Shape                          | Server behavior                                                                                                                                          |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No data (`SGL length = 0`)     | Dispatch with `data_out = None`.                                                                                                                         |
| Fully in-capsule (`ICD == SGL`) | Zero-copy: dispatch borrowing directly from the CapsuleCmd's body.                                                                                      |
| Partial ICD + R2T tail (`0 < ICD < SGL`) | Allocate `Vec<u8>` of `SGL` bytes, copy ICD prefix, emit R2T for the remainder at offset `icd.len()` length `SGL - icd.len()`. H2CData payloads land at their absolute `DATAO`. |
| Pure R2T (`ICD == 0`)          | Emit one R2T with `TTAG=1` covering the whole transfer; assemble H2CData PDUs by their `DATAO`. Honors host's MAXR2T and the advertised MAXH2CDATA per-PDU cap. |

Before any of the four shapes above runs, the SGL data length is
checked against the advertised MDTS ceiling (`MAX_TRANSFER_BYTES` =
1 MiB; MDTS=8 at the 4 KiB MPSMIN page). A command declaring more is
aborted with `Invalid Field in Command` *before* the `Vec<u8>` of
`SGL` bytes is allocated — otherwise a single CapsuleCmd could declare
a 4 GiB transfer and drive a memory-amplification allocation. Keep
`MAX_TRANSFER_BYTES` (nvme-tcp) and the `IdentifyController::mdts`
default (nvme-base) in lockstep.

Protocol violations during R2T fulfillment — CCCID/TTAG mismatch,
`DATAO+DATAL` overrun beyond the R2T window, wrong PDU type — close the
connection with C2HTermReq + FES 0x01 / 0x02. The R2T loop performs no
admission check beyond byte accounting.

## Fused Compare+Write (NVM Command Set §3.2.5)

The host submits two adjacent SQEs on the same I/O queue: Compare with
`FUSE = 01`, then Write with `FUSE = 10`. They share the same NSID, SLBA,
and NLB; the controller treats them as an atomic compare-and-swap.

Transport behavior:

- `FUSE = 01` (first half): the transport stashes
  `(sqe, assembled_data_out)` in a per-connection slot and emits no
  CQE.
- `FUSE = 10` (second half): the transport pulls the pending first
  half, assembles the write's data via ICD or R2T, and calls
  `NvmeCommandHandler::handle_fused_compare_write`. Two CQEs come
  back (one per CID — NVMe Base §4.2.6) and are written in order.
- Any non-fused or fabrics CapsuleCmd arriving while a fused-first
  is pending aborts the orphan with
  `aborted_due_to_missing_fused` (SC=0x0B, Generic, DNR=1) before
  processing the new command.

`NvmeNvmDispatcher::handle_fused_compare_write` routes through
`PageCache::compare_and_write_bytes`:

| Outcome              | Compare CQE                                  | Write CQE                                          |
| -------------------- | -------------------------------------------- | -------------------------------------------------- |
| Compare matches      | Success                                      | Success                                            |
| Compare mismatches   | `compare_failure` (SCT=Media, SC=0x85, DNR=1)| `aborted_due_to_failed_fused` (SC=0x0A, Generic)   |
| Internal error       | `internal_error` (SC=0x06)                   | `aborted_due_to_failed_fused`                      |

## NVM Command Set opcode mapping

| Opcode (NVM)              | Code | Routes to                                                |
| ------------------------- | ---- | -------------------------------------------------------- |
| Flush                     | 0x00 | `PageCache::synchronize_bytes(0, size_bytes)`            |
| Write                     | 0x01 | `PageCache::write_bytes(slba * lba, data_out)`           |
| Read                      | 0x02 | `PageCache::read_bytes(slba * lba, nlb * lba)`           |
| Compare                   | 0x05 | `read_bytes` + dispatcher-side byte equality             |
| Write Zeroes              | 0x08 | `write_bytes(... &vec![0; nlb * lba])`                   |
| Dataset Management (AD=1) | 0x09 | per-range `PageCache::unmap_bytes(off, len)`             |
| Verify                    | 0x0C | `read_bytes` (payload discarded)                         |
| Compare + Write fused     | 0x05 + 0x01 with FUSE bits | `PageCache::compare_and_write_bytes(...)` |

## Admin command coverage

Implemented:

| Admin opcode      | Code | Notes                                                                              |
| ----------------- | ---- | ---------------------------------------------------------------------------------- |
| Get Log Page      | 0x02 | LID 0x01 Error Info, 0x02 SMART/Health (temperature + spare), 0x03 FW Slot, 0x80 Reservation Notification (one 64-byte entry per call, oldest-first) |
| Identify          | 0x06 | CNS 0x00 Namespace, 0x01 Controller (NN from live registry, KAS=120 = 12 s), 0x02 Active List, 0x03 NS ID Descriptor List (NGUID + CSI=NVM), 0x06 I/O Command Set Identify Controller (4 KiB of zeros) |
| Abort             | 0x08 | Returns DW0 bit 0 = 1 (command already complete) — we don't queue at dispatcher    |
| Set Features      | 0x09 | FID 0x07 Number of Queues — clamps to internal cap (64), echoes granted count. FID 0x0F Keep Alive Timer — stores host-negotiated KATO in ms, no watchdog. FID 0x82 Reservation Notification Mask — per (HOSTID, NSID), echoes stored value |
| Get Features      | 0x0A | FID 0x07 Number of Queues, FID 0x0F Keep Alive Timer, FID 0x82 Reservation Notification Mask |
| Async Event Req.  | 0x0C | Parked by the transport until a reservation event fires; completes with DW0 = 0x00800006 (LID 0x80). See *Reservation notifications* below |
| Keep Alive        | 0x18 | No-op success                                                                      |
| Fabrics           | 0x7F | Connect / Property Get / Property Set / Disconnect — handled in the transport      |

Create / Delete I/O SQ / CQ are PCIe-only; in fabrics, queue creation
happens via Connect on a new TCP connection, so we return `Invalid
Command Opcode` there.

### Reservation notifications (AER + LID 0x80)

AER (Admin 0x0C) is the proactive-fencing path: a host fenced by
another host's Preempt / Release / Clear learns via an async event
instead of only reactively via `Reservation Conflict` (0x83) on its
next command. The pieces:

- **`nvme_nvm::ControllerRegistry`** — the per-subsystem controller
  registry + AER hub, constructed once at boot and shared (one `Arc`)
  between the dispatcher and the transport, the same way `ControllerRegs`
  is shared. It allocates CNTLIDs at Connect and holds the per-controller
  (keyed by CNTLID) runtime state: parked AER completion senders, the
  unread LID 0x80 queue, and the FID 0x82 masks. The controller — and all
  this state — is freed when its last association drops; the host's
  reservation registration is separate (HOSTID-keyed, persists).
- **Event source** — the shared `ReservationManager` itself computes the
  before/after diff (`scsi_spc::reservations::diff_reservation_changes`,
  issuer host excluded) on each mutating op and fires every registered
  `ReservationObserver`. `nvme_nvm::AerReservationSink` is the NVMe
  observer: it self-filters to NVMe registrants and feeds each affected
  host to `ControllerRegistry::notify`, which fans the event out to every
  live controller of that host. Because the diff lives in the manager and
  the sink fires regardless of which transport issued the change (#67), an
  iSCSI-issued preempt now reaches a fenced NVMe host — see *Cross-protocol
  reservation coherence* above.
- **Parking** — the transport's `reader_task` intercepts AER on the
  admin queue (qid 0), creates a `oneshot`, parks it on the controller,
  and spawns a task that awaits the completion and emits the CQE on the
  connection's writer. AER bypasses `handle_command` because it never
  completes synchronously. On connection teardown `disconnect` releases
  the parked senders and frees the controller once idle.
- **Delivery** — `notify` appends a LID 0x80 entry to each target
  controller and completes one of its parked AERs (DW0 `0x00800006`).
  The host reads `nvme resv-notif-log` (one entry per call, oldest-first)
  until the page is empty.

Routing is per-controller (keyed by CNTLID); the reservation state stays
HOSTID-keyed. A host with no live controller drops the event and learns
the new state via Reservation Report on reconnect.

## NQN / discovery

The subsystem NQN defaults to the `shared-naming` per-application identity:

```rust
shared_naming::DISK.nqn   // "nqn.2025-10.com.metebalci:thurvsa"
```

Operators override it with `nvmetcp.subnqn` in `thurvsa.yaml`; the resolved
value is validated at startup (`nqn.` prefix, ASCII, ≤ 223 chars) and feeds
the Connect SUBNQN admission check, the Identify Controller SUBNQN field, and
the TLS-PSK derivation.

### Discovery controller

A direct Discovery controller (NVMe-oF §1.5.7) lets hosts run `nvme discover` /
`nvme connect-all` instead of being handed the SUBNQN / address / port out of
band — the NVMe analog of the iSCSI SendTargets surface. It answers the
well-known NQN `nqn.2014-08.org.nvmexpress.discovery`.

Rather than multiplex two NQNs on one socket, the daemon binds a **second
listener** (default `0.0.0.0:8009`, the IANA discovery port; `nvmetcp.discovery`
in `thurvsa.yaml`, default on whenever `nvmetcp` is enabled) with a dedicated
`DiscoveryHandler` (`nvme-nvm`). Because the handler's `subnqn()` returns the
discovery NQN, the existing `nvme-tcp` Connect path admits the host and the whole
ICReq → Connect → Property Get/Set → command-loop machinery is reused unchanged.
The handler answers Identify Controller (CNTLTYPE = Discovery, byte 111 = 2) and
Get Log Page **LID 0x70** (the Discovery Log Page), and nothing else.

The Discovery Log Page lists the one I/O subsystem above: `TRTYPE=tcp`, the
subsystem NQN, `TRSVCID` = the I/O port (4420), and `TRADDR`. When the I/O
listener is bound to a concrete IP that address is advertised verbatim; for a
wildcard bind (`0.0.0.0` / `::`) the entry reflects the address each discovery
request actually landed on (the transport threads the connection's local address
into the handler), so a host that discovered via one interface gets a reachable
address back. `GENCTR` is a constant — `nvme discover` reads the page twice
(header to learn `NUMREC`, then the full page) and retries if it changes.

**Security.** The Discovery listener is deliberately cleartext and
unauthenticated — the spec/industry default (Linux, SPDK, and most arrays ship
unauthenticated discovery) and the analog of our unauthenticated iSCSI
SendTargets. The base spec permits this: the Discovery Log record carries `TREQ`
+ `TSAS.SECTYPE` precisely so an insecure discovery controller can advertise that
the *referenced* subsystem requires TLS. We set `SECTYPE = TLS 1.3` /
`TREQ = required` when `nvmetcp.tls.mode: psk`, so the host uses TLS for the real
Connect, and `NONE` / `not required` otherwise. No volume names leak at discovery
— per-volume admission stays at the I/O-subsystem Connect, whose
Active-Namespace-List is already admission-fenced (TLS-PSK / DH-HMAC-CHAP). A
hardened deployment that wants discovery itself secured (it is MITM-able, which
is why NVMe 2.0 added the Centralized Discovery Controller) can set
`nvmetcp.discovery.enabled: false` and distribute the address out of band as
before.

## Testing

Two layers, in increasing order of prerequisites:

- **In-process loopback tests** in `nvme-tcp/src/server.rs` — spawn
  the server on `127.0.0.1:0`, drive it with a `TcpStream` + a stub
  `NvmeCommandHandler`. Exercise the full PDU codec, the three-phase
  state machine, R2T flow (single + multi-PDU + partial ICD),
  Property Get / Set / Disconnect, SUBNQN admission, term-req on
  protocol violations, SUCCESS-bit folding. Run via
  `cargo test -p nvme-tcp`. No sudo, no kernel module.
- **`vsa/scripts/test-proto-nvmetcp.sh`** — live stack test
  driven by Linux `nvme-cli`. Self-elevates via sudo, loads
  `nvme_tcp` if needed, brings the daemon up with
  `transports: [nvmetcp]` on a free ephemeral port, runs
  `nvme connect` / `id-ctrl` / `id-ns` / `smart-log` / 10 MiB `dd`
  write+read with SHA-256 compare / `nvme disconnect`. Prereqs:
  `nvme-cli` + kernel ≥ 5.0. Pass `--tls` to exercise the TLS-PSK
  path (additional prereqs: `keyctl`, a running `tlshd`).
- **`vsa/scripts/test-dual-transport.sh`** — issue #66 acceptance:
  brings the daemon up with `transports: [iscsi, nvmetcp]` and one
  volume, connects over both (`iscsiadm` + `nvme connect`), checks
  cross-transport data coherence, then asserts a Write Exclusive
  reservation taken over either transport fences the other (both
  directions), with `nvme resv-report` / `sg_persist
  --read-reservation` reporting the same holder. Self-elevates via
  sudo. Prereqs: `open-iscsi`, `sg3-utils`, `lsscsi`, `nvme-cli`,
  `nvme_tcp`.

## TLS-PSK (NVMe-TCP §3.6.1.5)

TLS-PSK adds opt-in TLS 1.3 with pre-shared keys over the entire NVMe/TCP
session. The mode is selected in YAML:

```yaml
nvmetcp:
  tls:
    mode: psk      # disabled (default) | psk
    # identity_file: "/etc/thurvsa/nvmetcp-psks.json"
```

When enabled, every accepted TCP connection is wrapped in TLS **before** the
NVMe ICReq/Connect handshake runs — clients reach the PDU codec only after a
successful TLS handshake. The two NVMe-TCP §3.6.1.5 mandated cipher suites are
advertised (`TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`); TLS 1.2
fallback is refused.

### Two PSK strings (don't conflate)

There are two distinct PSK string formats that operators encounter; mixing
them up causes hard-to-diagnose handshake failures.

- **Interchange format** (operator config / `nvme-cli` keyring):
  `NVMeTLSkey-X:NN:<base64(key || crc32_le(key))>:`
  - `X` — version (`1` is what `nvme-cli` emits today)
  - `NN` — `01` (SHA-256) or `02` (SHA-384), matches a cipher suite
  - Generated host-side via
    `nvme gen-tls-key --hostnqn=H --subsysnqn=S --hmac=1 --identity=1`.
- **PSK identity on the wire** (in the TLS 1.3 `pre_shared_key`
  extension): `NVMe<ver>R<hash> <hostnqn> <subnqn> [<digest>]`
  - v0 omits the digest; v1 includes an HMAC binding the PSK to
    `hostnqn + subnqn`. Both are accepted — VSA derives the TLS PSK
    for both forms on every handshake and appends both, so s2n-tls's
    identity match wins regardless of which one the host emits.

### Identity file

The identity file lives at `<data_dir>/nvmetcp-psks.json` (mode 0640), with
one entry per host NQN. It is editable via
`thurvsa nvmetcp psks {add,remove,disable,enable,rotate,list}` or by
hand — the daemon reads the file on every TLS handshake, so edits take effect
on the next session with no restart or reload.

```json
{
  "version": 1,
  "psks": [
    {
      "host_nqn": "nqn.2014-08.org.nvmexpress:uuid:...",
      "interchange_key": "NVMeTLSkey-1:01:...."
    }
  ]
}
```

Optional per-entry fields:
- `"disabled": true` — entry kept for audit-history continuity but
  skipped at handshake (host fails TLS).
- `"previous_interchange_key"` + `"previous_expires_at"` — rotation
  grace window. Both keys derive their (v0, v1) PSK pairs while
  `previous_expires_at` is in the future; only the current key
  after. Set via `nvmetcp psks rotate --grace D`; revertible during
  the window via `nvmetcp psks rotate --cancel`.

Duplicate `host_nqn` entries are rejected; empty `host_nqn` is
rejected.

### Derivation

Per `nvme-tls(8)`:

```text
PRK     = HKDF-Extract(salt = <hash_len zero bytes>, IKM = RetainedPSK)
TLS PSK = HKDF-Expand-Label(PRK, "nvme-tls-psk",
                            context = PskIdentity, L = hash_len)
```

`RetainedPSK` is the base64-decoded key bytes from the interchange string
(after CRC-32 validation). `HKDF-Expand-Label` is the TLS 1.3 construction
(RFC 8446 §7.1). The full pipeline is unit-tested against RFC 5869 Test Case 1
and round-trip fixtures in `nvme/tcp/src/tls.rs`.

### Defense-in-depth cross-check

After the TLS handshake completes, the server reads the negotiated PSK
identity, parses the host-NQN field, and stashes it on the connection. When
the NVMe Connect command arrives, the host NQN it claims is compared against
the TLS-bound one. A mismatch — where the host authenticated as one identity
but is now claiming another — fails Connect with `connect_invalid_parameters`
and tears the session down. The check is silent in cleartext mode.

### Implementation

s2n-tls is the TLS stack used here, because it is the only Rust TLS library
that exposes a server-side TLS 1.3 external-PSK API (rustls has none;
openssl-rs exposes only the TLS 1.2 PSK callback). The integration sits behind
`nvme-tcp::server::accept_loop` so the PDU codec and dispatch path are
unchanged.

Per-handshake flow: the s2n-tls `ClientHelloCallback` registered on the
acceptor's `Config` reads `nvmetcp-psks.json` fresh, derives every PSK
(v0 + v1 identities × current + optional previous-key during grace), and
calls `connection.append_psk(...)` for each. s2n-tls then matches the
client-sent identity against the just-appended set. The cost per handshake is
one file read (sub-KB, page-cache-hot) plus N HKDFs plus 2N..4N
`append_psk` calls, where N is the number of registered hosts.

`build_psk_acceptor` parses the file once before opening the listener and
refuses to start if it is malformed or contains an invalid `interchange_key`.
Per-handshake parse failures — for example if the file is edited to a corrupt
state mid-run — fail that single handshake; the daemon keeps running and
previously-good PSKs remain good.

## DH-HMAC-CHAP (NVMe Base §8.13)

DH-HMAC-CHAP is in-band host authentication — the NVMe analog of the iSCSI CHAP
the iSCSI transport already speaks, and the common choice for Linux NVMe/TCP on
trusted networks (`nvme connect --dhchap-secret`). It authenticates the host
*without* a TLS stack, and is orthogonal to TLS-PSK: enable it alone over plain
TCP, or layer it inside a TLS-PSK channel ("dhchap+tls"). It is controlled by a
separate config block from `tls`:

```yaml
nvmetcp:
  auth:
    mode: dhchap          # none (default) | dhchap
    # identity_file: <data_dir>/nvmetcp-dhchap.json
```

The issue's three postures map to `(auth=none)` = off, `(auth=dhchap,
tls=disabled)` = dhchap-only, `(auth=dhchap, tls=psk)` = dhchap+tls.

### Where it sits

When `auth.mode = dhchap`, every Connect Response asserts **ATR** (Authentication
Transaction Required, DW0 bit 17 — `NVME_CONNECT_AUTHREQ_ATR`, not bit 16). Per
NVMe-oF the host must then complete the authentication exchange before
any other command — on every queue association (admin queue and each I/O
queue, since each is its own TCP connection). The exchange is strictly
serialized, so it runs as a dedicated phase **between the Connect Response and
the State-3 reader/writer split** in `serve_connection`
(`nvme-tcp::server::run_auth_phase`), reusing the same simple read/write style
as the Connect phase. The wire (de)serialization lives in `nvme-base::auth`; the
HMAC + Diffie-Hellman crypto in `nvme-tcp::auth` (OpenSSL, the same backend the
TLS-PSK HKDF uses).

### Message flow

```text
  host -> Negotiate    (Auth Send 0x05)      offered hashes + DH groups, sc_c
  ctrl -> Challenge    (Auth Receive 0x06)   chosen hash/group, C1, S1, g^x
  host -> Reply        (Auth Send 0x05)      R1, optional C2+g^y, S2
  ctrl -> Success1     (Auth Receive 0x06)   optional R2 (mutual auth)
  host -> Success2     (Auth Send 0x05)      acknowledgement
```

Authentication Send carries the host message as in-capsule data; Authentication
Receive returns the controller message as a C2HData PDU followed by a
CapsuleResp. An in-band failure (wrong secret, no entry, unusable hash/group)
is delivered as an `AUTH_Failure` message on the next Authentication Receive,
then the connection is closed — the Fabrics commands themselves still complete
with a success CQE. Each auth-phase CapsuleResp echoes the queue's QID in its
SQID field (consistent with the Connect Response, so I/O-queue auth matches the
admin queue). The one case where an Authentication Receive does *not* complete
with success is a host whose advertised Allocation Length (CDW11) cannot hold
the controller message: rather than over-send, the command is failed with
Invalid Field in Command. No conformant host trips this — the Challenge is at
most ~1.1 KiB (FFDHE-8192) against an AL sized near IOCCSZ — so it is purely
conformance hardening.

### Negotiation + crypto

- **Hash:** SHA-256 / 384 / 512. The controller picks the strongest the host
  offers.
- **DH group:** NULL or RFC 7919 FFDHE ffdhe2048 / 3072 / 4096 / 6144 / 8192.
  The controller picks the strongest FFDHE offered, else NULL. With a DH group,
  the controller generates an ephemeral keypair, and the challenge fed to the
  response HMAC is the *augmented* challenge `HMAC(H(g^xy), C)`. The shared
  secret is MSB-zero-padded to the prime length before hashing — matching the
  kernel's `crypto_kpp_maxsize` buffer — so the session key is byte-identical.
  The two heavy modular exponentiations (ephemeral keygen and the session-key
  derivation, plus the reply HMACs that ride with it) run on a `spawn_blocking`
  thread so a connection flood negotiating a large group cannot stall the async
  reactor — the auth-phase timeout still bounds the per-connection cost.
- **Response:** `HMAC_K(challenge ‖ seqnum(LE32) ‖ t_id(LE16) ‖ sc_c ‖ label ‖
  nqn_a ‖ 0x00 ‖ nqn_b)`, where `K` is the NQN-transformed secret. `label` is
  `"HostHost"` with `(hostnqn, subnqn)` for the host's R1; `"Controller"` with
  `(subnqn, hostnqn)` for the controller's R2. Byte-exact layout in
  [`SPEC.md`](SPEC.md) § DH-HMAC-CHAP authentication.

### Secrets, admission, bidirectional

Per-host secrets live in `<data_dir>/nvmetcp-dhchap.json` (daemon-managed,
0640, atomic save), re-read on every Connect — `thurvsa nvmetcp dhchap` edits
take effect on the next session with no restart. Each entry carries a `volumes`
allow-list; under `auth.mode = dhchap` admission is **mandatory** and gates
every I/O command (same model as TLS-PSK admission). A `dhchap_ctrl_key`
enables **bidirectional** auth: when the host sends a challenge
(`--dhchap-ctrl-secret`), the controller proves itself with R2 in Success1; a
host requesting mutual auth without a configured controller secret is failed.
Secrets rotate with a grace window (`previous_dhchap_key` +
`previous_expires_at`) — both authenticate while the window is open. Operator
surface + secret-store schema in [`NETWORK_SECURITY.md`](../admin/NETWORK_SECURITY.md) § NVMe/TCP DH-HMAC-CHAP.

Because `nvmetcp-dhchap.json` and `nvmetcp-psks.json` are structurally the
same rotatable per-host record, the daemon's `nvmetcp dhchap` and `nvmetcp
psks` admin verbs (add / remove / disable / enable / rotate / rotate-cancel /
grant / revoke) are one generic implementation parameterized on a per-surface
`Surface` trait — `vsa/daemon/src/admin/nvmetcp_host_file.rs` — so a fix to the
rotation grace state machine lands on both surfaces at once. The on-disk
records expose that common shape through the `HostCredentialEntry` /
`HostCredentialFile` traits in `nvme-tcp::identity`, and the two secret parsers
(`NVMeTLSkey-...` and `DHHC-1:...`) share one base64 + CRC32-LE-tail validation
core. The DH-HMAC-CHAP-only `set-ctrl-key` / `clear-ctrl-key` verbs and each
surface's `list` envelope stay surface-specific.

## Header / data digests (CRC32C, NVMe/TCP §3.4)

The handshake honors the header (`HDGSTF`) and data (`DDGSTF`) digests a host
requests in `ICReq.dgst`, echoing the agreed set in `ICResp.dgst`. The server
supports both and never requires either, so the choice is the host's: a host
sending `dgst=0` keeps the no-digest fast path (the codec returns each PDU
buffer untouched), while a host configured with `--hdr_digest` / `--data_digest`
gets fully digested framing in both directions. This closes the interop gap
where such a host would otherwise fail connection setup outright.

The digest is CRC32C (Castagnoli, polynomial 0x1EDC6F41 — the same as iSCSI),
transmitted little-endian, via the `crc32c` crate (hardware-accelerated on
x86 SSE4.2). When enabled, a 4-byte header digest over the HLEN header bytes
sits immediately after the header (the PDO bumps past it), and a 4-byte data
digest trails the payload. All of this is centralized: `pdu::apply_digests`
post-processes every outbound PDU, and `RawPdu::verify_header_digest` /
`verify_data_digest` check the inbound direction. The negotiated `DigestCfg`
threads from the handshake into the writer task (outbound) and reader task
(inbound), and through the Connect + DH-HMAC-CHAP auth PDUs in between.

Error handling follows §3.4: a header-digest mismatch is fatal (the header
can't be trusted to isolate the offending command, so the connection is torn
down with a C2HTermReq carrying FES 0x03, Header Digest Error), while a
data-digest mismatch fails just the owning command with Data Transfer Error
(Generic SC 0x04, DNR clear — the host may retry) and leaves the connection
up. There is no FES for data-digest errors precisely because they are meant to
be command-scoped. On an H2CData mismatch the per-command task drains the rest
of the transfer off the wire before completing in error, so trailing H2CData
PDUs don't strand as unknown-CCCID protocol violations.

CRC32C also appears elsewhere in the codebase for an unrelated purpose — SSC
LTO-7+ Logical Block Protection appends a per-tape-block CRC32C trailer
(`core/mediachanger/src/lbp.rs`); that is a stored data guard, distinct from
this transport-frame digest.

## Out of scope (with rationale)

The following features are intentionally not implemented in this stack. Each
entry describes why the current choice is correct and what would change the
calculation.

### Multi-outstanding R2T

The server issues exactly one R2T per write command and waits for the host to
fulfill it before processing the next command. The spec allows up to `MAXR2T`
concurrent R2Ts. Single-R2T is bandwidth-equivalent for any transfer that fits
the network's bandwidth-delay product; the round-trip only becomes the
bottleneck on a high-latency link. Lift this restriction if a benchmark
demonstrates that latency is the constraint.

### Async events other than reservation notifications

AER (Admin 0x0C) is implemented, but the only wired event source is reservation
notifications (see *Reservation notifications (AER + LID 0x80)* above). The
Notice-type events (namespace-attribute behind FID 0x0B Async Event
Configuration, firmware-activation) and thermal notices are not produced — VSA
has no firmware mechanism or thermal sensors, and namespaces are bound at
`volume create`. The generic AER plumbing (`ControllerRegistry`, the DW0 builder,
the park/notify/oneshot path) is reusable when a namespace-change source lands; it
would add OACS bit 8 + a Changed Namespace List (LID 0x04), out of scope here.

Reservation notifications **are** fanned out across transports as of #67: a
reservation taken/preempted over iSCSI raises an NVMe AER on affected NVMe
controllers, and an NVMe reservation change raises a SCSI Unit Attention on
affected iSCSI sessions. The transport-neutral `ReservationManager`
change-observer that makes this work is described under *Cross-protocol
reservation coherence* above; only the reservation-notification source is
shared here — the other AER event sources below remain unwired.

(The Discovery controller is now implemented — see *NQN / discovery* §
*Discovery controller* above. Still out of scope there: a Centralized Discovery
Controller and discovery-log-change AENs; the single-subsystem direct Discovery
controller covers the `nvme discover` / `nvme connect-all` use case.)

### Secure-channel concatenation (DH-HMAC-CHAP `sc_c` ≠ 0)

DH-HMAC-CHAP itself is implemented (see *DH-HMAC-CHAP* above); the one piece we
refuse is the secure-channel-concatenation variant, where the auth exchange
negotiates a TLS-PSK to insert for the rest of the connection (`sc_c` ≠ 0 in
the Negotiate). We reject it with `AUTH_Failure` (CONCAT_MISMATCH). Operators
who want an encrypted data stream use `tls.mode = psk` directly (optionally with
`auth.mode = dhchap` layered on top), which is simpler and the documented path.
