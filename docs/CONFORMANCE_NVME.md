# thurvsa Standards Conformance — NVMe / NVMe-oF / NVMe-TCP

This document covers per-spec compliance for thurvsa's NVMe-over-TCP block
target. thurvsa exposes one NVM Subsystem (NQN
`nqn.2025-10.com.metebalci:thurvsa`) over NVMe/TCP on port 4420 by default,
selected by listing `nvmetcp` in the daemon config's `transports:`. Volumes are
page-grained internally (default 64 KiB) and advertise 4 KiB LBAs to the
host. NSID maps one-to-one onto the volume → LUN registry as `nsid = lun + 1`
(NVMe Base reserves NSID 0 for broadcast / no namespace).

This document is the per-opcode / per-feature compliance table.
Related docs:

- thurvsa iSCSI / SCSI surface (SBC-3) →
  [`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md).
- End-to-end NVMe/TCP design (crate split, per-connection state
  machine, fused Compare+Write, R2T flow, TLS-PSK derivation) →
  [`NVMETCP.md`](NVMETCP.md).

iSCSI and NVMe/TCP can bind concurrently (`transports: [iscsi, nvmetcp]`,
issue #66) or singly. Both reduce to the same `PageCache` + `ChunkPool` below
the dispatcher boundary, so encryption, dedup, and backend upload behave
identically regardless of which transport the host used. When both are bound
they share one `scsi_spc::reservations::ReservationManager`, so reservations
fence across protocols — see *Reservation notifications* and the cross-protocol
note in [`NVMETCP.md`](NVMETCP.md).

**Targets:** NVMe Base 1.4, NVM Command Set 1.0, NVMe-oF 1.1, NVMe
TCP Transport 1.0a.

**Status legend:**

- **Yes** — fully implemented to spec.
- **Partial** — opcode / feature handled, but a subset of the spec
  (specific service actions, parameter values, fields, or modes only).
- **Stub** — opcode answered with a structurally-valid but
  hard-coded / all-zero response.
- **No-op** — opcode accepted (returns success) but produces no state
  change.
- **No** — not implemented; returns `Invalid Command Opcode`
  (SCT=Generic, SC=0x01) or `Invalid Field in Command` (SC=0x02).
- **N/A** — feature does not apply (PCIe-only on a fabrics-only
  target, etc.).

**Spec column:** **M** mandatory, **O** optional (a "No" against an O
entry is conformant), **CC** conditionally mandatory, **—**
vendor-specific / outside the listed standard.

**Status-cell color squares:** 🟩 implemented (Yes / Partial / Stub /
No-op / N/A); 🟨 not implemented and not required; 🟥 not implemented
and required.

---

## NVM Command Set — I/O queue opcodes

Commands an NVM-Command-Set namespace may receive on an I/O submission
queue are dispatched through `nvme_nvm::NvmeNvmDispatcher`, which routes
each opcode directly into the per-volume `core_block::PageCache`. The
PageCache is the same backing store that the SBC-3 dispatcher uses on
the iSCSI side, so data written over NVMe/TCP and data written over iSCSI
are indistinguishable at the chunk level.

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x00 | Flush | 🟩 Yes | M | `PageCache::synchronize_bytes(0, size_bytes)` — a real fence, awaits storage-backend ack of every dirty page. NSID 0xFFFFFFFF (broadcast) accepted on the live namespace; one namespace per `PageCache` so no per-controller global flush is needed. |
| 0x01 | Write | 🟩 Yes | M | `PageCache::write_bytes(slba * lba_bytes, data_out)`. Sub-page writes via cache RMW. WORM volumes refuse with a media-error status. |
| 0x02 | Read | 🟩 Yes | M | `PageCache::read_bytes(slba * lba_bytes, nlb * lba_bytes)`. Unallocated pages return zeros (sparse holes). |
| 0x04 | Write Uncorrectable | 🟨 No | O | No analog on a chunk-pool-backed virtual volume. Refused with `Invalid Command Opcode`. |
| 0x05 | Compare | 🟩 Yes | O | Reads the LBA range and compares against the host buffer at byte granularity. Mismatch returns `Compare Failure` (SCT=Media, SC=0x85, DNR=1). |
| 0x08 | Write Zeroes | 🟩 Yes | O | Lowered to `PageCache::write_bytes(..., &vec![0; nlb * lba_bytes])`. Sub-page zeroing via cache RMW. WORM refuses with a media-error status. |
| 0x09 | Dataset Management | 🟩 Partial | O | AD (Deallocate, CDW11 bit 2) only — each 16-byte range descriptor calls `PageCache::unmap_bytes(slba * lba_bytes, nlb * lba_bytes)`. Without AD, IDR / IDW hint passes return success as a no-op. |
| 0x0C | Verify | 🟩 Yes | O | Routes to `PageCache::read_bytes` and discards the payload — surfaces medium errors without returning data. Sparse-hole pages succeed. |
| 0x0D | Reservation Register | 🟩 Yes | O | RREGA Register / Unregister / Replace, with IEKEY + CPTPL. Backed by the shared `scsi_spc::reservations::ReservationManager` keyed by the 128-bit HOSTID from Fabrics Connect (`nvme_nvm::reservations`). CPTPL = "set PTPL" (0b11) sets the namespace PTPL state and persists the registration to `<data_dir>/reservations.json` before the command completes (persist-before-ack; a durable-write failure returns `Internal Error`); "clear PTPL" (0b10) clears it; "no change" (0b00) leaves it. PTPL is advertised (RESCAP bit 0 = 1). |
| 0x0E | Reservation Report | 🟩 Yes | O | Reservation Status Data Structure from a snapshot of the shared state. EDS (CDW11 bit 0) selects the extended 64-byte-per-controller form (full 128-bit HOSTID) vs the 24-byte form (low 64 bits). PTPLS (byte 9) reflects the namespace's current Persist Through Power Loss state. One registered-controller entry per HOSTID; CNTLID is the registrant's representative live controller (its lowest CNTLID), or `0` if the host has a persisted registration but no live controller. The fencing identity remains the HOSTID, not the CNTLID. |
| 0x11 | Reservation Acquire | 🟩 Yes | O | RACQA Acquire / Preempt / Preempt-and-Abort. RTYPE 1..6 maps to the six SCSI reservation types. Acquire from an unregistered host returns `Reservation Conflict`. Preempt and Preempt-and-Abort collapse (no task-manager hook), matching the SCSI side. |
| 0x15 | Reservation Release | 🟩 Yes | O | RRELA Release / Clear. A non-holder's data-path command (Read / Compare / Verify gated by `allow_read`; Write / Write Zeroes / DSM-deallocate / fused Compare+Write by `allow_write`) is rejected with `Reservation Conflict` (SCT=Generic, SC=0x83 — NVMe Base §4.6.1.2.1; the Linux nvme driver maps this to `BLK_STS_NEXUS` only when SCT=0). Flush is deliberately **not** gated (the NVM Command Set does not restrict it). |
| — | Fused Compare + Write (0x05 FUSE=01 → 0x01 FUSE=10) | 🟩 Yes | O | Both halves accumulated by the transport (`nvme-tcp::server`), atomic compare-and-swap via `NvmeCommandHandler::handle_fused_compare_write` → `PageCache::compare_and_write_bytes`. Two CQEs per NVMe Base §4.2.6. Mismatch returns `Compare Failure` on the Compare CQE + `Aborted due to failed fused` (SC=0x0A) on the Write CQE. Sub-LBA CAW (single-sector VMFS heartbeat) honored end-to-end. |

---

## Admin command set

Admin commands are routed through `NvmeNvmDispatcher::dispatch_admin`. The
opcodes in the 0x00–0x05 range are PCIe queue-management primitives — in a
fabrics context, queue creation happens via Connect on a new TCP connection
rather than via these commands, so a host that submits them receives
`Invalid Command Opcode`.

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x00 | Delete I/O Submission Queue | 🟩 N/A | M (PCIe) | PCIe-only; fabrics uses Disconnect. Returns `Invalid Command Opcode`. |
| 0x01 | Create I/O Submission Queue | 🟩 N/A | M (PCIe) | PCIe-only; fabrics uses Connect on a new TCP connection. |
| 0x02 | Get Log Page | 🟩 Partial | M | LIDs 0x01 Error Information (zero-entry), 0x02 SMART / Health (composite temperature + spare capacity, both static), 0x03 Firmware Slot Information (single slot, slot 1 active, FR mirrors Identify Controller), 0x80 Reservation Notification (one 64-byte entry per call, oldest-first; empty page when the host's queue is drained — see *Reservation notifications* below). Other LIDs return `Invalid Field in Command`. |
| 0x04 | Delete I/O Completion Queue | 🟩 N/A | M (PCIe) | PCIe-only. |
| 0x05 | Create I/O Completion Queue | 🟩 N/A | M (PCIe) | PCIe-only. |
| 0x06 | Identify | 🟩 Partial | M | See CNS table below. |
| 0x08 | Abort | 🟩 Stub | O | Returns CQE.DW0 bit 0 = 1 (command not aborted / already complete). The dispatcher does not queue commands at the controller level. |
| 0x09 | Set Features | 🟩 Partial | M | See FID table below. |
| 0x0A | Get Features | 🟩 Partial | M | See FID table below. |
| 0x0C | Async Event Request | 🟩 Partial | O | Parked on the admin queue by the transport (not the dispatcher) until a controller event fires; completes with DW0 = `0x00800006` (AET=0x6 I/O Command Set specific, AEI=0x00 Reservation Log Page Available, LID=0x80) pointing the host at the Reservation Notification log. The only event source today is reservations; namespace-change / firmware / thermal notices are not produced. See *Reservation notifications* below. |
| 0x10 | Firmware Commit | 🟨 No | O | No firmware-download surface — the daemon ships as a binary, not a flashable image. |
| 0x11 | Firmware Image Download | 🟨 No | O | As above. |
| 0x14 | Device Self-test | 🟨 No | O | No self-test surface. |
| 0x15 | Namespace Attachment | 🟨 No | O | Namespaces are bound 1:1 to volumes at `volume create`; no host-driven attach surface. |
| 0x18 | Keep Alive | 🟩 Yes | M | Unconditionally acknowledged. No watchdog; KATO is captured by Set Features (FID 0x0F) but not enforced. Identify Controller advertises KAS=120 (12 s) to satisfy the fabrics keep-alive requirement. |
| 0x1A | Directive Send | 🟨 No | O | Not implemented. |
| 0x1B | Directive Receive | 🟨 No | O | Not implemented. |
| 0x1C | Virtualization Management | 🟨 No | O | Not implemented. |
| 0x1D | NVMe-MI Send | 🟨 No | — | Out-of-band management not in scope. |
| 0x1E | NVMe-MI Receive | 🟨 No | — | As above. |
| 0x7C | Doorbell Buffer Config | 🟩 N/A | O (PCIe) | PCIe-only. |
| 0x7F | Fabrics | 🟩 Yes | M (fabrics) | Intercepted at the transport (`nvme-tcp::server`), routed by FCTYPE — see fabrics table below. |
| 0x80 | Format NVM | 🟨 No | O | LBA format is fixed at `volume create`. |
| 0x81 | Security Send | 🟨 No | O | No NVMe-side security protocol surface; at-rest encryption is keystore-driven below the dispatcher boundary. |
| 0x82 | Security Receive | 🟨 No | O | As above. |
| 0x84 | Sanitize | 🟨 No | O | The chunk-pool model has no in-place sanitize. |

### Identify (Admin 0x06) CNS values

| CNS | Name | Status | Notes |
|----:|------|--------|-------|
| 0x00 | Identify Namespace | 🟩 Yes | NSZE / NCAP / NUSE in 4 KiB-LBA units; LBAF[0] = 4 KiB sector; NSFEAT bit 0 (thin provisioning) set; RESCAP (byte 31) = `0x7F` (all six reservation types + PTPL bit 0; the daemon persists reservation state across power loss — bit 0 is cleared to `0x7E` only if the manager is built without a data dir, which never happens in a real install); VWC bit 0 set (so Linux issues Flush). |
| 0x01 | Identify Controller | 🟩 Yes | VID=0 / SSVID=0 (fabrics-only), CNTLID = the per-controller ID assigned at Connect (distinct per association), CMIC (byte 76) bit 1 set (subsystem may contain two or more controllers; bits 0/3 clear — single port, no ANA), VER=`0x00010400` (NVMe 1.4.0), NN from live registry, KAS=120 (12 s), MDTS=8 (1 MiB max transfer at the 4 KiB MPSMIN page), ONCS (bytes 520..522) bit 5 set (Reservations supported), SUBNQN=`nqn.2025-10.com.metebalci:thurvsa`, SGLS bit 0 set, IOCCSZ=1028 / IORCSZ=1. |
| 0x02 | Active Namespace ID List | 🟩 Yes | Up to 1024 u32 NSIDs greater than `SQE.NSID`, zero-padded. |
| 0x03 | Namespace ID Descriptor List | 🟩 Yes | Two descriptors: NIDT=0x02 NGUID (from volume UUID), then NIDT=0x04 CSI=0x00 (NVM). Linux nvme-tcp issues this right after CNS 0x00 and silently fails namespace attach without a CSI descriptor. |
| 0x06 | I/O Command Set specific Identify Controller | 🟩 Yes | 4 KiB of zeros = "no specific NVM limits." Linux nvme-tcp issues this against a 1.4-versioned controller during bring-up; refusing it kills the namespace attach. |
| other | Controller list, NS management lists, NS granularity, UUID list, etc. | 🟨 No | Return `Invalid Field in Command`. |

### Get / Set Features (Admin 0x09 / 0x0A) FIDs

| FID | Name | Status | Notes |
|----:|------|--------|-------|
| 0x01 | Arbitration | 🟨 No | Not implemented. |
| 0x02 | Power Management | 🟨 No | No power-state model. |
| 0x04 | Temperature Threshold | 🟨 No | No thermal model. |
| 0x05 | Error Recovery | 🟨 No | |
| 0x06 | Volatile Write Cache | 🟨 No | VSA exposes a write-back cache that cannot be disabled at runtime. |
| 0x07 | Number of Queues | 🟩 Yes | Set / Get round-trip. Granted = min(host-requested, internal cap = 64) for both NSQ and NCQ; echoed in CQE.DW0. |
| 0x08 | Interrupt Coalescing | 🟩 N/A | PCIe-only (no MSI-X on fabrics). |
| 0x09 | Interrupt Vector Configuration | 🟩 N/A | PCIe-only. |
| 0x0A | Write Atomicity Normal | 🟨 No | |
| 0x0B | Async Event Configuration | 🟨 No | Gates *Notice*-type async events (namespace-attribute / firmware-activation), which VSA does not produce — distinct from the reservation-notification mask (FID 0x82 below), which is implemented. Returns `Invalid Field in Command`. |
| 0x0F | Keep Alive Timer | 🟩 Partial | KATO captured on Set, echoed on Get. No watchdog — KA admin commands are unconditionally acknowledged — so the value is stored only for symmetry. |
| 0x10 | Host Identifier | 🟨 No | Host identity is captured at Connect via HOSTID in `ConnectData`. |
| 0x82 | Reservation Notification Mask | 🟩 Yes | Per-namespace (CDW1 NSID). CDW11 bits 1 / 2 / 3 suppress Registration Preempted / Reservation Released / Reservation Preempted notifications respectively (0 = all enabled). Stored keyed by (HOSTID, NSID) so one host's masking cannot silence another's; Set echoes the stored value in CQE.DW0, Get returns it. The host's enable/disable knob for reservation async events. |

Get / Set Features for unknown FIDs return `Invalid Field in
Command`.

---

## Fabrics commands (Admin 0x7F)

Fabrics commands are identified by their FCTYPE field in `SQE.NSID & 0xFF`
and are intercepted at the transport layer (`nvme-tcp::server`) before the
admin dispatcher ever sees them. This interception is the right boundary:
Connect, Disconnect, and Property Get/Set have meaning to the transport
state machine itself, not just to the namespace or controller abstraction
above it.

| FCTYPE | Name | Status | Spec | Notes |
|-------:|------|--------|:----:|-------|
| 0x00 | Property Set | 🟩 Yes | M | Writes the shared `ControllerRegs`; INTMS / INTMC / NSSR / AQA / ASQ / ACQ accepted as no-op. |
| 0x01 | Connect | 🟩 Yes | M | First CapsuleCmd on every TCP connection must be Connect. 1024-byte in-capsule `ConnectData` carries HOSTID + requested CNTLID + SUBNQN + HOSTNQN. SUBNQN mismatch returns `Connect Invalid Parameters` (SCT=CommandSpecific, SC=0x82, DNR=1). QID from CDW10[31:16] selects the queue type: QID=0 (admin) creates a controller and is assigned a fresh CNTLID; QID>0 (I/O) attaches to the controller named in `ConnectData` CNTLID, refused with `Connect Invalid Parameters` if that CNTLID has no live controller or belongs to another HOSTID. The assigned CNTLID is echoed in the success CapsuleResp DW0. A second Connect on an established queue is refused. |
| 0x04 | Property Get | 🟩 Yes | M | Reads the shared `ControllerRegs`. |
| 0x05 | Authentication Send | 🟨 No | CC | DH-HMAC-CHAP not implemented — see *Deliberate non-conformance*. Returns `Invalid Field in Command`. |
| 0x06 | Authentication Receive | 🟨 No | CC | As above. |
| 0x08 | Disconnect | 🟩 Yes | M | Sends a success CapsuleResp then closes the TCP connection from the controller side. Any in-flight pending fused-first half is reported as `aborted_due_to_missing_fused` before close. |

### Controller registers (touched via Property Get / Set)

| Offset | Register | Width | Direction | Notes |
|-------:|----------|------:|:---------:|-------|
| 0x00   | CAP      | 8     | R         | Static: MQES=1024, TO=15 s, CSS=NVM. |
| 0x08   | VS       | 4     | R         | `0x0001_0400` (NVMe 1.4.0). |
| 0x14   | CC       | 4     | RW        | Host sets EN / SHN; CSTS is toggled in response. |
| 0x1C   | CSTS     | 4     | R         | RDY mirrors CC.EN; SHST mirrors SHN. |
| 0x0C, 0x10, 0x20, 0x24..0x30 | INTMS / INTMC / NSSR / AQA / ASQ / ACQ | 4 | RW | Accepted as no-op (PCIe-only relevance). |

---

## NVMe-TCP transport — PDU types

The PDU codec lives in `nvme-tcp::pdu`; the per-connection state machine
lives in `nvme-tcp::server`. Every accepted TCP connection progresses
through three states in strict order — Initialization → Admission →
Steady — and any deviation from that sequence closes the connection
immediately with a C2HTermReq carrying a Fatal Error Status. This design
means that a misbehaving initiator is always handled at the boundary where
the violation occurs, without leaking partial state upward into the
dispatcher.

| Type | PDU | Direction | Status | Spec | Notes |
|-----:|-----|-----------|--------|:----:|-------|
| 0x00 | ICReq | H → C | 🟩 Yes | M | First PDU on every accepted TCP connection (128 bytes: 8-byte common header + 120-byte payload). `PFV != 0` → FES 0x01 (Invalid PDU Header Field). |
| 0x01 | ICResp | C → H | 🟩 Yes | M | Advertises `MAXH2CDATA = 128 KiB`, `dgst=0`. Captures host's MAXR2T per-connection. |
| 0x02 | H2CTermReq | H → C | 🟩 Yes | M | Logged; closes the connection silently. |
| 0x03 | C2HTermReq | C → H | 🟩 Yes | M | Emitted on protocol violations (FES 0x01 Invalid Header Field / 0x02 PDU Sequence Error / 0x07 Invalid PDU Header Type) before closing. |
| 0x04 | CapsuleCmd | H → C | 🟩 Yes | M | 64-byte SQE + optional in-capsule data. |
| 0x05 | CapsuleResp | C → H | 🟩 Yes | M | 16-byte CQE. |
| 0x06 | H2CData | H → C | 🟩 Yes | M | R2T fulfillment — partial-ICD + tail stitching supported. Honors host's MAXR2T and the advertised MAXH2CDATA per-PDU cap. |
| 0x07 | C2HData | C → H | 🟩 Yes | M | One PDU per command for controller-to-host transfers. SUCCESS bit folds the CQE into the C2HData when the CQE is success with `DW0 = DW1 = 0` (saves one PDU on every Identify / Read / Get Log Page). |
| 0x09 | R2T | C → H | 🟩 Yes | M | One R2T per write command; `TTAG=1` covers the whole transfer (pure-R2T) or the tail (partial-ICD). See [`NVMETCP.md`](NVMETCP.md) § *Write data flow (ICD + R2T)*. |

### Negotiated parameters

| Parameter | Value | Status | Notes |
|-----------|-------|--------|-------|
| `PFV` | 0 | 🟩 Fixed | Only PFV=0 accepted. |
| `HDGSTF` (header digest, CRC32C) | Off | 🟨 No | See *Deliberate non-conformance*. |
| `DDGSTF` (data digest, CRC32C) | Off | 🟨 No | See *Deliberate non-conformance*. |
| `MAXH2CDATA` (advertised) | 128 KiB | 🟩 Fixed | Per-PDU cap on host-to-controller data. |
| `MAXR2T` (host-advertised) | Captured | 🟩 Yes | Clamped to ≥ 1 per NVMe-TCP §3.6.1. |
| Outstanding R2Ts per command | 1 | 🟩 Fixed | Multi-R2T deferred — see *Deliberate non-conformance*. |
| Max transfer per command (MDTS) | 1 MiB | 🟩 Fixed | Advertised as MDTS=8 (2^8 × 4 KiB page). The SGL data length is checked against this before the receive buffer is allocated; over-cap commands are aborted with `Invalid Field in Command`. Bounds a host-declared length so one CapsuleCmd can't drive a multi-GiB allocation. |

### Write data flow (ICD + R2T)

Total transfer length comes from the SGL Data Block descriptor at
`SQE.DPTR[8..12]`. Four shapes the command loop handles:

| Shape | Server behavior |
|-------|-----------------|
| No data (SGL length = 0) | Dispatch with `data_out = None`. |
| Fully in-capsule (ICD == SGL) | Zero-copy: dispatch borrowing from the CapsuleCmd body. |
| Partial ICD + R2T tail (0 < ICD < SGL) | Allocate `Vec<u8>` of SGL bytes, copy ICD prefix, emit R2T for the remainder. H2CData payloads land at their absolute `DATAO`. |
| Pure R2T (ICD == 0) | Emit one R2T with `TTAG=1` covering the whole transfer; assemble H2CData PDUs by their `DATAO`. |

Protocol violations during R2T fulfillment (CCCID / TTAG mismatch,
`DATAO + DATAL` overrun beyond the R2T window, wrong PDU type) close
the connection with C2HTermReq + FES 0x01 / 0x02.

### Direction derivation (CapsuleCmd routing)

From `OPC[1:0]` (NVMe Base §4.2.1):

| Bits | Direction | Behavior |
|-----:|-----------|----------|
| `00` | No transfer | Dispatch immediately. |
| `01` | Host-to-controller | ICD / R2T flow (above), then dispatch. |
| `10` | Controller-to-host | Dispatch immediately; non-empty `data_in` sent as one C2HData PDU. SUCCESS-bit folding active. |
| `11` | Bidirectional | Refused with C2HTermReq. |

---

## TLS-PSK (NVMe-TCP §3.6.1.5)

TLS-PSK is an opt-in mode that wraps the entire NVMe/TCP session in TLS 1.3
with pre-shared keys. It is mode-selected via `nvmetcp.tls.mode` in YAML
(`disabled` / `psk`); when `psk`, every accepted TCP connection is wrapped
in TLS **before** the NVMe ICReq / Connect handshake runs. This means the
NVMe identity exchange happens inside an already-established encrypted
channel, not in the clear. Full design — interchange-vs-identity key
formats, HKDF derivation, per-handshake parse, NQN cross-check, and the
s2n-tls integration — is in [`NVMETCP.md`](NVMETCP.md) §
*TLS-PSK (NVMe-TCP §3.6.1.5)*.

| Feature | Status | Spec | Notes |
|---------|--------|:----:|-------|
| TLS 1.3 wrapping the NVMe/TCP byte stream | 🟩 Yes | CC | Required when `nvmetcp.tls.mode = psk`. |
| TLS 1.2 fallback | 🟨 No | — | Refused. NVMe-TCP §3.6.1.5 mandates TLS 1.3. |
| Cipher suite `TLS_AES_128_GCM_SHA256` | 🟩 Yes | M (TLS-PSK) | One of the two §3.6.1.5 mandated suites. |
| Cipher suite `TLS_AES_256_GCM_SHA384` | 🟩 Yes | M (TLS-PSK) | The other §3.6.1.5 mandated suite. |
| PSK identity v0 (no HMAC binding) | 🟩 Yes | M | VSA derives the TLS PSK for both v0 and v1 on every handshake. |
| PSK identity v1 (HMAC binds key to hostnqn+subnqn) | 🟩 Yes | M | As above. |
| Interchange format `NVMeTLSkey-1:NN:<base64(key‖crc32)>:` | 🟩 Yes | — | Operator config + `nvme-cli` keyring format. CRC-32 validated at parse time. |
| HKDF derivation (`nvme-tls-psk` label) | 🟩 Yes | M (TLS-PSK) | Unit-tested against RFC 5869 Test Case 1 and round-trip fixtures. |
| Per-handshake config reload (`ClientHelloCallback`) | 🟩 Yes | — | Adds, disables, and rotates take effect on the next session with no daemon restart. |
| Mutual host-NQN cross-check | 🟩 Yes | — | After the TLS handshake, the host-NQN field of the negotiated PSK identity is compared against the host-NQN in the subsequent Connect. Mismatch fails Connect with `connect_invalid_parameters` and tears the session down. Silent in cleartext mode. |
| Rotation grace window | 🟩 Yes | — | Per-entry `previous_interchange_key` + `previous_expires_at`; both keys derive their (v0, v1) PSK pairs while the grace window is open. |
| DH-HMAC-CHAP (Admin Fabrics 0x05 / 0x06) | 🟨 No | O | See *Deliberate non-conformance*. |

---

## Discovery, NQN, identifiers

| Item | Status | Spec | Notes |
|------|--------|:----:|-------|
| Subsystem NQN | 🟩 Yes | M | `nqn.2025-10.com.metebalci:thurvsa`, single source of truth in `shared_naming::DISK.nqn`; operator override via `nvmetcp.subnqn`. |
| Discovery controller (`nqn.2014-08.org.nvmexpress.discovery`) | 🟨 No | O | Hosts connect direct to the subsystem NQN. |
| NSID encoding | 🟩 Yes | M | `nsid = lun + 1`. NSID 0 reserved per NVMe Base §6. |
| NGUID in Identify Namespace (bytes 104..120) | 🟩 Yes | O | Populated from the per-volume UUID, so Linux generates a stable `/dev/disk/by-id/nvme-<wwid>` that survives NSID renumber. Symmetric with the SBC-3 side's INQUIRY VPD 0x80 / 0x83. |
| EUI-64 in Identify Namespace | 🟨 No | O | NGUID alone is sufficient — Linux's wwid resolution picks the first non-zero of (NGUID, EUI-64). |
| NGUID descriptor in CNS 0x03 (NIDT=0x02) | 🟩 Yes | O | List emits NIDT=0x02 NGUID first, then NIDT=0x04 CSI. Some kernels prefer the descriptor-list NGUID for wwid generation. |
| UUID descriptor in CNS 0x03 (NIDT=0x03) | 🟨 No | O | NGUID + CSI is sufficient for wwid generation and namespace attach. |

---

## Deliberate non-conformance — thurvsa NVMe

The items below are deliberate departures from the NVMe specifications —
features that the specs define but that VSA does not implement, with the
reasoning for each. SCSI-side cross-cutting departures are in
[`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md) §
*Deliberate non-conformance — shared*.

| Item | Why |
|------|-----|
| Header / data digests (CRC32C, NVMe-TCP §3.5) | Negotiate `dgst=0`. TCP provides a checksum and TLS-PSK provides AEAD over the whole stream. Symmetric with the iSCSI transport, which also negotiates `HeaderDigest=None` / `DataDigest=None`. CRC32C is implemented elsewhere — SSC LTO-7+ Logical Block Protection appends a per-tape-block CRC32C trailer (`core/mediachanger/src/lbp.rs`) — distinct from the transport-frame digest. Hosts that require transport digests see negotiation fail and pick a different target. |
| Multi-outstanding R2T per command | Single-R2T-per-command is bandwidth-equivalent for any transfer that fits the network's bandwidth-delay product. Lift only if a benchmark shows the round-trip is the bottleneck on a high-latency link. |
| Async events other than reservation notifications | AER (Admin 0x0C) is implemented, but the only event source wired is reservation notifications (LID 0x80 — see *Reservation notifications* below). Namespace-attribute (the Notice-type events behind FID 0x0B Async Event Configuration), firmware-activation, and thermal notices are not produced — VSA has no firmware mechanism or thermal sensors, and namespaces are bound at `volume create`. The generic AER plumbing is reusable when a namespace-change source lands. |
| Discovery controller absent | Hosts connect direct to the subsystem NQN; operators distribute the address / port / NQN out of band. A discovery listener lands when multi-subsystem deployments become a documented use case. |
| DH-HMAC-CHAP (NVMe Base §8.13.5, Fabrics 0x05 / 0x06) | The non-TLS auth alternative — auth exchange encrypted, data stream not. Lands only if a deployment can't terminate TLS at the target. |
| Firmware download / commit (Admin 0x10 / 0x11) | The daemon ships as a binary, not a flashable image. |
| Sanitize, Format NVM, Security Send / Receive (Admin 0x80 / 0x82 / 0x84) | No in-place sanitize on a chunk-pool-backed virtual volume; LBA format is fixed at `volume create`; at-rest encryption is keystore-driven below the dispatcher boundary. |
| PCIe-only admin opcodes (0x00 / 0x01 / 0x04 / 0x05 / 0x7C) | Fabrics queue creation happens via Connect; doorbells are a PCIe concept. `Invalid Command Opcode` is the correct fabrics behavior. |

---

## Common questions about the surface

### What's a "namespace"?

The NVMe equivalent of a SCSI LUN: one addressable block device the
host attaches as a `/dev/nvmeXn1`-style node. One controller exposes
N namespaces, each with its own NSID, size, and LBA format. In VSA,
**one namespace = one volume = one LUN**, with `nsid = lun + 1` so
NSID 0 stays reserved.

The LUN (and therefore NSID) is **pinned at `volume create` time**
and persisted in the volume manifest, so host references survive
sibling volume add / remove cycles. Auto-assign picks the smallest
unused LUN; `thurvsa volume create --lun N` pins an explicit
value (refuses with HTTP 409 on collision).

Namespace Management admin commands (Admin 0x15) would let the
*host* mutate that map at runtime; VSA does not implement them
because volumes are created out-of-band via
`thurvsa volume create` (or `POST /api/v1/volumes`).

### Don't NVMe storage arrays expose Namespace Management on the wire?

No — out-of-band lifecycle is the enterprise-array norm. Every
enterprise NVMe-oF array routes namespace lifecycle through its
management plane (REST / CLI / GUI), not through NVMe Namespace
Management on the data plane: capacity policy, multi-tenant quotas,
AuthN / AuthZ, RBAC, and audit don't fit in a 64-byte SQE submitted
by whoever connected to the controller. The host's NVMe identity
(NQN + maybe a TLS-PSK) is a transport credential, not a
storage-admin credential. `POST /api/v1/volumes` over the admin Unix
socket is structurally identical to Pure's `POST /1.x/volumes` or
ONTAP's `POST /api/storage/namespaces`.

Namespace Management *is* expected on physical NVMe SSDs that
advertise OACS bit 8 — directly attached, where the OS admin runs
`nvme create-ns` to split a drive for QoS isolation, and on NVMe
SR-IOV passthrough. VSA's Identify Controller correctly clears
**OACS bit 8** ("Namespace Management Supported"), which tells a
probing host to use the management plane.

### Is there a discovery surface for volumes?

- **Protocol-level inventory** (one target → N namespaces): yes, via
  the standard NVMe path. After Connect, the host issues Identify
  CNS 0x02 (Active Namespace List) to walk the NSIDs, then CNS 0x00
  + CNS 0x03 per NSID for size, LBA format, and CSI. Linux nvme-tcp
  does this automatically on `nvme connect`. The iSCSI analog is
  REPORT LUNS (SPC-4 0xA0) after login.
- **Multi-target / multi-subsystem discovery**: not implemented on
  the NVMe side. We don't ship a Discovery Controller
  (`nqn.2014-08.org.nvmexpress.discovery`), so the operator
  distributes the subsystem NQN + IP + port out of band. The iSCSI
  side does answer SendTargets discovery, but again returns the
  single target.

Operators get a multi-volume view via the admin Unix socket
(`GET /api/v1/volumes` / `thurvsa volume list`) — a
management-plane catalog, not a host-side discovery surface.

### What is AER, and how does VSA use it?

Async Event Request (Admin 0x0C) is the NVMe analog of SCSI Unit
Attention: AER commands sit pending on the admin queue until the
controller has news. VSA parks each AER on the per-subsystem controller
registry (`nvme_nvm::ControllerRegistry`, shared with the transport,
keyed per CNTLID) and completes one when
an event fires, with DW0 = `0x00800006` (AET=0x6 I/O Command Set
specific, AEI=0x00 Reservation Log Page Available, LID=0x80) telling
the host to read the Reservation Notification log. Parking lives in the
transport (`nvme-tcp::server`), not the dispatcher, because completing
an AER needs the connection's writer — the dispatcher hands back one
synchronous CQE per command and has no deferred-completion path.

The **only** event source today is reservations. Thermal, firmware-
activation, and namespace-change notices are not produced (no sensors,
no firmware mechanism, namespaces bound at `volume create`), so an AER
parks until a reservation event affects the host or the connection
tears down. Per NVMe Base §5.2 an AER that never completes is legal;
parked AERs are released when the admin connection closes.

### Reservation notifications (LID 0x80)

When a host is fenced by another host's Reservation Acquire (Preempt),
Release, or Clear, the loser learns *proactively* instead of only
reactively via `Reservation Conflict` (0x83) on its next command. The
NVMe reservation adapter diffs the shared `ReservationManager` state
before and after each mutating op and derives the affected hosts:

- **Registration Preempted (type 1)** — a non-holder registrant whose
  registration is removed by a Preempt.
- **Reservation Released (type 2)** — every other registrant when a
  reservation is released (Release, or the holder self-unregistering a
  non-all-registrants reservation). An all-registrants holder
  *rotation* keeps the reservation, so it emits nothing.
- **Reservation Preempted (type 3)** — the prior holder whose
  reservation is taken over by a Preempt, and every other registrant on
  a Clear. A host that loses both its registration and its held
  reservation in one Preempt gets type 3 only.

The issuing host is never notified (it learns the result from its own
command completion). A reservation event fans out to every live
controller of the affected host: each appends a 64-byte entry to *that
controller's* LID 0x80 queue (Log Page Count, type, NSID, and the count
of further unread entries) and completes one parked AER. `nvme
resv-notif-log` reads the oldest entry per call until the queue drains
to an all-zero empty page. A controller can suppress any class via Set
Features FID 0x82 (Reservation Notification Mask), stored per (CNTLID,
NSID).

Routing is per-controller (keyed by CNTLID); the reservation state
itself is keyed by the 128-bit Connect HOSTID. A controller's
notification log + FID 0x82 masks are freed when the controller's last
association drops, while the host's reservation registration persists
(HOSTID-keyed) — a host that reconnects gets a fresh controller and
learns the current state via Reservation Report, not stale
notifications. One deliberate simplification: a host's *own* other
controllers are not notified of its commands (the issuer is excluded at
the host level, not the controller level) — a rare case for a
single-host-multi-controller setup releasing its own reservation.

**Cross-transport scope (issue #66).** When a volume is exported over both
iSCSI and NVMe/TCP, the two data paths share this one `ReservationManager`, so
*enforcement* is fully coherent: a Write Exclusive (or any of the six types)
reservation held by a SCSI initiator denies an NVMe host's writes via the same
`allow_write` / `allow_read` checks, and vice-versa — the SCSI↔NVMe type mapping
is 1:1 and an iSCSI `(IQN, ISID)` registrant never equals an NVMe HOSTID
registrant. The *pull* reports also agree (`nvme resv-report` shows an iSCSI
holder with `hostid = 0`, `cntlid = 0`; `sg_persist --read-reservation` shows
the NVMe holder's key + type). What is **not** wired is *proactive*
cross-transport notification: the diff above only fans LID 0x80 events to NVMe
controllers, and only for NVMe-originated mutations — a reservation change made
over iSCSI raises no NVMe AER (and no NVMe change raises a SCSI Unit Attention,
which the SCSI path does not emit for reservations at all). A host fenced from
across the other transport discovers it reactively (Reservation Conflict on its
next I/O) or by polling the report. A transport-neutral `ReservationManager`
change-observer that fans out to both paths is a tracked follow-up.

**Persistence across a target restart (PTPL).** When a host's most-recent
Reservation Register set CPTPL = "set PTPL", the registration and any
reservation are written to `<data_dir>/reservations.json` before the
command completes and reloaded at the next daemon start (RESCAP bit 0 =
1, Reservation Report PTPLS = 1). The reloaded state is **authoritative**:
after a target restart a non-holder is fenced immediately with no
re-registration, and the prior holder — reconnecting under the same
HOSTID — is still the holder. A host must therefore **not** blind-register
after a restart: a plain `RREGA_REGISTER` (IEKEY = 0) with `CRKEY = 0`
gets Reservation Conflict, because a registration already exists for that
HOSTID and the CRKEY must match the current key. To deliberately rotate
its key a host uses `Register and Ignore Existing Key` (IEKEY = 1), which
rebinds the HOSTID to the new key and re-persists. Linux `nvme` does not
auto-register, so the common path is clean. The HOSTID is host-stable, so
no identity fixup is needed on reload (unlike the iSCSI side, which keys
by the initiator IQN + ISID rather than the ephemeral TSIH).

### Is DH-HMAC-CHAP used in practice?

Rarely. DH-HMAC-CHAP (NVMe Base §8.13.5, Fabrics 0x05 / 0x06) is
NVMe's challenge-response auth wrapped in a Diffie-Hellman key
exchange. Its raison d'être is environments that can't terminate
TLS at the storage target — typically enterprise SAN gear with a
fabric appliance doing TLS in front of the array. Almost every
other deployment uses TLS-PSK (what we ship) or runs cleartext on a
trusted network.

### Format NVM and Sanitize — should we at least stub them?

- **Format NVM (Admin 0x80)** — "reformat the namespace with a new
  LBA size, optionally with secure erase." Not in any host bring-up
  path. The "format with current LBA size" case could be stubbed as
  a success no-op, but refusing and documenting
  `thurvsa volume delete && volume create` is also fine. The
  full Crypto-Erase / User-Data-Erase / SES spec has no analog —
  the chunk pool is shared and content-addressed.
- **Sanitize (Admin 0x84)** — "guarantee all user data is
  unrecoverable per NIST 800-88." The whole point is the
  *guarantee*; no-op success would be a lie. Better to refuse than
  to claim a guarantee we don't provide.

So: maybe stub Format, definitely don't stub Sanitize.

### Are Security Send / Receive (Admin 0x81 / 0x82) for encryption?

Not the encryption *we* do. Security Send / Receive carry arbitrary
security-protocol payloads — the SCSI SECURITY PROTOCOL IN / OUT
model (thurvtl does implement that on the tape side, to receive
AES-256 keys via SPOUT page 0x10 for Application-Managed
Encryption). Protocols carried this way include TCG Opal, IEEE
1667, and in-band TLS.

For VSA, neither applies: at-rest encryption is keystore-driven,
AES-256-GCM, applied per-page below the dispatcher boundary —
invisible to the host, no SED-style host-unlock surface. In-band
TLS is superseded by socket-level TLS-PSK
(`nvmetcp.tls.mode: psk`). Security Send / Receive land only if a
deployment needs TCG Opal-style host-controlled unlock or in-band
auth that can't ride the existing TLS-PSK path.

---

## How this table stays honest

The compliance tables above are only useful if they track the code. Each
entry maps to a specific code path; if an opcode is added, changed, or
deliberately dropped, this table must be updated in the same commit. The
relevant source files are:

NVMe code paths:

- NVM Command Set dispatcher:
  [`../nvme/nvm/src/dispatcher.rs`](../nvme/nvm/src/dispatcher.rs)
  (`NvmeNvmDispatcher::dispatch_io` / `dispatch_admin`).
- NVM opcode enum:
  [`../nvme/nvm/src/opcode.rs`](../nvme/nvm/src/opcode.rs).
- Reservation command adapter (parses RR wire fields, drives the
  shared `ReservationManager` by HOSTID, renders the Report):
  [`../nvme/nvm/src/reservations.rs`](../nvme/nvm/src/reservations.rs).
- Admin opcode enum + Fabrics types + Identify CNS + Controller
  registers + log-page + reservation wire shapes (RESCAP / ONCS /
  RTYPE map / Reservation Status Data Structure):
  [`../nvme/base/src/`](../nvme/base/src/) (`opcode.rs`,
  `fabrics.rs`, `identify.rs`, `log_page.rs`, `reservation.rs`).
- NVMe/TCP transport (PDU codec, per-connection state machine,
  fabrics interception, R2T flow, fused-pair tracking):
  [`../nvme/tcp/src/`](../nvme/tcp/src/) (`pdu.rs`, `server.rs`).
- TLS-PSK derivation and `ClientHelloCallback` integration:
  [`../nvme/tcp/src/tls.rs`](../nvme/tcp/src/tls.rs),
  [`../nvme/tcp/src/identity.rs`](../nvme/tcp/src/identity.rs).
- Daemon-side `NamespaceLookup` impl (NSID → `PageCache`):
  [`../vsa/daemon/src/registry.rs`](../vsa/daemon/src/registry.rs).

When a new NVM opcode / admin opcode / fabrics command / PDU type /
CNS value / FID / LID / TLS knob ships, update this table in the
same commit. SBC-3 / iSCSI surface changes go in
[`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md).
