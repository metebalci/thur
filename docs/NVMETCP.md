# NVMe/TCP transport

VSA's data path is wire-protocol-agnostic above the `core-block` boundary.
Volumes always reduce to a per-volume `PageCache` over a content-addressed
`ChunkPool`; what changes between iSCSI and NVMe/TCP is only the framing and
the dispatcher that translates incoming commands into `PageCache` calls. That
clean separation is what makes it possible to add a second transport without
touching the storage engine at all.

This document is the NVMe/TCP design walkthrough: crate split, the
per-connection state machine, write data flow, fused Compare+Write, TLS-PSK,
testing, and the out-of-scope list. The per-opcode and per-feature compliance
table is in [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md); the iSCSI and SBC-3
path is in [`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md).

## Selecting the transport

`thurvsa.yaml` carries a top-level `transport:` enum:

```yaml
transport: iscsi      # default
# transport: nvmetcp
```

The two transports are mutually exclusive — only one listener binds. NVMe/TCP
defaults to the IANA-registered nvme-tcp port (`0.0.0.0:4420`); iSCSI keeps
`0.0.0.0:3260`. Both can be overridden via the per-transport `listen:` field:

```yaml
transport: nvmetcp
nvmetcp:
  listen: 0.0.0.0:4420
```

Because the default ports don't clash, a future concurrent-both-transports
mode would require no operator override.

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
   (Invalid PDU Header Field). The server negotiates `dgst=0` (no digests)
   and advertises `MAXH2CDATA = 128 KiB`. The host's MAXR2T is captured
   per connection and clamped to ≥ 1 per §3.6.1.
2. **Admission — Connect.** The first CapsuleCmd must be Admin Fabrics
   (`AdminOpcode::Fabrics`, opcode 0x7F) with FCTYPE at SQE byte 4 set to
   `FabricsType::Connect`. The in-capsule data is a 1024-byte
   `ConnectData` carrying HOSTID / requested CNTLID / SUBNQN / HOSTNQN.
   A SUBNQN mismatch causes a CapsuleResp with `connect_invalid_parameters`
   (SCT=CommandSpecific, SC=0x82, DNR=1) followed by close. On a match,
   the server assigns CNTLID=1, captures QID from CDW10[31:16], and emits
   a success CapsuleResp.
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
`aborted_due_to_missing_fused` before the close. Authentication Send /
Receive (FCTYPE 0x05 / 0x06) and a second Connect on an established queue
are both refused.

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
| Get Log Page      | 0x02 | LID 0x01 Error Info, 0x02 SMART/Health (temperature + spare), 0x03 FW Slot         |
| Identify          | 0x06 | CNS 0x00 Namespace, 0x01 Controller (NN from live registry, KAS=120 = 12 s), 0x02 Active List, 0x03 NS ID Descriptor List (NGUID + CSI=NVM), 0x06 I/O Command Set Identify Controller (4 KiB of zeros) |
| Abort             | 0x08 | Returns DW0 bit 0 = 1 (command already complete) — we don't queue at dispatcher    |
| Set Features      | 0x09 | FID 0x07 Number of Queues — clamps to internal cap (64), echoes granted count. FID 0x0F Keep Alive Timer — stores host-negotiated KATO in ms, no watchdog |
| Get Features      | 0x0A | FID 0x07 Number of Queues, FID 0x0F Keep Alive Timer                               |
| Keep Alive        | 0x18 | No-op success                                                                      |
| Fabrics           | 0x7F | Connect / Property Get / Property Set / Disconnect — handled in the transport      |

Async Event Request (AER, 0x0C) returns `Invalid Command Opcode` —
see *Out of scope (with rationale)*. Create / Delete I/O SQ / CQ are
PCIe-only; in fabrics, queue creation happens via Connect on a new
TCP connection, so we return `Invalid Command Opcode` there too.

## NQN / discovery

The subsystem NQN defaults to the `shared-naming` per-product identity:

```rust
shared_naming::DISK.nqn   // "nqn.2025-10.com.metebalci:thurvsa"
```

Operators override it with `nvmetcp.subnqn` in `thurvsa.yaml`; the resolved
value is validated at startup (`nqn.` prefix, ASCII, ≤ 223 chars) and feeds
the Connect SUBNQN admission check, the Identify Controller SUBNQN field, and
the TLS-PSK derivation.

A future Discovery service (with its own separate NQN, typically
`nqn.2014-08.org.nvmexpress.discovery`) is a layer above this. We do not ship
a discovery controller today; hosts connect directly to the subsystem NQN.

## Testing

Two layers, in increasing order of prerequisites:

- **In-process loopback tests** in `nvme-tcp/src/server.rs` — spawn
  the server on `127.0.0.1:0`, drive it with a `TcpStream` + a stub
  `NvmeCommandHandler`. Exercise the full PDU codec, the three-phase
  state machine, R2T flow (single + multi-PDU + partial ICD),
  Property Get / Set / Disconnect, SUBNQN admission, term-req on
  protocol violations, SUCCESS-bit folding. Run via
  `cargo test -p nvme-tcp`. No sudo, no kernel module.
- **`vsa/scripts/test-nvmetcp-conformance.sh`** — live stack test
  driven by Linux `nvme-cli`. Self-elevates via sudo, loads
  `nvme_tcp` if needed, brings the daemon up with
  `transport: nvmetcp` on a free ephemeral port, runs
  `nvme connect` / `id-ctrl` / `id-ns` / `smart-log` / 10 MiB `dd`
  write+read with SHA-256 compare / `nvme disconnect`. Prereqs:
  `nvme-cli` + kernel ≥ 5.0. Pass `--tls` to exercise the TLS-PSK
  path (additional prereqs: `keyctl`, a running `tlshd`).

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

### Out of scope here

- **DH-HMAC-CHAP** — the alternative auth mechanism for environments that
  cannot terminate TLS at the target. It is strictly worse than TLS-PSK for
  the typical case (auth encrypted, data stream not). Lands only on concrete
  request.

## Out of scope (with rationale)

The following features are intentionally not implemented in this stack. Each
entry describes why the current choice is correct and what would change the
calculation.

### Header / data digests (CRC32C, NVMe/TCP §3.5)

The handshake negotiates `dgst=0` and the codec rejects PDUs with DGSTF bits
set. TCP already provides a checksum, and the modern deployment story puts
TLS-PSK underneath — AEAD over the whole stream. Digest negotiation doubles
the codec surface for marginal benefit. Hosts that require digests will see
negotiation fail and should be pointed at a different target.

### Multi-outstanding R2T

The server issues exactly one R2T per write command and waits for the host to
fulfill it before processing the next command. The spec allows up to `MAXR2T`
concurrent R2Ts. Single-R2T is bandwidth-equivalent for any transfer that fits
the network's bandwidth-delay product; the round-trip only becomes the
bottleneck on a high-latency link. Lift this restriction if a benchmark
demonstrates that latency is the constraint.

### Async Event Request (Admin 0x0C)

Returns `Invalid Command Opcode`. VSA produces no async events today. Per
NVMe Base §5.2, AER has no timeout, but Linux nvme-tcp logs warnings during
bring-up if it does not see AERs complete; returning Invalid Opcode makes that
warning fire once instead of spinning. Wire this when a real async-event
trigger lands.

### Discovery controller

Hosts connect directly to the subsystem NQN; we do not ship a separate
discovery NQN (`nqn.2014-08.org.nvmexpress.discovery`). Operators distribute
the address, port, and NQN out of band. A discovery controller lands when
multi-subsystem deployments become a documented use case.

### DH-HMAC-CHAP (NVMe Base §8.13.5)

The non-TLS auth alternative — auth exchange encrypted, data stream not. Lands
only if a deployment cannot terminate TLS at the target (e.g. behind a fabric
appliance doing the TLS work).
