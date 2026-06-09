# SCSI Conformance

This document is the per-spec coverage map for the entire SCSI
surface that VTL and VSA present. SCSI is a single wire protocol with
a layered command set — primary commands at the bottom, device-type
command sets stacked on top — so rather than scatter the coverage
across several files it all lives here, divided into three
independent parts:

- **[Part 1: SPC-4, SAM-5, and iSCSI](#part-1-spc-4-sam-5-and-iscsi)**
  — the SCSI primary commands, the architecture model, and the
  iSCSI / CHAP transport that both products share. The code lives in
  `shared/iscsi/` plus the per-product SCSI dispatchers.
- **[Part 2: SSC-4 and SMC-3 (VTL)](#part-2-ssc-4-and-smc-3-vtl)** —
  VTL's sequential-access tape drives (SSC-4) and medium changer
  (SMC-3), the tape VPD / mode / log pages, the SECURITY PROTOCOL
  tape-encryption surface, and the behavioral model.
- **[Part 3: SBC-3 (VSA)](#part-3-sbc-3-vsa)** — VSA's direct-access
  block volumes over iSCSI / SCSI.

VSA can also be reached over a second transport — NVMe / NVMe-oF /
NVMe-TCP — concurrently with iSCSI (`transports: [iscsi, nvmetcp]`,
issue #66). That surface has its own document,
[`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md). When both are bound they
share one `scsi_spc::reservations::ReservationManager`, so a reservation
taken over one transport fences initiators on the other (see *Persistent
Reservations* below). Byte-level CDB and page layouts are not repeated
here; they live in [`SPEC.md`](SPEC.md).

Read each part as a snapshot of *what is actually wired in the code*,
not as a restatement of what the standard mandates. The **Spec**
column and the color squares are there to flag the gaps between the
two.

**Targets:** SPC-4 (T10/1731-D), SAM-5 (T10/2104-D), iSCSI (RFC 7143
consolidated / RFC 7144 SCSI mapping), CHAP (RFC 1994), SSC-4
(T10/2069-D), SMC-3 (T10/1730-D), SBC-3 (T10/1799-D). Plus the
LTO-7 / LTO-8 drive features maintained by the LTO Consortium.

## Reading the tables

**Status legend:**

- **Yes** — fully implemented to spec.
- **Partial** — opcode / feature handled, but a subset of the spec
  (specific service actions, parameter values, or modes only).
- **Stub** — opcode answered with a structurally-valid but
  hard-coded / all-zero response (initiators see a healthy device,
  no real telemetry behind it).
- **No-op** — opcode accepted (returns GOOD) but produces no state
  change. The surface exists for backup-software compatibility but
  the underlying behavior doesn't apply.
- **No** — not implemented; returns CHECK CONDITION / ILLEGAL
  REQUEST / INVALID COMMAND OPCODE.
- **N/A** — feature does not apply to the emulation target.

**Spec column:** **M** mandatory for the device type the table
covers; **O** optional (a "No" / "No-op" against O is conformant,
against M is not); **CC** conditionally mandatory (required only
when a prerequisite feature is supported); **—** vendor-specific or
outside the listed standard.

**Status-cell color squares:**

- 🟩 — implemented (Status = Yes / Partial / Stub / No-op / N/A).
- 🟨 — not implemented, spec doesn't require it (Status = No,
  Spec = O / CC-not-triggered / —). A conformant gap.
- 🟥 — not implemented and the spec *does* require it (Status = No,
  Spec = M). A real conformance gap. **Hunt these.**

**Per-row product annotations.** Some commands behave differently
depending on which surface they target. Where that is the case, the
Notes column carries a `thurvtl tape:` or `thurvsa block:` prefix to
split the explanation by product.


---

# Part 1: SPC-4, SAM-5, and iSCSI

## SPC-4 — SCSI Primary Commands (all LUNs)

These are the commands SPC-4 defines for any SCSI logical unit,
regardless of device type — the baseline every LUN both products
expose must answer.

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x00 | TEST UNIT READY | 🟩 Yes | M | Returns GOOD once UA cleared. |
| 0x03 | REQUEST SENSE | 🟩 Yes | M | Fixed + descriptor formats. |
| 0x12 | INQUIRY | 🟩 Yes | M | Standard page + VPD pages — see per-product VPD tables below. thurvtl tape: partition-fenced sessions get the SPC-4 "no logical unit" sentinel (peripheral qualifier 0b011, peripheral type 0x1F) when INQUIRYing a drive LUN whose drive isn't in the CHAP user's partition. Same shape thurvsa returns for unmapped volume LUNs. Lets the Linux kernel iSCSI initiator's all-LUNs post-login probe skip the LU and keep scanning the rest. |
| 0x15 | MODE SELECT(6) | 🟩 Partial | M | thurvtl tape: honors page 0x0F DCE bit and page 0x11 partition layout for behavior. Other advertised pages (0x01, 0x02, 0x10/0x00, 0x10/0x01 SPF=1, 0x1A, 0x1C) are round-tripped — bytes the host writes are stored per-drive (emulated NVRAM) and re-emitted verbatim by the next MODE SENSE under PC=Current / PC=Saved. SP=1 mirrors the bodies into `<data_dir>/library/drive_state.json` so they survive cartridge swaps and restarts. PS=1 advertised on every page header; per-page Changeable masks reflect round-tripped fields. Changer LUN 0 is read-only — every page accepted-and-ignored. thurvsa block: validate-and-accept-if-matches against the values MODE SENSE just returned (PF=1 required; SP=1 → SAVING PARAMETERS NOT SUPPORTED; every Changeable bit zero so WCE / RCD / DRA / D_SENSE can't flip). |
| 0x16 | RESERVE(6) | 🟩 No-op | O | Accepted; reservation state not tracked. SPC-4 obsoletes RESERVE/RELEASE in favor of PERSISTENT RESERVE. |
| 0x17 | RELEASE(6) | 🟩 No-op | O | Accepted. |
| 0x1A | MODE SENSE(6) | 🟩 Yes | M | See per-product / per-LUN mode-page tables. |
| 0x1B | START STOP UNIT | 🟩 Yes | O | thurvtl tape: LOAD/UNLOAD on tape; no-op on changer. thurvsa block: accept-and-GOOD regardless of PowerCondition / LOEJ / START bits. |
| 0x1C | RECEIVE DIAGNOSTIC RESULTS | 🟩 Partial | CC | Pages 0x00 (Supported Diagnostic Pages → `[0x00, 0x10]`) and 0x10 (Self-Test Results, SPC-4 §7.2.21 — 20 entries × 20 bytes, page-length 0x0190, most recent first). PCV=0 returns page 0x00. Other page codes return CHECK CONDITION + ILLEGAL REQUEST + INVALID FIELD IN CDB. (thurvtl only; thurvsa has no diagnostic ring buffer.) |
| 0x1D | SEND DIAGNOSTIC | 🟩 Partial | M | thurvtl tape: default no-op probe (SELFTEST=0 + SELF-TEST CODE=0) returns GOOD without recording. SELFTEST=1 routes by LUN: LU0 runs library + inventory + storage-backend health (full `validate_object_store_backend` probe — auth + write + delete on every named entry); LU1+ re-validates the loaded cartridge's `manifest.json`, or GOOD if no cartridge loaded. Foreground/background extended self-test codes (0b001..0b110) accepted as GOOD without execution. Failures return CHECK CONDITION + HARDWARE ERROR + DIAGNOSTIC FAILURE ON COMPONENT 80h. Per-LUN history (most recent 20) queryable via RECEIVE DIAGNOSTIC RESULTS page 0x10. thurvsa block: no diagnostic surface. |
| 0x1E | PREVENT/ALLOW MEDIUM REMOVAL | 🟩 Yes | O | thurvtl tape: per-I_T-nexus state. Bit 0 (data-transport) gates SCSI UNLOAD on the drive and MOVE MEDIUM with that drive as source — refused with ILLEGAL REQUEST + 0x53/0x02. Bit 1 (mechanical) gates the admin `POST /api/v1/changer/unload` endpoint — refused with HTTP 409 + `refused: "mechanical_eject_prevented"`; `force: true` overrides. The two bits are independent. State cleared when the I_T nexus ends. Issued against changer LUN: accepted, no enforcement. thurvsa block: accept-and-GOOD regardless of bit 0 / bit 1. |
| 0x3B | WRITE BUFFER | 🟩 Stub | O | Firmware-download surface accepted, ignored. (thurvtl only; thurvsa rejects with INVALID OPERATION CODE.) |
| 0x3C | READ BUFFER | 🟩 Stub | O | Returns zeros. (thurvtl only.) |
| 0x4C | LOG SELECT | 🟩 No-op | O | Accepted; PCR and parameter list ignored. The live LOG SENSE counters are lifetime odometers, which real LTO drives don't let a host zero via LOG SELECT — they reset only through the operator CLI (`reset-stats`). |
| 0x4D | LOG SENSE | 🟩 Yes | O | thurvtl tape: see per-LUN log-page tables. thurvsa block: page 0x00 only, listing just 0x00 itself. Other page codes return INVALID FIELD IN CDB. |
| 0x55 | MODE SELECT(10) | 🟩 Partial | O | Same coverage as 0x15 (round-trip + SP=1 persistence + SPF=1 subpage parsing). thurvsa block: same validate-and-accept-if-matches semantics. |
| 0x56 | RESERVE(10) | 🟩 No-op | O | |
| 0x57 | RELEASE(10) | 🟩 No-op | O | |
| 0x5A | MODE SENSE(10) | 🟩 Yes | O | |
| 0x5E | PERSISTENT RESERVE IN | 🟩 Yes | O | Both products: full READ KEYS / READ RESERVATION / REPORT CAPABILITIES / READ FULL STATUS surface backed by the shared `scsi_spc::reservations::ReservationManager`. thurvtl tape: on the drive LUN (LUN ≥ 1) and the medium changer (LUN 0) — reservation state is keyed per-LUN, so the changer's is independent of the drives'. |
| 0x5F | PERSISTENT RESERVE OUT | 🟩 Partial | O | Both products implement SAs 0x00 REGISTER, 0x01 RESERVE, 0x02 RELEASE, 0x03 CLEAR, 0x04 PREEMPT, 0x05 PREEMPT AND ABORT, 0x06 REGISTER AND IGNORE EXISTING KEY against the shared `ReservationManager`; 0x07 REGISTER AND MOVE rejected (no multi-port). APTPL = 1 is honored (state persisted to `<data_dir>/reservations.json`, reloaded at start) and PTPL_C = 1 in REPORT CAPABILITIES; SPEC_I_PT / ALL_TG_PT still reject as INVALID FIELD IN PARAMETER LIST. A durable-write failure on a persist-eligible mutation returns CHECK CONDITION / HARDWARE ERROR (INTERNAL TARGET FAILURE 0x44) rather than a false GOOD. thurvtl tape: on both the drive LUN (LUN ≥ 1, fences the medium read/write path) and the medium changer (LUN 0, fences MOVE / EXCHANGE / element-status — see SMC-3 § PERSISTENT RESERVE). |
| 0xA0 | REPORT LUNS | 🟩 Yes | M | thurvtl tape: LUN 0 (changer) + LUN 1..N (drives). Partition-fenced sessions see only LUN 0 plus drives the bound partition owns. thurvsa block: SAM-5 single-level flat-space encoding over the live volume → LUN map. CHAP-user volume-admission–fenced sessions (`UserEntry.volumes`) see only the LUNs of their admitted volumes; INQUIRY / TUR / READ CAPACITY against non-admitted LUNs return PQ=0x3 (no LU). The admission set is resolved dynamically per command, so a `grant` / `revoke` reaches already-connected sessions (a REPORTED LUNS DATA HAS CHANGED UA prompts re-enumeration) — see § Dynamic LUN admission. |
| 0xA2 | SECURITY PROTOCOL IN | 🟩 Partial | CC | thurvtl tape: protocol 0x00 (supported list) + 0x20 (Tape Data Encryption) only. Not implemented: TCG / OPAL (0x01–0x06), IEEE 1667 (0x40), IKEv2-SCSI (0x41), SPC-4 authentication (0xEE / 0xEF) — all return CHECK CONDITION (none apply to tape). Mandatory only on devices advertising data encryption. thurvsa block: not implemented. |
| 0xA3 | MAINTENANCE IN | 🟩 Partial | O | See SA table below. thurvtl SPC-4 SAs not implemented: 0x05 REPORT IDENTIFYING INFORMATION plus storage-array-specific SAs (0x01–0x04, 0x06–0x08, 0x0B, 0x0E, 0x10–0x11). thurvsa block: SAs 0x0A REPORT TARGET PORT GROUPS, 0x0C REPORT SUPPORTED OPERATION CODES, 0x0D REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS — other SAs return INVALID FIELD IN CDB. |
| 0xA4 | MAINTENANCE OUT | 🟩 Partial | O | See SA table below. thurvtl SPC-4 SAs not implemented: 0x06 SET IDENTIFYING INFORMATION, 0x0E SET PRIORITY, storage-array-specific SAs. (thurvtl only; thurvsa rejects 0xA4.) |
| 0xB5 | SECURITY PROTOCOL OUT | 🟩 Partial | CC | thurvtl tape: protocol 0x20 / SPSP 0x0010 (Set Data Encryption) only. Other SPSPs and protocols return CHECK CONDITION. (LUN 0 dispatches the same opcode to REQUEST VOLUME ELEMENT ADDRESS, stub — see SMC-3 table.) Mandatory only on devices advertising data encryption. thurvsa block: not implemented. |

### MAINTENANCE IN (0xA3) service actions

MAINTENANCE IN is a multiplexed opcode: the real command is picked by
a service-action field in the CDB. Both the parent 0xA3 and every
individual service action are optional in SPC-4, so an initiator
cannot assume any of them is present — it discovers which ones are
accepted by issuing REPORT SUPPORTED OPERATION CODES (SA 0x0C).

| SA | Name | Status | Spec | Notes |
|---:|------|--------|:----:|-------|
| 0x05 | REPORT IDENTIFYING INFORMATION | 🟨 No | O | Companion to MAINTENANCE OUT SA 0x06; both unimplemented. |
| 0x0A | REPORT TARGET PORT GROUPS | 🟩 Yes | O | Both products. One TPG per distinct configured TPGT, member RTPIs assigned sequentially from 1 in portal order, all TPGs default to ACTIVE/OPTIMIZED (implicit ALUA). |
| 0x0C | REPORT SUPPORTED OPERATION CODES | 🟩 Yes | O | Both products. thurvtl tape: one-command and all-commands forms. thurvsa: every routed CDB in ascending order; source of truth for VAAI / Hyper-V offload discovery. |
| 0x0D | REPORT SUPPORTED TASK MGMT FUNCTIONS | 🟩 Yes | O | Advertises ABORT TASK / ABORT TASK SET / CLEAR TASK SET / LU RESET / I_T NEXUS RESET. |
| 0x0F | REPORT TIMESTAMP | 🟩 Yes | O | thurvtl only. |
| 0x1E | DYNAMIC RUNTIME ATTRIBUTE — read | 🟩 Yes | — | thurvtl only. Vendor-specific extension. |
| 0x1F | READ LOGGED-IN HOST TABLE | 🟩 Yes | — | thurvtl only. Vendor-specific extension. Returns active iSCSI session table. |

### MAINTENANCE OUT (0xA4) service actions

MAINTENANCE OUT is the write-side counterpart, again service-action
multiplexed and again with every SA individually optional in SPC-4.
Only thurvtl routes this opcode.

| SA | Name | Status | Spec | Notes |
|---:|------|--------|:----:|-------|
| 0x0F | SET TIMESTAMP | 🟩 Yes | O | |
| 0x1E | DYNAMIC RUNTIME ATTRIBUTE — write | 🟩 No-op | — | Vendor-specific extension. Accepted, no persisted state. |

### Reservations — thurvtl tape vs thurvsa block

SCSI reservations exist to keep two initiators from stepping on each
other's I/O. Both products implement the SCSI-3 PERSISTENT RESERVE
family for real, backed by one shared state machine
(`scsi_spc::reservations::ReservationManager`) so the block and tape
surfaces can't drift. The topologies still differ — VSA is a genuine
clustered SAN, while a VTL drive is usually owned by one backup
server — but "one connection per session" (`MaxConnections=1`, no
MC/S) does not mean "one host per LUN": distinct initiators each open
their own session (distinct TSIH), so two I_T nexuses can reach the
same drive and a reservation genuinely arbitrates between them.

**thurvsad (block)** — Windows Failover Cluster, VMware MSCS,
Pacemaker `fence_scsi`, and Oracle ASM all depend on SCSI-3
persistent reservations to decide which node owns a LUN:

- **PRIN (0x5E) READ KEYS / READ RESERVATION / READ FULL STATUS**
  walk live `ReservationManager` state. **REPORT CAPABILITIES**
  advertises the full SBC-3 type matrix (WR_EX / EX_AC / WR_EX_RO /
  EX_AC_RO / WR_EX_AR / EX_AC_AR — TYPE_MASK = `0xEA, 0x01`) and sets
  PTPL_C = 1 (the daemon persists reservation state across power loss);
  PTPL_A reflects the LU's currently-active APTPL bit. Under a
  dual-transport export (`transports: [iscsi, nvmetcp]`, issue #66) the
  registrant set on a LUN can include NVMe hosts: READ KEYS (keys only)
  and READ RESERVATION (holder key + type) render them transparently;
  READ FULL STATUS has no SPC-4 TransportID format for an NVMe host, so
  such a registrant's descriptor carries an **empty iSCSI TransportID**
  (its key, R_HOLDER bit, and type stay correct) — a documented
  limitation, not in any acceptance path.
- **PROUT (0x5F) REGISTER, RESERVE, RELEASE, CLEAR, PREEMPT,
  PREEMPT AND ABORT, REGISTER AND IGNORE EXISTING KEY** mutate state
  per SPC-4 §6.14. Registrations are keyed by the **stable iSCSI
  initiator port** — the initiator IQN plus its ISID (the SCSI
  TransportID identity, RFC 7143 / SPC-4 §7.6.4.7) — **not** the
  ephemeral, target-assigned TSIH. That identity round-trips across
  logout and daemon restart and distinguishes MPIO paths.
  `iscsi.reservations.initiator_port: iqn` collapses the ISID so
  registrants key by IQN alone — a host then reclaims its reservation
  across a reconnect even if its ISID changes (Linux open-iscsi mints a
  fresh ISID per login); the default `iqn-isid` keeps the full port.
  Data-path
  enforcement: WRITE (10/16), SYNCHRONIZE CACHE (10/16), READ (10/16)
  consult `ReservationManager::allow_write` / `allow_read` and surface
  RESERVATION CONFLICT (status 0x18, no sense) when blocked. Because
  those checks are transport-neutral and keyed by LUN, a reservation
  held by an NVMe host (when the volume is also exported over NVMe/TCP,
  issue #66) fences a SCSI initiator's WRITE just the same — the iSCSI
  port and the NVMe host are distinct registrants under the 1:1 SCSI↔NVMe
  type mapping. **Proactive notification (issue #67):** a reservation
  preempted/released — over either transport — now also raises a
  RESERVATIONS PREEMPTED (0x06/0x2A/0x03) or RESERVATIONS RELEASED
  (0x06/0x2A/0x04) Unit Attention on each affected iSCSI initiator's next
  command (delivered by the dispatch-level UA preemption, exactly like
  MEDIUM MAY HAVE CHANGED on tape). It is driven by a transport-neutral
  observer on the shared `ReservationManager`, so it fires whether the
  change originated over iSCSI or NVMe, and the same path closes the
  pre-existing iSCSI→iSCSI gap for VTL tape drive + changer LUNs; see
  [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md) § Reservation notifications.
- **REGISTER AND MOVE (SA 0x07)** rejected — thurvsa is single-port.
- **APTPL = 1** is honored: the LU's registrations + reservation are
  written to `<data_dir>/reservations.json` (atomic temp-file write +
  rename + parent-dir fsync) **before** the PROUT is acknowledged GOOD
  (persist-before-ack); a write failure returns CHECK CONDITION /
  HARDWARE ERROR (INTERNAL TARGET FAILURE 0x44). Setting APTPL back to 0
  (or CLEAR) erases the on-disk record in the same durable rewrite.
  **SPEC_I_PT = 1, ALL_TG_PT = 1** still reject as INVALID FIELD IN
  PARAMETER LIST (no multi-port registration).
- **Persistence across nexus loss is unconditional.** A persistent
  registration is removed only by an explicit PROUT (RELEASE /
  unregister / PREEMPT / CLEAR) or — when APTPL = 0 — a daemon restart.
  A logout / TCP drop no longer evicts it (SPC-4: persistent
  reservations survive I_T nexus loss), so a reconnecting initiator
  reusing its ISID is still the holder and need not re-register.
  `on_session_close` therefore touches no reservation state.

**thurvtld (tape)** implements the same PR family on the **drive LUN**
(LUN ≥ 1), reusing the shared `ReservationManager` (issue #16). PRIN /
PROUT service actions, the (IQN, ISID) initiator-port keying, the APTPL
persistence, the SPEC_I_PT / ALL_TG_PT rejections, and the survive-
nexus-loss behavior are identical to the block side; the drive / changer
LUN is itself the stable persistence identity. The differences are
tape-shaped:

- **Data-path enforcement** fences the medium opcodes: WRITE(6) 0x0A,
  WRITE FILEMARKS 0x10 / 0x80, ERASE 0x19, FORMAT MEDIUM 0x04
  (write-side) and READ(6) 0x08, VERIFY 0x13 / 0x8F, SPACE 0x11 / 0x91
  (read-side) return RESERVATION CONFLICT to a non-permitted nexus.
  Positioning (REWIND, LOCATE, READ POSITION, LOAD/UNLOAD), mode
  pages, identity, and the PR commands themselves are never fenced
  (SAM-5 §5.9.1).
- **RESERVE / RELEASE (6) and (10) — no-op + GOOD.** The legacy SPC-2
  "claim the device" CDBs some backup software issues at session
  acquire stay accepted no-ops (CRH = 0); the SCSI-3 PR family is the
  real mechanism.
- **Medium changer (LUN 0)** implements the same PR family as the
  drives (issue #53) — SMC-3 changers legitimately support
  reservations, which matter when multiple media servers share one
  library's robotics. `ReservationManager` is keyed by LUN, so the
  changer's reservation is **independent** of every drive's: reserving
  the changer does not fence any drive, and vice versa. The
  enforcement gate (`scsi_smc::dispatch::pr_enforce`) fences the
  movement / inventory opcodes — MOVE MEDIUM 0xA5, EXCHANGE MEDIUM
  0xA6, INITIALIZE ELEMENT STATUS 0x07 / 0x37, SEND VOLUME TAG 0xB6
  (write-side), and READ ELEMENT STATUS 0xB8, REQUEST VOLUME ELEMENT
  ADDRESS 0xB5 (read-side) — returning RESERVATION CONFLICT to a
  non-permitted nexus. Identity / status / mode pages / the PR
  commands themselves are never fenced (SAM-5 §5.9.1). PROUT (0x5F)
  appears in both the changer's and the drives' REPORT SUPPORTED
  OPERATION CODES.

### Read-attribute / write-attribute (thurvtl tape only)

The READ ATTRIBUTE / WRITE ATTRIBUTE opcodes are SSC-4-flavored and
apply only to the tape side; their full coverage is in
[Part 2 (SSC-4 / SMC-3)](#part-2-ssc-4-and-smc-3-vtl) § SSC-4.

---

## SAM-5 — SCSI Architecture Model

| Feature | Status | Spec | Notes |
|---------|--------|:----:|-------|
| LU model | 🟩 Yes | M | thurvtl tape: one changer + N drives behind one target (per the YAML `library:` block). thurvsa: N volumes as flat LUNs behind one target (per `volume create`). |
| Task management — ABORT TASK | 🟩 Yes | M | Always returns "Function complete." |
| Task management — ABORT TASK SET | 🟩 Yes | M | |
| Task management — CLEAR TASK SET | 🟩 Yes | M | |
| Task management — LOGICAL UNIT RESET | 🟩 Yes | M | |
| Task management — I_T NEXUS RESET | 🟩 Yes | M | |
| Task management — CLEAR ACA | 🟨 No | CC | Mandatory only if ACA is implemented; ACA not modeled. |
| Task management — TARGET RESET | 🟨 No | — | Deprecated by SAM-4. |
| Task attributes — SIMPLE / ORDERED / HEAD OF QUEUE / ACA | 🟨 No | O | Accepted but not differentiated; commands serialized per LU. |
| Auto Contingent Allegiance (ACA) | 🟨 No | O | |
| Asynchronous Event Notification | 🟨 No | O | No async event queue. |
| Unit Attention model | 🟩 Yes | M | Per (I_T_L) UA queue; preempts every opcode except INQUIRY / REQUEST SENSE / REPORT LUNS. |
| Sense data — fixed format | 🟩 Yes | M | thurvtl tape uses 0x70/0x71 fixed format. |
| Sense data — descriptor format | 🟩 Yes | M | thurvsa block uses 0x72 descriptor format. |
| LUN encoding — single-level / flat-space | 🟩 Yes | M | Both products. SAM-5 §4.7.4 8-byte LUN field. |

---

## iSCSI — RFC 7143 (consolidated) / RFC 7144 (SCSI mapping)

iSCSI is the transport that carries SCSI command and data PDUs over
TCP. The whole transport — login phase, PDU framing, sequencing, and
the CHAP exchange — lives in `shared/iscsi/` and is byte-for-byte
identical between the two products; what differs is only the SCSI
command set layered on top. Each daemon binds its own TCP port
(default 3260, which an operator overrides on one of the two when
both run co-resident) and presents its own target IQN
(`iqn.2025-10.com.metebalci:thurvtl` vs
`iqn.2025-10.com.metebalci:thurvsa`).

### PDU types

| Opcode | PDU | Direction | Status | Spec | Notes |
|-------:|-----|-----------|--------|:----:|-------|
| 0x00 | NOP-Out | I → T | 🟩 Yes | M | |
| 0x20 | NOP-In | T → I | 🟩 Yes | M | |
| 0x01 | SCSI Command | I → T | 🟩 Yes | M | |
| 0x21 | SCSI Response | T → I | 🟩 Yes | M | |
| 0x02 | SCSI Task Management Function Request | I → T | 🟩 Yes | M | |
| 0x22 | SCSI Task Management Function Response | T → I | 🟩 Yes | M | |
| 0x03 | Login Request | I → T | 🟩 Yes | M | |
| 0x23 | Login Response | T → I | 🟩 Yes | M | |
| 0x04 | Text Request | I → T | 🟩 Yes | M | SendTargets + parameter negotiation. |
| 0x24 | Text Response | T → I | 🟩 Yes | M | |
| 0x05 | SCSI Data-Out | I → T | 🟩 Yes | M | |
| 0x25 | SCSI Data-In | T → I | 🟩 Yes | M | |
| 0x06 | Logout Request | I → T | 🟩 Yes | M | |
| 0x26 | Logout Response | T → I | 🟩 Yes | M | |
| 0x31 | Ready-to-Transfer (R2T) | T → I | 🟩 Yes | M | |
| 0x32 | Asynchronous Message | T → I | 🟨 No | O | Target may emit; not required. |
| 0x3F | Reject | T → I | 🟩 Yes | M | Reason 0x04 stray Data-Out / 0x09 unsupported opcode; connection then closed. |
| 0x10 | SNACK Request | I → T | 🟨 No | CC | Mandatory only at ErrorRecoveryLevel ≥ 1; we negotiate ERL=0. |

### Login phase

| Feature | Status | Spec | Notes |
|---------|--------|:----:|-------|
| Discovery sessions (TSIH = 0) | 🟩 Yes | M | SendTargets=All / specific IQN. |
| Normal sessions | 🟩 Yes | M | Two-stage Security → Operational. |
| Stage transitions (T-bit) | 🟩 Yes | M | |
| Session reinstatement | 🟩 Yes | M | New TSIH supersedes prior. |

### Operational parameters (negotiated)

During the Operational stage of login the two ends agree on a set of
text keys. The Status column below reflects whether shared-iscsi
actually implements the negotiation handshake for each key, as
opposed to relying on the spec default.

| Key | Default / range | Status |
|-----|-----------------|--------|
| MaxRecvDataSegmentLength | 128 KiB | 🟩 Yes (negotiated per direction) |
| HeaderDigest | None / CRC32C | 🟩 Negotiated; always answers `None` |
| DataDigest | None / CRC32C | 🟩 Negotiated; always answers `None` |
| InitialR2T | No (unsolicited Data-Out allowed) | 🟩 Yes |
| ImmediateData | Yes | 🟩 Yes |
| FirstBurstLength | 128 KiB | 🟩 Yes |
| MaxBurstLength | 16 MiB | 🟩 Yes (matches READ BLOCK LIMITS max) |
| DefaultTime2Wait | 2 s | 🟩 Yes |
| DefaultTime2Retain | 0 s | 🟩 Yes |
| MaxOutstandingR2T | 1 | 🟩 Spec default; not transmitted |
| DataPDUInOrder | Yes | 🟩 Spec default; not transmitted |
| DataSequenceInOrder | Yes | 🟩 Spec default; not transmitted |
| MaxConnections | 1 | 🟩 Fixed (no MC/S) |
| ErrorRecoveryLevel | 0 | 🟩 Fixed (no level 1/2 recovery) |
| OFMarker / IFMarker | No | 🟩 Markers not supported (RFC 7143 deprecated) |

### Fixed values

A few negotiated keys are not really negotiated at all — the daemon
pins them to one value and declines to budge. Each choice is
deliberate:

- **`MaxConnections=1` (no MC/S).** The session is pinned to a single
  TCP connection. Multiple-Connections-per-Session would let one
  session span several TCP links for bandwidth or failover; where
  that is genuinely needed, the answer is multipath at the SCSI
  layer (dm-multipath) rather than MC/S inside the session.
- **`ErrorRecoveryLevel=0`.** At ERL 0 the session is simply torn
  down on any error and the SCSI layer retries the command. The
  higher levels — digest-failure retransmit at level 1, full
  connection recovery under MC/S at level 2 — are not implemented,
  which costs nothing in practice because every mainstream initiator
  runs ERL 0 anyway.
- **`OFMarker=No` / `IFMarker=No`.** These keys would insert
  fixed-interval boundary markers into the byte stream; RFC 7143
  deprecated the mechanism, so the daemon never negotiates them on.
- **`HeaderDigest=None` / `DataDigest=None`.** These keys ask for a
  CRC32C integrity check on every PDU. The daemon does negotiate
  them — an initiator that proposes a digest expects an answer — but
  the answer is always `None`, because TCP already checksums the
  stream and a second per-PDU CRC buys little. The NVMe-TCP
  transport makes the symmetric choice, negotiating `dgst=0` in its
  ICReq / ICResp handshake; see
  [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md) § Deliberate
  non-conformance — thurvsa NVMe. Note that CRC32C *is* implemented
  on the SCSI tape side, but as Logical Block Protection — an
  LTO-7+ end-to-end per-block trailer driven by mode page
  0x0A/0xF0. That is a SCSI-layer data-protection feature applied to
  the stored block, not a transport-frame digest.

### Spec defaults relied on (not transmitted)

Three keys — `MaxOutstandingR2T`, `DataPDUInOrder`, and
`DataSequenceInOrder` — never appear in the Login response at all.
RFC 7143 says that when a key is absent its default applies, and the
defaults here (`1`, `Yes`, `Yes`) are exactly the behavior
shared-iscsi already implements: at most one outstanding R2T per
command, Data-In PDUs delivered in monotonically increasing
`BufferOffset`, and Data-In sequences in command order. So there is
nothing to transmit. If an initiator does propose one of these keys
with a non-default value, the daemon simply ignores the proposal
rather than echoing it — a path no shipping initiator actually
takes, since open-iscsi leaves all three at their defaults, VMware
proposes `MaxOutstandingR2T=1`, and Windows matches.

### Sequencing

| Mechanism | Status | Spec |
|-----------|--------|:----:|
| CmdSN ordering, sliding window (currently 1 — see TODO § Async PDU demux) | 🟩 Yes | M |
| StatSN per response | 🟩 Yes | M |
| ExpCmdSN / MaxCmdSN echoed in responses | 🟩 Yes | M |
| DataSN / R2TSN per command | 🟩 Yes | M |
| ExpStatSN acknowledgment | 🟩 Yes | M |

### Data flow

| Mechanism | Status | Spec |
|-----------|--------|:----:|
| Unsolicited immediate data | 🟩 Yes | CC |
| Unsolicited Data-Out (up to FirstBurstLength) | 🟩 Yes | CC |
| Solicited Data-Out via R2T | 🟩 Yes | M |
| Final-bit (F) on last Data-Out of a burst | 🟩 Yes | M |
| BufferOffset monotonicity check | 🟩 Yes | M |
| Phase-collapse (status piggybacked on Data-In) | 🟩 Yes | O |

### Path redundancy — multi-portal advertisement, per-portal TPG, ALUA

The daemon accepts a list of TCP listen portals in `iscsi.listen`
and binds one `TcpListener` per entry. SendTargets discovery returns
one `TargetAddress=<address>,<tpgt>` line for every configured
portal, so an initiator that runs `iscsiadm -m discovery` once gets
one node record per portal back. That is enough to feed
`dm-multipath` two (or more) paths without the operator running
SendTargets twice against two different host IPs — the manual
workaround that path-redundancy deployments previously needed.

Each portal carries its own Target Portal Group Tag. The YAML shape
exposes the TPGT directly:

- Bare-string entries (e.g. `listen: ["10.0.0.5:3260",
  "10.0.0.6:3260"]`) auto-assign sequential TPGTs from input
  position (1, 2, …). The single-portal default
  `listen: "0.0.0.0:3260"` keeps the historical `TPGT=1`.
- Object-form entries (e.g.
  `listen: [{address: "10.0.0.5:3260", tpgt: 1},
   {address: "10.0.0.6:3260", tpgt: 2}]`) carry an explicit TPGT —
  operators preparing for ALUA give each portal its own.
- Multiple portals may share one TPGT (one TPG, many paths). The
  same address listed twice is rejected at boot: `bind(2)` would
  fail anyway and the initiator can't disambiguate two
  `TargetAddress` lines with identical `ip:port`.

The Login Response `TargetPortalGroupTag` key echoes the *arrival*
portal's TPGT per RFC 7143 §12.10 — the value the initiator gets
back matches the portal it dialed, so a session bound to portal 2
sees `TargetPortalGroupTag=2` regardless of what the other portals
advertise.

The SCSI layer publishes the same topology through the standard
ALUA surface (SPC-4 §5.16) so `dm-multipath`'s ALUA path-priority
checker can drive path selection automatically:

- **INQUIRY standard data byte 5** — TPGS field = 01b (implicit
  ALUA only — REPORT TPG is supported but SET TPG is not).
- **VPD 0x83 — Device Identification** — three additional
  `Association=TargetPort` designators per advertised portal:
  - NAA-3 (`DesignatorType::Naa`) — 8-byte identifier derived from
    the chassis serial (VTL) / target IQN (VSA) plus the RTPI, so
    each port has a stable identity across daemon restarts.
  - Relative Target Port Identifier (`DesignatorType::RelativeTargetPort`)
    — the RTPI assigned at startup (sequential from 1 in portal
    order).
  - Target Port Group (`DesignatorType::TargetPortGroup`) — the
    portal's TPGT, which is the wire identifier the host correlates
    against the REPORT TPG response.
- **VPD 0x86 — Extended INQUIRY Data** — published with every
  capability bit clear so VPD-page enumeration sees a contiguous
  list. TPGS lives in INQUIRY standard data byte 5, not in this
  page (SPC-4 §6.4.2).
- **REPORT TARGET PORT GROUPS** (MAINTENANCE IN service action
  0x0A, SPC-4 §6.27.7) — one TPG descriptor per distinct
  configured TPGT, each carrying that TPG's asymmetric access state
  plus the RTPIs of its member ports. Every TPG defaults to
  `ACTIVE/OPTIMIZED` on startup — no operator action needed; out of
  the box every advertised path is usable at full priority.

SET TARGET PORT GROUPS (MAINTENANCE OUT 0x0A) is deliberately not
wired today: TPGS=01b (implicit-only) tells initiators not to issue
SET TPG, so the four standard transitions (`active-optimized`,
`active-non-optimized`, `standby`, `unavailable`) are reserved for
a later operator-driven CLI verb. The other deferred edges — the
`transitioning` access state, multi-controller logical-unit groups,
and Persistent Reservation participation across paths — are tracked
separately; implicit-only is enough for ALUA-aware multipath to
work out of the box.

Wildcard entries (`0.0.0.0:*`, `[::]:*`) are substituted with the
connection's actual local IP when SendTargets emits — emitting the
wildcard literally would hand the initiator an unusable record.
Single-entry `listen: "0.0.0.0:3260"` is therefore identical in
behavior to the pre-multi-portal target.

### Out of scope

The following parts of iSCSI are deliberately not implemented, each
for a reason that follows from the fixed values above:

- **MC/S** — ruled out directly by `MaxConnections=1`.
- **ERL 1/2 recovery** — out-of-order retransmit, connection
  recovery, and the SNACK / A-bit machinery that go with the higher
  error-recovery levels are not implemented.
- **Markers (IFMarker / OFMarker)** — RFC-deprecated, so never
  negotiated.
- **Asynchronous Message (PDU 0x32)** — this PDU would carry iSCSI
  parameter renegotiation and SCSI AEN delivery, but the daemon
  needs neither: it never renegotiates parameters mid-session, and
  every SCSI state change reaches the initiator through a Unit
  Attention on its next command instead. On daemon shutdown the
  socket simply closes and open-iscsi reconnects.
- **iSER / iSCSI offload** — this is an initiator- and
  hardware-side concern. The daemon is a software target speaking
  plain iSCSI-over-TCP with no RDMA verbs endpoint; an iSER HBA
  reaching it just falls back to the ordinary TCP path.

---

## CHAP — RFC 1994 (iSCSI in-band authentication)

| Feature | Status | Spec | Notes |
|---------|--------|:----:|-------|
| `AuthMethod=None` | 🟩 Yes | M | Default when CHAP not configured. |
| `AuthMethod=CHAP` | 🟩 Yes | M | Required when any user is configured. |
| `CHAP_A=5` (MD5) | 🟩 Yes | M | RFC 1994 / RFC 7143 standard algorithm. |
| `CHAP_A=6` (SHA-1) | 🟩 Yes | — | De-facto extension; interoperates with Linux LIO and open-iscsi 3.x+. |
| `CHAP_A=7` (SHA-256) | 🟩 Yes | — | De-facto extension. |
| `CHAP_A=8` (SHA3-256) | 🟩 Yes | — | De-facto extension; preferred by default. |
| Algorithm negotiation from a `CHAP_A` list | 🟩 Yes | M | Target picks the strongest algorithm common to its `allowed_algorithms` list (preference SHA3-256 → SHA-256 → SHA-1 → MD5) and the initiator's offer. |
| One-way CHAP (target authenticates initiator) | 🟩 Yes | M | |
| Mutual CHAP (initiator also authenticates target) | 🟩 Yes | O | Per-user `mutual_chap` flag; mutual response uses the same negotiated algorithm. |
| 16-byte challenge | 🟩 Yes | O | `rand::thread_rng()`; same length regardless of digest. |
| Identifier cycling 1..255 | 🟩 Yes | M | |
| Audit trail (success / failure) | 🟩 Yes | — | Success records the negotiated algorithm. Failure reasons logged: missing CHAP_N/CHAP_R, invalid hex, response mismatch, no common algorithm. |

The exchange runs as the four steps below, where `H` stands for
whichever digest the two ends settled on — MD5, SHA-1, SHA-256, or
SHA3-256:

1. Initiator: `AuthMethod=CHAP,None` → Target picks `CHAP`.
2. Initiator: `CHAP_A=8,7,6,5` → Target picks the strongest mutually
   supported algorithm, replies `CHAP_A=<id>, CHAP_I, CHAP_C`.
3. Initiator: `CHAP_N=<user>, CHAP_R=<H(I‖password‖C)>` → Target verifies.
4. (Mutual only) Initiator sends its own `CHAP_C, CHAP_I` → Target replies `CHAP_N=<target>, CHAP_R=<H(I‖target_password‖C)>`.

Three limitations are worth being explicit about:

- CHAP authenticates the login handshake but does nothing to encrypt
  the SCSI traffic that follows. On an untrusted path the answer is
  to wrap the session — IPsec or an SSH tunnel — because
  iSCSI-over-TLS is not implemented.
- CHAP passwords sit in plaintext in the daemon config. This is not
  a shortcut: CHAP by construction requires a plaintext-equivalent
  secret to be present at both ends, so there is no hashed-at-rest
  form that would still let the daemon compute the response.
- The algorithm IDs for SHA-1, SHA-256, and SHA3-256 (6, 7, and 8)
  are not part of RFC 7143. They are used here because they match
  the numbering already adopted by Linux LIO and open-iscsi, which
  keeps interoperability intact.

---

## Deliberate non-conformance — shared

The table below collects the departures from SPC-4 / SAM-5 / iSCSI /
CHAP that cut across both products — places where conformance was
knowingly traded away because the feature has no purpose in either
daemon. Departures specific to one product are listed elsewhere: in
Part 2 for the tape and changer, in Part 3 for the SBC-3 block
target, and in [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md) for the
NVMe block transport.

| Item | Why |
|------|-----|
| Task attributes (SIMPLE / ORDERED / HOQ) ignored | Commands serialized per-LU; queueing model unnecessary. |
| ACA / CLEAR ACA absent | Same reason. |
| Async Event Notification absent | SAM-5 Unit Attention queue plus iSCSI session teardown cover every state change either product cares about. |
| MC/S fixed to 1 | No real-world deployment for either product. |
| ErrorRecoveryLevel fixed to 0 | TCP retransmit handles reliability. |
| `READ BUFFER` / `WRITE BUFFER` stubbed (thurvtl only — thurvsa rejects) | Firmware-download surface has no analog on a software target. |
| SPC-5 absent | Conformance target is SPC-4. |

---

## How this table stays honest

A conformance table is only worth reading if it tracks the code, so
this section anchors each part of Part 1 to the files that implement
it. The shared-iscsi transport — login, PDU framing, the R2T loop,
and CHAP — is in [`../../shared/iscsi/src/`](../../shared/iscsi/src/),
spread across `transport.rs`, `auth.rs`, `session.rs`, and
`unit_attention.rs`. The SCSI dispatch that sits on top is
per-product:

- thurvtl tape: [`../../vtl/daemon/src/iscsi/protocol.rs`](../../vtl/daemon/src/iscsi/protocol.rs)
  (`handle_scsi_command` / `dispatch_scsi`). Per-page handlers in
  `scsi/ssc/src/scsi/{mode_pages.rs, log_pages.rs,
  encryption_pages.rs, attributes.rs}`; library-touching handlers in
  `scsi/ssc/src/dispatch/handlers.rs` and `scsi/smc/src/dispatch/`.
- thurvsa block: [`../../scsi/sbc/src/dispatcher.rs`](../../scsi/sbc/src/dispatcher.rs)
  (`SbcScsiDispatcher::dispatch`). Per-opcode arms in
  `scsi/sbc/src/{data_path.rs, mode_sense.rs, reservations.rs,
  probes.rs, maintenance.rs}`.

The rule that keeps the document trustworthy: whenever a new opcode,
VPD page, mode page, log page, or service action ships — or an
existing one is explicitly rejected — the matching table is updated
in the same commit. Changes to SPC-4 / SAM-5 / iSCSI / CHAP belong in
this file; SSC-4 / SMC-3 in Part 2; SBC-3 in Part 3; NVMe in
[`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md).


---

# Part 2: SSC-4 and SMC-3 (VTL)

## SSC-4 — Sequential-Access Commands (LUN ≥ 1, tape drives)

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x01 | REWIND | 🟩 Yes | M | Seeks to BOM (block 0). |
| 0x04 | FORMAT MEDIUM | 🟩 Yes | O | Every SSC-4-defined FORMAT field value is handled: 0x00 (erase + rewind), 0x01 (apply pending Mode Page 0x11 partition layout), 0x02 (default single partition). FORMAT 0x03–0x0F are reserved by SSC-4 and correctly return ILLEGAL REQUEST. IMMED bit ignored — completes synchronously (format is effectively instant on a VTL, so the host observes the same result). |
| 0x05 | READ BLOCK LIMITS | 🟩 Yes | M | Max 16 MiB − 1 (`0x00FF_FFFF`, matches MaxBurstLength), min 0, optimal 0. |
| 0x08 | READ(6) | 🟩 Yes | M | Variable + fixed block; SILI / FIXED bits honored. Hitting a filemark surfaces CHECK CONDITION + NoSense + FM=1 + INFO = TRANSFER LENGTH (SSC-4 §7.6) so the host can advance position. Reading past EOD surfaces CHECK CONDITION + BlankCheck + ASC/ASCQ 0x00/0x05 + INFO = TRANSFER LENGTH (SSC-4 §4.2.20 / §8.3.1); the EOM bit is *not* set — EOD is not physical end-of-medium. Both responses carry the residual via INFO so the Linux st driver can short-read; without it dd hands stale kernel-buffer bytes back to userspace (issues #25 / #26). |
| 0x0A | WRITE(6) | 🟩 Yes | M | Variable + fixed block; per-block WORM / legal-hold checks. |
| 0x0B | SET CAPACITY | 🟩 Yes | O | Acts as ERASE-equivalent and persists CAPACITY PROPORTION VALUE (CDB[2..4]) in the manifest. Subsequent WRITE / WRITE FILEMARKS gate at the host-set effective capacity: 95% raises Early Warning (NoSense + EOM=1 + 0x00/0x02), 100% returns EndOfMedium (VolumeOverflow + EOM=1 + 0x00/0x02). EW is sticky-once-per-pass; rewind / locate-to-BOM / erase / SET CAPACITY clears it. IMMED bit ignored. |
| 0x10 | WRITE FILEMARKS(6) | 🟩 Yes | M | 24-bit filemark count. |
| 0x11 | SPACE(6) | 🟩 Yes | M | Records / Filemarks / EOD; sign-extended 24-bit count. Spacing over *records* (code 000b) **halts on the first filemark in the path of motion** (SSC-4 §7.5): CHECK CONDITION + NoSense + ASC/ASCQ 0x00/0x01 (Filemark detected) + FM=1, INFO = residual `count − moved`, position left on the EOP side of the mark going forward / the BOP side going reverse. The Linux st driver depends on that stop to keep its (file, block) model in sync — walking past the mark made a following WRITE land where the host didn't intend (issue #102). A short traversal without a filemark (`moved != count`) also terminates CHECK CONDITION with the residual in INFO; that sense is **direction-aware** (SSC-4 §7.5): a *forward* shortfall ran past recorded data → BlankCheck + ASC/ASCQ 0x00/0x05 (End-of-data) — the slow-MTEOM / Bareos path (issue #33); a *backward* shortfall can only have reached Beginning-of-Partition → NoSense + ASC/ASCQ 0x00/0x04 (Beginning-of-Partition/Medium) — never EOD (issue #73). Reporting BlankCheck on backward motion broke the Linux st driver's `MTBSF` reposition. |
| 0x13 | VERIFY(6) | 🟩 No-op | O | All blocks valid on virtual media. |
| 0x19 | ERASE | 🟩 Yes | M | Wipes data, rewinds; refused on WORM. |
| 0x34 | READ POSITION | 🟩 Partial | M | SAs 0x00 / 0x01 (Short Form, 20 B), 0x06 (Long Form, 32 B), 0x08 (Extended Form, 32 B). SAs 0x02–0x05, 0x07, 0x09–0x1F return INVALID FIELD IN CDB. Long Form sets MPU (file/set numbers not tracked); Extended Form sets LOCU + BYCU (buffer counts not tracked); Short Form sets BPU when position > 2³². |
| 0x44 | REPORT DENSITY SUPPORT | 🟩 Yes | CC | LTO-7 / LTO-8 descriptors per `library.lto_generation` in the YAML. Mandatory on devices advertising multiple density codes. |
| 0x80 | WRITE FILEMARKS(16) | 🟩 Yes | O | 32-bit count at CDB[12..16]. |
| 0x82 | ALLOW OVERWRITE | 🟩 Yes | O | Volatile flag, cleared on UNLOAD. |
| 0x8F | VERIFY(16) | 🟩 No-op | O | 64-bit LBA range. |
| 0x91 | SPACE(16) | 🟩 Yes | O | 64-bit count. Same semantics as SPACE(6): records-mode traversal halts on a filemark with NoSense 0x00/0x01 + FM=1 + residual (issue #102), and the residual + direction-aware EOD-vs-BOP termination is identical: forward shortfall → BlankCheck 0x00/0x05, backward shortfall → NoSense 0x00/0x04 (issue #73). |
| 0x92 | LOCATE(16) | 🟩 Yes | O | 64-bit LBA; CP bit honored. |
| 0x2B | LOCATE(10) | 🟩 Yes | O | 32-bit LBA; CP bit honored. (Same opcode as POSITION TO ELEMENT on LUN 0.) |

### Read-attribute / write-attribute (LUN ≥ 1)

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x8C | READ ATTRIBUTE | 🟩 Partial | O | SA 0x00 (attribute values) returns the synthesized device/medium attributes — 0x0000 Remaining Capacity, 0x0001 Maximum Capacity, 0x0003 Load Count, 0x0400 Manufacturer, 0x0401 Serial Number (all served read-only) — *merged with any host-written attributes persisted on the cartridge* (see WRITE ATTRIBUTE), in ascending-id order. SA 0x05 (supported list) returns those synthesized ids plus every persisted host id. SAs 0x01 / 0x02 return CHECK CONDITION. The response uses a 4-byte big-endian AVAILABLE DATA header and the SSC-4 5-byte attribute descriptor (`id(2) + control(1: bit 7 read-only, bits 1–0 format) + length(2) + value`). Barcode 0x0806 is *not* synthesized — like a real LTO-CM it is absent until a host writes it (the authoritative barcode is the changer's volume tag from READ ELEMENT STATUS). The remaining SSC-4 Annex A device/medium attributes (TapeAlert 0x0002, MAM space 0x0004, density 0x0006, init count 0x0007, byte counters 0x0220–0x0223, encryption-position 0x0224–0x0225, manufacture date 0x0406) are not synthesized. |
| 0x8D | WRITE ATTRIBUTE | 🟩 Yes | O | Host-range attributes (SSC-4 host ranges 0x0800–0x0BFF and 0x1400–0x17FF — e.g. application vendor/name/version 0x0800–0x0802, barcode 0x0806) are persisted to the cartridge's `runtime.json` sidecar and survive UNLOAD/reload; a zero-length write deletes the id. They are cleared on ERASE / FORMAT MEDIUM. Device/medium read-only ids (0x0000, 0x0001, 0x0003, 0x0400, 0x0401) and ids outside the host ranges are rejected, all-or-nothing, with CHECK CONDITION / ILLEGAL REQUEST / INVALID FIELD IN PARAMETER LIST (5h/26h/00h). An empty parameter list is a GOOD no-op. |

### Drive-side mode pages (LUN ≥ 1)

Every mode page is individually optional in SPC-4 / SSC-4, so the
table below is a record of which ones the drive LUN chooses to
expose rather than a checklist against a mandatory set.

| Page | Subpage | Name | Status | Spec | Notes |
|-----:|--------:|------|--------|:----:|-------|
| 0x01 | — | Read-Write Error Recovery | 🟩 Yes | O | |
| 0x02 | — | Disconnect-Reconnect | 🟩 Stub | O | All-zero (no disconnect/reconnect on iSCSI). |
| 0x0A | 0x01 | Control Extension | 🟩 Yes | O | SCSIP=1. |
| 0x0A | 0xF0 | Control Data Protection | 🟩 Yes | O | LTO-7+ Logical Block Protection (CRC32C). Drive advertises `LBP_INFO_LENGTH=4`, `LBP_METHOD=0x01` (CRC32C). Host sets LBP_W (body byte 0 bits 7..5) and LBP_R (bits 4..2) via MODE SELECT. With LBP_W set, WRITE(6/16) WRPROTECT > 0 validates the host-supplied 4-byte CRC32C trailer (mismatch → ABORTED COMMAND + 0x10/0x05). With LBP_R set, READ(6/16) RDPROTECT > 0 appends a freshly-computed CRC32C trailer. CRC recomputed from BLAKE3-verified plaintext on every read — no separate stored guard. |
| 0x0F | — | Data Compression | 🟩 Yes | O | DCC=1; DCE toggles per MODE SELECT; per-block algorithm recorded in `blocks-p<N>.idx` (each `BlockIndex` record carries `compression: Option<CompressionAlgo>`). |
| 0x10 | 0x00 | Device Configuration | 🟩 Yes | O | |
| 0x10 | 0x01 | Device Configuration Extension | 🟩 Yes | O | Round-tripped + enforced. WRITE MODE field (body byte 0 high nibble) drives Append-Only; WRE bit (body byte 2 bit 0) drives Encrypt-Only — see "LTO behaviors". PEWS bytes round-trip but aren't honored (fixed 95% EW trigger; SET CAPACITY relocates the threshold). |
| 0x11 | — | Medium Partition | 🟩 Yes | O | Staged via MODE SELECT, applied by FORMAT MEDIUM. |
| 0x1A | — | Power Condition | 🟩 Stub | O | All-zero idle/standby timers. |
| 0x1C | — | Informational Exceptions Control | 🟩 Yes | O | DExcpt=0, MRIE=6 (report on request). |

### Drive-side log pages (LUN ≥ 1)

Log pages, too, are each individually optional in SPC-4 / SSC-4 —
with one conditional twist: page 0x00, the Supported Log Pages list,
becomes mandatory the moment any other log page is implemented, since
an initiator needs a way to discover what is there.

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x00 | Supported Log Pages | 🟩 Yes | CC | |
| 0x02 | Write Errors | 🟩 Stub | O | All counters zero. |
| 0x03 | Read Errors | 🟩 Stub | O | |
| 0x06 | Non-Medium Errors | 🟩 Stub | O | |
| 0x0C | Sequential Access Device | 🟩 Yes | O | Live byte counters for the loaded cartridge (received-from / written-to / read-from media + transferred-to-initiator). Partition-capacity hints zero. |
| 0x0D | Temperature | 🟩 Stub | O | Fixed 25 °C. |
| 0x11 | DT Device Status | 🟩 Stub | O | |
| 0x12 | Tape Alert Response | 🟩 Stub | O | |
| 0x14 | Device Statistics | 🟩 Yes | O | Live Lifetime Volume Loads (param 0x0000, drive-scoped). Mirrors VPD 0xB1 serial in param 0x0040. Unmodeled counters (power-on hours, etc.) zero. |
| 0x16 | Last n Error Events | 🟩 Stub | O | Empty (no fault history). |
| 0x17 | Volume Statistics | 🟩 Yes | O | Validity=1 + live Volume Mounts when a volume is loaded; Validity=0 when empty. Error counters zero. |
| 0x1A | Power Condition Transitions | 🟩 Stub | O | |
| 0x1B | Data Compression | 🟩 Yes | O | 1:1 ratio; live cumulative byte counters (MB + remainder split) for the loaded cartridge. |
| 0x2E | TapeAlert | 🟩 Yes | O | All 64 flags reported, all clear (healthy drive). |
| 0x30 | Tape Usage (legacy) | 🟩 Yes | — | Legacy; thread count (loads) live from the loaded volume's mount count. Data-set / error counters zero. |
| 0x31 | Tape Capacity (legacy) | 🟩 Stub | — | Legacy. |
| 0x32 | Data Compression (legacy) | 🟩 Yes | — | Legacy mirror of 0x1B; live byte counters. |

### LTO behaviors

LTO is not a T10 standard — it is a tape-format and drive-feature
standard maintained separately by the LTO Consortium. The features
below therefore sit alongside the SSC-4 command set rather than
inside it, and the Spec column here reflects whether each feature is
required by the specific LTO generation being emulated (LTO-7 or
LTO-8) rather than by SSC-4.

| Feature | Status | Spec | Notes |
|---------|--------|:----:|-------|
| LTO-7 emulation | 🟩 Yes | M | 6 TB native; `library.lto_generation: 7` in the YAML. |
| LTO-8 emulation | 🟩 Yes | M | 12 TB native; default. |
| LTO-9 emulation | 🟨 No | — | Targets SPC-5 / SSC-5 + RAO; out of scope. |
| Append-only mode | 🟩 Yes | O | LTO-7+ feature. Mode Page 0x10/0x01 WRITE MODE = 1 → drive refuses WRITE / WRITE FILEMARKS at any LBA other than active-partition EOD with DATA PROTECT + 0x27/0x06 (CONDITIONAL WRITE PROTECT). State persists in `<data_dir>/library/drive_state.json` across cartridge swaps when SP=1. |
| Encrypt-only mode (LTO-8+) | 🟩 Yes | O | Mode Page 0x10/0x01 WRE bit set → drive refuses WRITE / WRITE FILEMARKS without an active drive encryption key (SECURITY PROTOCOL OUT 0x20/0x0010), DATA PROTECT + 0x74/0x0C (ENCRYPTION KEY ABSENT). |
| Application-Managed Encryption (AES-256-GCM) | 🟩 Yes | O | Per-block IV; key volatile, cleared on UNLOAD. |
| Drive compression (LZ4/zstd) | 🟩 Yes | O | Per-block algorithm recorded. SLDC reserved (not implemented). |
| Density: LTO-7 (0x5C) / LTO-8 (0x5E) | 🟩 Yes | M | |
| WORM cartridges | 🟩 Yes | O | Per-cartridge `--worm` flag; non-EOD writes return WRITE PROTECTED. |
| Legal hold (storage-resident) | 🟩 Yes | — | VTL-specific; not an LTO feature. Sentinel-driven; host sees write-protect at LOAD time. |

---

## SMC-3 — Medium Changer Commands (LUN 0)

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x07 | INITIALIZE ELEMENT STATUS | 🟩 Yes | M | Reloads `inventory.json`. Reservation-gated (write-side — see 0x5F). |
| 0x16 | RESERVE(6) | 🟩 No-op | O | |
| 0x17 | RELEASE(6) | 🟩 No-op | O | |
| 0x2B | POSITION TO ELEMENT | 🟩 No-op | O | Accepted; no robot motion to model. |
| 0x37 | INITIALIZE ELEMENT STATUS WITH RANGE | 🟩 No-op | O | Accepted; no reload — element status is always live in the virtual changer, so there is nothing to rescan. Reservation-gated (write-side — see 0x5F). |
| 0x56 | RESERVE(10) | 🟩 No-op | O | |
| 0x57 | RELEASE(10) | 🟩 No-op | O | |
| 0x5E | PERSISTENT RESERVE IN | 🟩 Yes | O | Full PRIN surface on the changer LUN, keyed independently of the drives (issue #53). See SPC-4 § PERSISTENT RESERVE IN. |
| 0x5F | PERSISTENT RESERVE OUT | 🟩 Partial | O | Real reservations on the changer LUN (issue #53): a held reservation fences MOVE / EXCHANGE / element-status opcodes (`scsi_smc::dispatch::pr_enforce`) with RESERVATION CONFLICT to a non-holder. Service-action coverage matches the drive LUN; see SPC-4 § PERSISTENT RESERVE OUT and Part 1 § Reservations. |
| 0xA5 | MOVE MEDIUM | 🟩 Yes | M | Storage ↔ Drive ↔ I/E ↔ Storage. Emits MEDIUM CHANGED UA. Partition-fenced when the session is bound to a logical partition (CHAP user → partition mapping); cross-partition src/dst returns ILLEGAL REQUEST + 0x21/0x01. Reservation-gated (write-side — see 0x5F). |
| 0xA6 | EXCHANGE MEDIUM | 🟩 Yes | O | Composed from two MOVE MEDIUMs. Same partition fence applies to all three element addresses. Reservation-gated (write-side — see 0x5F). |
| 0xB5 | REQUEST VOLUME ELEMENT ADDRESS | 🟩 Stub | O | Empty response. Reservation-gated (read-side — see 0x5F). |
| 0xB6 | SEND VOLUME TAG | 🟩 No-op | O | No barcode-assignment side-effect. Reservation-gated (write-side — see 0x5F). |
| 0xB8 | READ ELEMENT STATUS | 🟩 Yes | M | All element types; VOLTAG / DVCID / Mixed / CurData / Access flags honored. **Not** partition-fenced — every session sees every element. `mtx` parses a zero-descriptor per-type page as `Transport Element Descriptor Length too short`, so dropping out-of-partition elements there breaks `mtx load`. Topology leak is contained: MOVE MEDIUM / EXCHANGE MEDIUM still refuse out-of-partition src/dst, and drive-LUN INQUIRY against an out-of-partition LUN still returns PQ=NoDevice, so the data path stays isolated. Reservation-gated (read-side — see 0x5F). |

### Changer-side mode pages (LUN 0)

As on the drive side, every mode page is individually optional in
SPC-4 / SMC-3 — the table records which ones LUN 0 chooses to
present.

| Page | Subpage | Name | Status | Spec | Notes |
|-----:|--------:|------|--------|:----:|-------|
| 0x0A | 0x01 | Control Extension | 🟩 Yes | O | SCSIP=1. |
| 0x1C | — | Informational Exceptions Control | 🟩 Yes | O | MRIE=0 (host polls 0x2E). |
| 0x1D | — | Element Address Assignment | 🟩 Yes | O | Counts/start-addresses from live topology. |
| 0x1E | — | Transport Geometry | 🟩 Yes | O | Single transport. |
| 0x1F | — | Device Capabilities | 🟩 Yes | O | Advertises supported MOVE/EXCHANGE combinations. |

### Changer-side log pages (LUN 0)

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x00 | Supported Log Pages | 🟩 Yes | CC | Mandatory once any other log page is implemented. |
| 0x0D | Temperature | 🟩 Stub | O | 25 °C. |
| 0x2E | TapeAlert | 🟩 Yes | O | 64 flags, all clear. |

The 0xC0-0xFF range is reserved for vendor-specific changer log
pages. None are emitted: backup software never reads them, so
there is nothing to gain from inventing them.

### Element-type coverage

| Element type | Code | Status | Spec | Notes |
|--------------|-----:|--------|:----:|-------|
| Medium Transport | 1 | 🟩 Yes | M | Single transport at addr 1. |
| Storage | 2 | 🟩 Yes | M | Slots configured per topology (default base addr 1001). |
| Import/Export | 3 | 🟩 Yes | O | Mail slots configured per topology (default base addr 101). |
| Data Transfer (drive) | 4 | 🟩 Yes | CC | Mandatory for libraries that contain data-transfer elements (we do). Drives configured per topology (default base addr 1). |

---

## INQUIRY VPD pages — tape

### Changer (LUN 0)

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x00 | Supported VPD Pages | 🟩 Yes | CC | Mandatory once any other VPD page is implemented. |
| 0x80 | Unit Serial Number | 🟩 Yes | O | `<chassis_serial>_LL<NN>` — chassis serial from `library.json` (14-byte `TVLxxxxxxxxxxx` minted at init), `NN` = 1-based partition index. Sessions bound to different partitions see distinct serials. |
| 0x83 | Device Identification | 🟩 Yes | M | LU-association descriptors: NAA-3 (8 B locally assigned, `BLAKE3(chassis‖lun‖partition)` so per-(chassis, partition, LUN) unique), T10 vendor-based (ASCII), Logical Unit Group (4 B, `BLAKE3(chassis‖partition)`) — drives in the same partition share the group ID for backup-software auto-correlation. ALUA TargetPort-association descriptors per advertised iSCSI portal: NAA-3 (`BLAKE3(chassis‖"\|tp\|"‖RTPI)`), Relative Target Port Identifier (RTPI), Target Port Group (TPGT). |
| 0x85 | Management Network Address | 🟩 Yes | O | ASCII URL to HTTP listener. |
| 0x86 | Extended INQUIRY Data | 🟩 Yes | O | 64-byte page; all capability bits clear. TPGS for ALUA discovery lives in INQUIRY std-data byte 5, not here. |
| 0xC0 | Firmware Build Information | 🟩 Yes | — | Vendor-specific page-code range; ASCII daemon version. |

### Tape drive (LUN ≥ 1)

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x00 | Supported VPD Pages | 🟩 Yes | CC | Mandatory once any other VPD page is implemented. |
| 0x80 | Unit Serial Number | 🟩 Yes | O | Per-drive `mfg_serial` from `inventory.json` (10-byte `TVLxxxxxxx`, minted once when the drive is added). Falls back to legacy `THUR-MFG-NNN` for pre-field libraries. |
| 0x83 | Device Identification | 🟩 Yes | M | LU-association descriptors: NAA-3 + T10 + Logical Unit Group (same shape as the changer LUN; LUG ID identifies the logical library the drive belongs to). ALUA TargetPort-association descriptors per advertised iSCSI portal: NAA-3 (`BLAKE3(chassis‖"\|tp\|"‖RTPI)`), Relative Target Port Identifier (RTPI), Target Port Group (TPGT). |
| 0x86 | Extended INQUIRY Data | 🟩 Yes | O | 64-byte page; all capability bits clear. TPGS for ALUA discovery lives in INQUIRY std-data byte 5, not here. |
| 0xB0 | Sequential-Access Device Characteristics | 🟩 Yes | O | WORMM bit reflects loaded cartridge. |
| 0xB1 | Manufacturer-Assigned Serial Number | 🟩 Yes | O | 32 B ASCII, persisted per drive in `inventory.json::DriveInfo.mfg_serial` (random `TVLxxxxxxx`, minted once when the drive is added). |
| 0xB2 | TapeAlert Supported Flags | 🟩 Yes | O | Bitmap = 0xFF × 8 (all 64 flags advertised). |
| 0xB3 | Automation Device Serial Number | 🟩 Yes | O | Chassis serial from `library.json::chassis_serial` (14-byte `TVLxxxxxxxxxxx`), no partition suffix. Falls back to legacy `THUR-CHG-001` on pre-field libraries. |
| 0xB4 | Data Transfer Device Element Address | 🟩 Yes | O | Correlates drive LUN to changer element address. |

One subtlety: the drive LUN will answer a direct query for VPD 0xC0
(Firmware Build Information) even though that page is intentionally
absent from the page 0x00 supported-pages list — it is reachable but
not advertised.

---

## SECURITY PROTOCOL (Tape Data Encryption, protocol 0x20)

This is the SCSI surface for Application-Managed Encryption (AME).
AME means the *host* owns the key: it pushes the key down over
SECURITY PROTOCOL OUT, VTL uses it for the session, and VTL never
writes it to disk.

The whole SPC-4 SECURITY PROTOCOL opcode is optional. The
conditional-mandatory rule is the inverse, though: once a target
chooses to advertise Tape Data Encryption (protocol 0x20), the
individual SPSP pages that make encryption usable become mandatory.
VTL does advertise protocol 0x20, so it implements those pages, and
the supported-list SPSP returns exactly `[0x0000, 0x0001, 0x0010,
0x0011, 0x0020, 0x0021]`.

| SPSP | Direction | Name | Status | Spec |
|-----:|-----------|------|--------|:----:|
| 0x0000 | IN | Tape Data Encryption In Support | 🟩 Yes | CC |
| 0x0001 | IN | Tape Data Encryption Out Support | 🟩 Yes | CC |
| 0x0010 | IN | Data Encryption Capabilities | 🟩 Yes (AES-256-GCM) | CC |
| 0x0010 | OUT | Set Data Encryption | 🟩 Yes | CC |
| 0x0011 | IN | Supported Key Formats | 🟩 Yes (plaintext only) | CC |
| 0x0020 | IN | Data Encryption Status | 🟩 Yes | CC |
| 0x0021 | IN | Next Block Encryption Status | 🟩 Yes | CC |

Read of an encrypted block without the correct key →
CHECK CONDITION + DATA PROTECT (0x07) + ASC/ASCQ 0x74/0x0C.

---

## Behavioral model & deliberate divergences

VTL presents a generic, spec-conformant surface: one SMC-3
medium-changer at LUN 0, plus N SSC-4 LTO-7/8 tape drives at LUN ≥ 1.
The guiding principle is **spec conformance, not chassis
impersonation** — VTL is not modeled after any particular physical
library. It can afford to be generic because of how backup software
actually identifies a device: it keys on the device *classes* —
`(SMC-3 medium-changer, SSC-4 LTO drive)` — by reading the INQUIRY
peripheral-device-type and probing supported opcodes. None of that
discovery depends on the identity of a specific chassis.

The coverage tables above answer *what* opcode or page is wired. This
section answers the questions a table cannot: how a command behaves
once accepted, and where VTL knowingly behaves unlike physical LTO
hardware. Departures that VTL shares with thurvsa are not repeated
here — they are in
[Part 1 (SPC-4 / SAM-5 / iSCSI)](#part-1-spc-4-sam-5-and-iscsi)
§ *Deliberate non-conformance — shared*.

### LTO-generation features we don't model

Several LTO-generation features have no meaningful virtual
equivalent — they defend against physical-tape failure modes that
simply cannot occur in a VTL. Rather than fake them, VTL declines
each one explicitly, and the table records what an initiator sees
instead:

| Feature | Generation | Behavior on VTL |
|---|---|---|
| Archive Mode Unthread | LTO-7+ switchable | Not modeled — virtual unthread is instantaneous. |
| LTO-7 Type M media (`M8` barcode) | LTO-8 only | A physical-substrate artifact with no virtual equivalent. The `M8` suffix stays a valid label string, but the daemon does no LTO-generation inference from barcodes; cartridges carry their generation in the manifest. |
| LTO-7 cartridge creation | LTO-7 / LTO-8 | Refused — VTL ships as a clean LTO-8 drive. REPORT DENSITY SUPPORT still advertises LTO-7 RO as a secondary descriptor (matching real LTO-8 backwards-read advertisement), but no LTO-7 media is ever loaded. |
| End-to-end Logical Block Protection (CRC32C / Reed-Solomon) | LTO-7+ | Implemented for CRC32C (Castagnoli) — see Mode Page 0x0A/0xF0 above. Reed-Solomon not modeled. PROTECT=0 in standard INQUIRY; discovery is via the mode page. |
| WORM tamper detection (WTRE field, EOPD value) | LTO-7+ | Declined — we honor WORM at-EOD-only writes (the actual integrity guarantee); WTRE / EOPD out-of-band signaling has no physical tape to defend. |

VTL emulates LTO-7 and LTO-8 only, which is what keeps it aligned
with the SPC-4 / SSC-4 / SAM-5 conformance target. The generations on
either side are out of scope: LTO-5 and LTO-6 predate LTO-7, while
LTO-9 and SSC-5 introduce RAO and larger capacities that would pull
in a different spec baseline. Both ends are declined at the CLI.

### Deliberate divergences from typical LTO hardware

#### PERSISTENT RESERVE OUT (0x5F) — implemented on both LUN types

Earlier releases rejected PROUT on the tape drive, on the theory that
a VTL is single-initiator and a no-op accept could mislead a clustered
host into believing it held a fence. That reasoning was too cautious:
`MaxConnections=1` only forbids multiple connections *within* one
session — distinct initiators still open distinct sessions (distinct
TSIH) to the same drive, so a reservation genuinely arbitrates between
them. As of issue #16 the drive LUN (LUN ≥ 1) implements the full
PERSISTENT RESERVE family for real, backed by the same shared
`scsi_spc::reservations::ReservationManager` the block target uses:
REGISTER / RESERVE / RELEASE / CLEAR / PREEMPT / PREEMPT AND ABORT /
REGISTER AND IGNORE EXISTING KEY, truthful PRIN, and RESERVATION
CONFLICT (status 0x18) on medium read/write opcodes for a non-permitted
nexus. PROUT (0x5F) appears in the drive's REPORT SUPPORTED OPERATION
CODES. The full behavior — service actions, the enforcement opcode
set, the (IQN, ISID) initiator-port keying, APTPL persistence, and the
survive-nexus-loss semantics — is in
[Part 1](#part-1-spc-4-sam-5-and-iscsi)
§ *Reservations — thurvtl tape vs thurvsa block*.

Issue #53 extends the same surface to the **medium changer (LUN 0)** —
SMC-3 changers legitimately support PERSISTENT RESERVE, and it matters
when multiple media servers share one library's robotics. Because
`ReservationManager` is keyed by LUN, the changer's reservation is
**independent** of every drive's. The changer's enforcement gate
(`scsi_smc::dispatch::pr_enforce`, the mirror of the drive's
`pr_enforce` in `scsi-ssc`) returns RESERVATION CONFLICT to a
non-permitted nexus for the movement / inventory opcodes — MOVE
MEDIUM 0xA5, EXCHANGE MEDIUM 0xA6, INITIALIZE ELEMENT STATUS
0x07 / 0x37, SEND VOLUME TAG 0xB6 (write-side), and READ ELEMENT
STATUS 0xB8, REQUEST VOLUME ELEMENT ADDRESS 0xB5 (read-side).
Identity / status / mode pages / the PR commands themselves are never
fenced (SAM-5 §5.9.1). PROUT (0x5F) appears in the changer's REPORT
SUPPORTED OPERATION CODES alongside the drive's.

#### Operator-driven configuration changes — UAs not broadcast

When an operator reconfigures a physical library, the library raises
a `06/2A/00 MODE PARAMETERS CHANGED` Unit Attention to every host
currently connected. VTL cannot reproduce that exactly, and for a
benign reason: chassis topology lives in the YAML `library:` block,
the daemon reconciles it at start-up, and changes only take effect
on restart — so by the time the topology actually changes there is
no host connected to notify. What the hosts see instead is the next
thing that happens — on daemon restart they get a `06/29/00 POWER ON
RESET`. That carries broader semantics ("something changed,
re-discover everything") rather than the narrower "the topology
changed," which is the correct signal here anyway.

#### Appliance-side at-rest encryption (added on top of AME)

A real LTO drive offers exactly one form of encryption: host-driven
AME over SSC-4 SECURITY PROTOCOL OUT. VTL keeps that, and adds a
second, **opt-in** encryption layer that a physical drive has no
analog for. This layer runs entirely daemon-side, in the gap between
chunk-seal and pool insertion: once a chunk is sealed, the whole
chunk is wrapped in AES-256-GCM under a per-cartridge DEK before it
is allowed into the local pool or the storage backend. Crucially, the
SCSI surface does not change — host INQUIRY, the encryption-status
pages, and MODE SENSE 0x10/0x01 all report exactly what they would
for a plaintext cartridge, because this layer is invisible to the
host.

When both layers are switched on they compose, not collide. On write,
AME runs first and per block, using the host's key and a per-block IV
of `derive_iv(uuid, chunk_id, offset)`; then, at seal time, the
appliance-side layer encrypts the already-AME-ciphertext chunk as a
whole, using the daemon-managed DEK and a per-chunk IV of
`derive_iv(uuid, chunk_id, 0)`. On read the two unwrap in the reverse
order. The whole feature is opt-in per cartridge — `cartridge create
--encrypt --keystore NAME` — and the choice is recorded in
`manifest.encryption`. The deeper treatment is in
[`CARTRIDGE.md`](../admin/CARTRIDGE.md) § *At-rest encryption (appliance-side)*
and [`ENCRYPTION.md`](../admin/ENCRYPTION.md) § *VTL keystore backends*.

#### MAM attributes — host writes persisted in a sidecar

A real LTO drive stores host-written MAM attributes in the cartridge's
on-medium Medium Auxiliary Memory chip (LTO-CM). VTL has no CM chip, so
it persists host-written attributes in the cartridge's `runtime.json`
sidecar instead (issue #60); functionally the host sees the same
contract — WRITE ATTRIBUTE of a host-range id (0x0800–0x0BFF,
0x1400–0x17FF, including barcode 0x0806) round-trips through
UNLOAD/reload via READ ATTRIBUTE, and a zero-length write deletes the
id. The device/medium attributes (capacity, load count, medium
manufacturer 0x0400 / serial 0x0401) are synthesized read-only, as on
real hardware.

The barcode is worth a word, because it lives on two surfaces that a
real library keeps separate and so do we. The **authoritative**
cartridge barcode is the medium changer's volume tag — what READ
ELEMENT STATUS reports — which on real hardware is the robot's optical
read of the printed label and in VTL is the cartridge label, fixed
out-of-band at `cartridge create`. The drive cannot write it (a drive
has no optical reader, and you cannot rewrite a printed label over
SCSI). **MAM attribute 0x0806** is the separate, host-writable copy of
the barcode that an application records on the medium itself; like a
real CM it starts absent and is populated only if a host writes it
(typically the backup app's label step copies the value it read from
READ ELEMENT STATUS into 0x0806). VTL therefore does not synthesize
0x0806 and does not couple it to the volume tag — the two may differ,
exactly as they may on real hardware.

### Things real LTO hardware does that we don't model

Beyond the specific divergences above, a physical library carries a
good deal of behavior that is purely an artifact of being made of
moving parts and sheet metal. None of it has a virtual equivalent, so
VTL either reports a benign placeholder or does nothing at all:

- **Multi-partition libraries** — see § Multi-partition libraries
  below.
- **Operator control panel** — no front-panel display, OCP key
  state, or operator-cancel flow. "Operator intervention required"
  conditions (jam, sled removed) just don't happen in a VTL.
- **Robot motion timing** — real MOVE MEDIUM takes seconds (worst
  case minutes); VTL completes in microseconds. Backup software
  using motion time to detect a hung robot never trips.
- **Hardware-state log pages** — fan RPM, PSU voltage, module
  temperature, power-cycle count emitted as zeros / unknown on the
  few standard log pages we implement (0x0D Temperature reports
  25 °C). Vendor-specific 0xC0-0xFF log pages not emitted at all.
- **Cleaning cartridges** — the ACE (Auto Clean Enabled) bit in the
  Device Capabilities page is 0 (no head to clean). Cleaning
  cartridges identified by barcode suffix (e.g. `CLN001CU`) are
  reported with Medium Type 2 in element descriptors; the changer
  doesn't actually consume them.
- **NVRAM-resistant event log** — no event log on the changer LUN;
  the audit log (`<data_dir>/audit/`) covers this functionally but
  isn't surfaced over SCSI.
- **Background self-tests with motion timing** — real LTO drives
  advertise SELF-TEST CODE 0b001 / 0b010 (background short /
  extended) and 0b101 / 0b110 (foreground). VTL accepts every code
  as GOOD without execution — only SELFTEST=1 runs an actual probe,
  a purely software check (parse `library.json` + `inventory.json` +
  every cartridge `manifest.json` + `validate_object_store_backend` for
  LU0; cartridge `manifest.json` for LU1+). It completes in
  milliseconds; hosts polling LOG SENSE 0x10 never see an
  in-progress state.

#### Multi-partition libraries

A physical library can be carved into N logical libraries, each
behaving like an independent device with its own SCSI serial and its
own element address space. VTL implements a software-level analog of
this. The `library.json::partitions` field splits the chassis into N
logical partitions, each owning a disjoint set of storage, mail, and
drive elements. A session is bound to one partition through its CHAP
credentials — the `partition:` field on a CHAP user in
`<data_dir>/iscsi-users.json`. Once bound, the fence is enforced
several ways: MOVE MEDIUM and EXCHANGE MEDIUM refuse to reach
elements outside the partition, REPORT LUNS hides drives that belong
to other partitions, and an out-of-partition drive-LUN INQUIRY
returns the SPC-4 "no logical unit" sentinel (peripheral qualifier
0b011 / peripheral type 0x1F) — the kernel iSCSI initiator's
all-LUNs post-login probe skips the LU and keeps scanning, instead
of fail-stopping on CHECK CONDITION. READ ELEMENT STATUS is the one
intentional exception: it returns the full chassis to every session,
because `mtx(1)` parses a zero-descriptor per-type page as
`Transport Element Descriptor Length too short` and aborts. The
resulting leak is topological only — a session can *learn* that
other elements exist, but the data path stays isolated (drive-LUN
INQUIRY blocks the SCSI surface; MOVE/EXCHANGE MEDIUM blocks the
robot path). Underneath, there is still a single IQN and a single,
global element-address space — the chassis namespace is not actually
subdivided — but the addressable SCSI surface the bound session sees
behaves as though out-of-partition data-path elements did not exist.

**Distinct serials per partition.** So that a host treats each
partition as a separate library, the identity values are made
partition-specific. The changer LUN's VPD `0x80` Unit Serial Number
becomes `<chassis_serial>_LL<NN>` (with `NN` the 1-based partition
index), the VPD `0x83` NAA-3 identifier is computed as
`BLAKE3(chassis‖lun‖partition)`, and a Logical Unit Group descriptor
carries a per-partition group ID — the value backup software uses to
auto-correlate "these LUNs belong to one logical library."

**Cross-partition operator authority.** Changer operations issued
over the admin socket refuse a cross-partition move by default; an
operator who genuinely intends one passes `cross_partition: true` as
an explicit override, and every override is tagged in the audit log.
Two things still differ from real hardware: there is no per-partition
front-panel navigation, and there is one physical chassis serial
rather than a distinct serial per partition — partition identity is
layered on top via the `_LL<NN>` suffix and the LUG group ID instead.
Leaving the `partitions` list empty reverts the whole library to
single-partition mode, with no fence at all.

### Topology bounds

| Element type | Default first address | Cap | Source of cap |
|---|---:|---:|---|
| Storage slots | 1001 | 65535 | 16-bit SMC-3 element address |
| Mail (I/E) slots | 101 | 65535 | 16-bit SMC-3 element address |
| Data Transfer (drives) | 1 | 255 | iSCSI single-byte LUN encoding |
| Medium Transport (robot) | 0 | 1 | one robot per library |

An SMC-3 element address is a 16-bit value (0..=65535), which sets
the absolute ceiling for storage and mail slots at 65535 each. But
that 0..=65535 space is shared: all four element ranges — transport,
storage, mail, drives — have to fit inside it without overlapping.
`validate_element_address_layout` in
`core/mediachanger/src/library/mod.rs` checks exactly that when the
daemon materializes `library.json` on first start. The practical
consequence is that the ceilings
are not simultaneously reachable — configuring 65535 storage slots
leaves no addresses for the other three element types.

The drive cap of **255** is a tighter, and unrelated, limit — it
comes from the iSCSI transport rather than from SMC-3. In
`shared/iscsi/src/transport.rs` the daemon parses the 8-byte LUN
field of a SCSI Command PDU under SAM "peripheral device addressing"
(Method 0): only `lun[1]` carries the LUN number. That single byte
yields 256 LUNs total; LUN 0 is reserved for the changer, which
leaves 1..=255 for drives. Whatever counts the topology is configured
with, the SCSI surface reports them back through MODE SENSE page 0x1D
(Element Address Assignment), derived live from the topology rather
than stored separately.

Because only `lun[1]` is significant, a malformed LUN with a non-zero
`lun[0]` (the addressing-method / bus field) or any non-zero byte in
`lun[2..8]` is **rejected**, not aliased: `decode_lun` maps it to an
out-of-range sentinel so the dispatcher answers CHECK CONDITION /
ASC 0x25 (LOGICAL UNIT NOT SUPPORTED). This stops a host from, say,
addressing `0x01_00` and silently landing on LUN 0 (the changer) by
having the high byte dropped.

### Firmware revision identity

The 4-byte ASCII revision an initiator reads at INQUIRY byte 32..36
defaults to a per-LTO-generation signature: `TVL7` for LTO-7, `TVL8`
for LTO-8, and `TVL0` as a fallback for any other generation, all set
by `default_firmware_for_lto`. The decision worth understanding here
is what VTL deliberately does *not* do: it does not impersonate a
real drive vendor's firmware revision. Claiming a revision string
that belongs to actual hardware would, in effect, inherit that
revision's published CVEs and the known-bug workarounds initiators
apply against it. When a backup-software compatibility matrix
genuinely insists on a specific code, the operator can set it
explicitly via `library.firmware: <CODE>` in `thurvtl.yaml` (and
restart the daemon — the reconcile engine applies firmware changes
freely) rather than have it faked by default. Whichever string is in
effect is reported identically on the changer LUN and on every
tape-drive LUN, since they all read it from the same
`LibraryTopology.firmware`.

---

## How this table stays honest

As in Part 1, the tape-side tables are anchored to the code that
backs them. Opcode dispatch starts in
[`../../vtl/daemon/src/iscsi/protocol.rs`](../../vtl/daemon/src/iscsi/protocol.rs)
(`handle_scsi_command` / `dispatch_scsi`). From there, the per-page
handlers live in
`scsi/ssc/src/scsi/{mode_pages.rs, log_pages.rs, encryption_pages.rs,
attributes.rs}`, the drive handlers that touch library state are in
`scsi/ssc/src/dispatch/handlers.rs`, and the changer dispatch is in
`scsi/smc/src/dispatch/`. The CHAP / iSCSI login / PDU-framing layer
underneath all of it is the shared one, in
`shared/iscsi/src/{transport.rs, auth.rs, session.rs,
unit_attention.rs}`.

The same discipline applies: every new tape-side opcode, VPD page,
mode page, log page, or service action — and every explicit
rejection — is reflected in this table in the same commit. SPC-4 /
SAM-5 / iSCSI / CHAP changes belong in Part 1; SBC-3 and thurvsa-side
VPD in Part 3; NVMe in
[`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md).


---

# Part 3: SBC-3 (VSA)

## SBC-3 — Direct-Access Block Commands

| Opcode | Command | Status | Spec | Notes |
|-------:|---------|--------|:----:|-------|
| 0x25 | READ CAPACITY (10) | 🟩 Yes | M | Caps `LAST LBA` at `0xFFFFFFFF` so big volumes force the host to RC16. Reports the live size, so a re-issue after an online `volume resize` reflects the new capacity (issue #76). |
| 0x9E sa 0x10 | READ CAPACITY (16) | 🟩 Yes | M | Full 8-byte last LBA. Byte 14 carries LBPME=1 + LBPRZ=1 (thin-provision hint, unmapped reads zero). Reports the live size — see *Online resize* below. |
| 0x28 | READ (10) | 🟩 Yes | M | Sub-page supported via cache RMW. Unallocated pages return zeros (sparse holes). Reservation-gated. |
| 0x2A | WRITE (10) | 🟩 Yes | M | Sub-page supported via cache RMW. WORM volumes refuse with WRITE PROTECTED. Reservation-gated. |
| 0x2F | VERIFY (10) | 🟩 Yes | O | BYTCHK=00 reads the requested range to surface medium errors (sparse-hole pages succeed). BYTCHK=01 compares Data-Out against on-medium bytes; mismatch surfaces as MISCOMPARE (sense key 0x0E, ASC/ASCQ 0x1D/0x00). BYTCHK=10/11 rejected with INVALID FIELD IN CDB. VRPROTECT must be 0. Reservation-gated as a read-side opcode. |
| 0x35 | SYNCHRONIZE CACHE (10) | 🟩 Yes | O | Real fence — `cache.synchronize_bytes` awaits the cache's flush of every dirty page in the requested LBA range through to storage-backend ack via `VolumeWriter::write_page`. Reservation-gated as a write-side opcode. |
| 0x41 | WRITE SAME (10) | 🟩 Partial | O | VAAI Block Zero / `blkdiscard --zeroout` primitive. Data-Out is one logical block (the per-sector pattern); the daemon expands it across the requested range. UNMAP=1 with a zero pattern routes via `cache.unmap_bytes`; other patterns expand and route via `cache.write_bytes` in 16 MiB sector-aligned chunks. ANCHOR / WRPROTECT / PBDATA / LBDATA rejected with INVALID FIELD IN CDB. NUMBER OF BLOCKS = 0 is a no-op per SBC-3 §5.49. WORM refuses with WRITE PROTECTED. Reservation-gated. |
| 0x42 | UNMAP | 🟩 Yes | O | 8-byte header + N × 16-byte UNMAP BLOCK DESCRIPTOR. Sub-page descriptors zero the affected sectors via cache RMW; full-page descriptors clear `PageIndex` entries (backend chunks linger until `system gc`). ANCHOR=1 rejected. Two-phase commit (validate every descriptor before any clear). WORM refuses with WRITE PROTECTED. Reservation-gated. Advertised via VPD 0xB0, VPD 0xB2 (LBPU=1, LBPRZ=001, PROVISIONING TYPE=thin), and RC16 LBPME=1. |
| 0x83 sa 0x00 | EXTENDED COPY (LID1) | 🟩 Partial | O | The VAAI Hardware Accelerated Copy primitive — SPC-3 §6.3 LID1 subset, what ESXi and Windows VAAI issue. Identification target descriptors (type 0xE4) carrying NAA designators (designator type 0x03, 8 bytes, from VPD 0x83's NAA descriptor); block-to-block segment descriptors (type 0x02) with 16-bit block count + 64-bit src / dst LBAs. LID4 (sa 0x01) is implemented separately (same descriptor subset, 48-byte header — see the next row); T10 identification descriptors and other segment descriptor types reject as INVALID FIELD IN PARAMETER LIST. Per-segment fast path: same backend + matching pool namespace + page-aligned + non-overlapping + (for encrypted volumes) matching crypto identity (`dek_uuid`) at the same page offset → `PageCache::clone_page_range_into` rebinds the destination's page-index entry to the source's chunk hash, zero data I/O. Cross-LUN clones share the fast path under those conditions. Anything else — mismatched pool, unaligned, distinct encrypted identity, cross-page-offset (the AES-GCM IV folds in `page_id`), or enc↔unenc — falls back to the 1 MiB streaming bytes copy, which *recrypts* (decrypts under the source identity, re-encrypts under the destination), so a cross-identity copy can never leave an undecryptable destination page (issue #88). Synchronous (whole copy completes before GOOD). Destination LUN reservation-gated; WORM destinations refuse with WRITE PROTECTED. Advertised via VPD 0x8F and REPORT SUPPORTED OPERATION CODES. |
| 0x83 sa 0x01 | EXTENDED COPY (LID4) | 🟩 Partial | O | SPC-4 §6.4. Same block-to-block subset as LID1 (0xE4 target descriptors, 0x02 segment descriptors, shared resolution + execution path) behind the richer 48-byte LID4 header. LIST FORMAT must be 0x01; HEADER CSCD LIST LENGTH and INLINE DATA LENGTH must be 0 (header CSCDs / inline data unmodeled → INVALID FIELD IN PARAMETER LIST); the IMMED flag is accepted and ignored (the copy still runs synchronously). No production initiator issues LID4 — implemented for SPC-4 completeness. |
| 0x83 sa 0x10 | POPULATE TOKEN (ODX) | 🟩 Partial | O | The Hyper-V / Windows ODX primitive. CDB carries a 32-bit LIST IDENTIFIER + parameter list pointer; parameter list is a 16-byte header (INACTIVITY TIMEOUT, BDRD list length) + N × 16-byte Block Device Range Descriptors. Source LUN is the addressed LUN (no target descriptors). Snapshots the per-page chunk hashes from `pages.idx` after flushing any in-range dirty pages, pins each unique chunk via `shared_pool::ChunkPool::pin` so eviction + GC skip them, mints a 512-byte ROD token (`OsRng`), and records a `Done` job + token under the LIST IDENTIFIER for RECEIVE ROD TOKEN INFORMATION to fetch. Sync-inline. Cap on range descriptors = 8 (matches VPD 0x8F descriptor 0x0000 advertisement). Vendor-specific token types (zero-fill, anchor) deferred — only the default point-in-time copy is honored. |
| 0x83 sa 0x11 | WRITE USING TOKEN (ODX) | 🟩 Partial | O | Applies a snapshot minted by POPULATE TOKEN to the addressed destination LUN. Parameter list = 16-byte header + 512-byte ROD token + 8-byte BDRD list length + N × 16-byte BDRDs. Token miss → ASC 0x23/0x07 INVALID TOKEN OPERATION, TOKEN INVALID; expired token → ASC 0x23/0x05 TOKEN NOT MAINTAINED. Source and destination must share a chunk pool (same backend + matching `DedupScope` namespace) and the same `page_size_bytes`; mismatch refuses with INVALID FIELD IN PARAMETER LIST (cross-pool ODX stays out of scope). Per destination page: when the destination can reconstruct the chunk's (key, IV) — both unencrypted, or both encrypted under the same `dek_uuid` with the page at the same offset (`core_block::rebind_is_sound`) — the page-index slot is rebound to the snapshot's hash, zero data I/O. Otherwise it is *recrypted* (issue #88): the pinned snapshot chunk is decrypted under the source identity (via the decrypt handle the token retained at POPULATE TOKEN, frozen point-in-time) and re-sealed under the destination identity (fresh `iv_salt`, real pool consumption, honest upload state) — so a cross-crypto-identity / cross-offset / enc↔unenc ODX never produces an undecryptable destination page. WORM destinations refuse with WRITE PROTECTED; reservation-gated. Job outcome recorded under the CDB's LIST IDENTIFIER for RRTI (TRANSFER COUNT reports committed blocks on a mid-apply failure). |
| 0x83 sa 0x12 | CANCEL ROD TOKEN (ODX) | 🟩 Yes | O | Invalidates the ROD token minted by the POPULATE TOKEN whose LIST IDENTIFIER (bytes 6-9) matches — drops the token entry (releasing its chunk pins so eviction + GC reclaim them ahead of the TTL) and forgets the job so a later RRTI reports "no operation in progress". Token identified by LIST IDENTIFIER; any parameter list is accepted and ignored. Cancelling a token no longer held (unknown / expired / never-minted) is a GOOD no-op per SPC-4 §6.5. |
| 0x84 sa 0x00 | RECEIVE COPY RESULTS — COPY STATUS | 🟩 Yes | O | 16-byte response. COPY MANAGER STATUS = 0x02 (operation completed without errors); per-segment accounting always zero (XCOPY is synchronous so no list ID tracking). |
| 0x84 sa 0x01 | RECEIVE COPY RESULTS — RECEIVE DATA | 🟩 Yes | O | Retrieves held data produced by inline-data / host-bound segment descriptors. We accept neither (MAXIMUM INLINE DATA LENGTH = 0), so there is never held data: the response is the bare 4-byte AVAILABLE DATA header set to zero. |
| 0x84 sa 0x03 | RECEIVE COPY RESULTS — OPERATING PARAMETERS | 🟩 Yes | O | Advertises our per-XCOPY limits — max target descriptors = 2, max segment descriptors = 1, max descriptor list length = 128 bytes, max segment length = 16 MiB, data segment granularity = log2(page_size). IMPLEMENTED DESCRIPTOR LIST: 0xE4 (identification target), 0x02 (block-to-block segment). |
| 0x84 sa 0x04 | RECEIVE COPY RESULTS — FAILED SEGMENT DETAILS | 🟩 Yes | O | 60-byte fixed copy-results header (EXTENDED COPY COMMAND STATUS at byte 56, SENSE DATA LENGTH at bytes 58-59, sense data at byte 60+). Synchronous XCOPY surfaces a failing segment inline as CHECK CONDITION on the EXTENDED COPY command and retains no per-LIST IDENTIFIER record, so this always reports "no failed segment" (command status 0, sense data length 0). Service action 0x05 is reserved in SPC-4 (no "operations count" action) and rejects with INVALID FIELD IN CDB. |
| 0x84 sa 0x07 | RECEIVE ROD TOKEN INFORMATION (ODX) | 🟩 Partial | O | Companion to POPULATE TOKEN / WRITE USING TOKEN. Looks up the job recorded under the CDB's LIST IDENTIFIER and emits the SPC-4 §6.21.2.3 response: 32-byte fixed header (RESPONSE TO SA = 0x10 for POPULATE TOKEN jobs / 0x11 for WRITE USING TOKEN; COPY OPERATION STATUS = 0x02 Done or 0x03 Failed; TRANSFER COUNT in blocks) + 4-byte ROD TOKEN DESCRIPTORS LENGTH + optional 4-byte descriptor header + 512-byte ROD token (POPULATE TOKEN jobs only). Misses (unknown list ID) return the "no operation in progress" header per spec. |
| 0x88 | READ (16) | 🟩 Yes | M | Same semantics as READ (10), 64-bit LBA. |
| 0x89 | COMPARE AND WRITE | 🟩 Yes | O | Atomic test-and-set per SBC-3 §5.2. Data-Out PDU is `2 * blocks * sector_bytes` (compare ‖ write). Diff returns CHECK CONDITION + MISCOMPARE (sense key 0x0E, ASC/ASCQ 0x1D/0x00) without committing the write. Sub-page CAW (1-sector VMFS heartbeat) honored end-to-end via cache RMW. Read+compare+write is atomic against other CAWs on the same LUN via `CawLocks`. Advertised via VPD 0xB0 MAXIMUM COMPARE AND WRITE LENGTH = sectors-per-page. |
| 0x8A | WRITE (16) | 🟩 Yes | M | Same semantics as WRITE (10), 64-bit LBA. |
| 0x8F | VERIFY (16) | 🟩 Yes | O | Same semantics as VERIFY (10), 64-bit LBA + 32-bit transfer length. |
| 0x91 | SYNCHRONIZE CACHE (16) | 🟩 Yes | O | Same semantics as 0x35, 64-bit LBA. |
| 0x93 | WRITE SAME (16) | 🟩 Partial | O | Same semantics as WRITE SAME (10) plus the NDOB bit (byte 1 bit 0): NDOB=1 means "no Data-Out, zero-fill." NUMBER OF BLOCKS = 0 means "from LBA to end of medium" per SBC-3 §5.50. Reservation-gated. |

---

## Online resize (capacity-change notification)

`thurvsa volume resize NAME --size N` changes a volume's logical capacity
while it is exported (grow: issue #76; shrink: issue #77). Grow is
metadata-only — the page table is sparse, so pages past the old end
already read as zero and the data-path range gate admits I/O into the
grown region the instant the size changes. The size is the live source of
truth (an in-memory shadow the daemon flips and persists to
`manifest.json`); READ CAPACITY (10/16), MODE SENSE block descriptors, and
the READ/WRITE range check all read it, so a connected host sees the new
capacity without a daemon restart.

Shrink is the riskier half and is **non-destructive by construction**: the
daemon flushes the page cache, then refuses if the volume is WORM, if a
persistent reservation is held, or if any allocated page sits at or beyond
the new last page — so a shrink can never silently drop data. The operator
frees the tail from the host first (resize the filesystem down, then
`fstrim`/`blkdiscard`, which UNMAPs and deallocates those pages), after
which the shrink succeeds and trims the page table (the orphaned chunks are
left for `system gc`, matching `volume destroy`). `--shrink-to-fit` snaps
to the smallest size that keeps every allocated page so the operator need
not compute the exact byte count; `--size` and `--shrink-to-fit` are
mutually exclusive.

To prompt the host to re-read, a resize raises a **CAPACITY DATA HAS
CHANGED** Unit Attention (sense key 0x06, ASC/ASCQ `0x2A/0x09`) on every
live iSCSI session, delivered by the same dispatch-level UA preemption as
RESERVATIONS PREEMPTED / MEDIUM MAY HAVE CHANGED — popped ahead of the
next command on each nexus (except INQUIRY / REQUEST SENSE / REPORT
LUNS). The host clears the UA and re-issues READ CAPACITY. The Linux SCSI
midlayer still needs an explicit rescan (`iscsiadm -m node --rescan` or
`echo 1 > /sys/block/sdX/device/rescan`) to resize the block device; the
UA invalidates the host's cached capacity but does not by itself trigger
a rescan. The same UA fires on a shrink. The
NVMe/TCP counterpart fires a Namespace Attribute Changed AER — see
[`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md) § Namespace-change
notifications.

---

## Dynamic LUN admission (REPORTED LUNS DATA HAS CHANGED)

A CHAP session is fenced to the volume set on its `UserEntry.volumes`
admission list (see SPC-4 REPORT LUNS, INQUIRY above): REPORT LUNS shows
only the LUNs of admitted volumes, and an un-admitted LUN answers as "no
LU" (PQ=0x3) to INQUIRY / TUR / READ CAPACITY / data-path opcodes.

That admission set is resolved **dynamically, per command** against a
live in-memory view, not snapshotted at login. An `iscsi users grant
USER --volume V` (or `revoke`) therefore takes effect on sessions that
are *already connected*: the next REPORT LUNS reflects the new set
without a re-login. The view is seeded from `iscsi-users.json` at boot
and updated in lockstep by the `iscsi users {add,grant,revoke,remove}`
admin handlers; a removed user's live session resolves to the empty set
(sees nothing), the safe fallback. Sessions that skipped CHAP
(`auth.method: None`) are unfenced and see every LUN, unchanged.

To prompt a connected host to re-enumerate, a grant / revoke raises a
**REPORTED LUNS DATA HAS CHANGED** Unit Attention (sense key 0x06,
ASC/ASCQ `0x3F/0x0E`) on every live session of the affected CHAP user,
on the LUNs that user is admitted to after the change — delivered by the
same dispatch-level UA preemption as CAPACITY DATA HAS CHANGED
(above), popped ahead of the next command on each nexus (except
INQUIRY / REQUEST SENSE / REPORT LUNS). The host clears the UA and
re-issues REPORT LUNS. As with capacity change, the Linux SCSI midlayer
still needs an explicit rescan (`iscsiadm -m node --rescan`) to attach a
newly-visible LUN's block device; the UA invalidates the host's view but
does not by itself trigger the rescan.

This is what lets the Kubernetes CSI driver use **one CHAP user per node**
(issue #15): all VSA volumes share one target IQN, so a node holds a
single iSCSI session, and each volume the node mounts is incrementally
granted to that node's user and picked up on the existing session by a
post-login rescan. See [`CSI.md`](../admin/CSI.md) § Per-node CHAP isolation.

---

## INQUIRY VPD pages — thurvsa block volume

A standard INQUIRY against a thurvsa volume reports a fixed
identity — vendor `MB`, product `THUR VSA`, revision `0001`. One
behavior is worth calling out: an INQUIRY aimed at a LUN that is not
mapped does not fail. Instead it returns the SPC-4 "no LUN" pattern —
peripheral qualifier 0b011 with peripheral device type 0x1F — which
lets an initiator walk the LUN map and learn which LUNs exist without
triggering a cascade of CHECK CONDITION responses.

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x00 | Supported VPD Pages | 🟩 Yes | CC | Lists `[0x00, 0x80, 0x83, 0x86, 0x8F, 0xB0, 0xB2]` for registered LUNs; `[0x00]` for the "no LUN" reply. |
| 0x80 | Unit Serial Number | 🟩 Yes | O | Hex-encoded volume UUID. |
| 0x83 | Device Identification | 🟩 Yes | M | LU-association descriptors per LUN: T10 vendor-based (8-byte vendor ID `MB` + ASCII `MBD_<uuid_hex>`) and NAA Locally Assigned (8 bytes binary: top nibble = NAA type 0x3, remaining 60 bits from the volume UUID's first 8 bytes). The NAA descriptor exists so EXTENDED COPY (0x83) target descriptors — which only have a 20-byte designator slot — can reference LUNs by NAA. ALUA TargetPort-association descriptors per advertised iSCSI portal: NAA-3 (`BLAKE3(target_iqn‖"\|tp\|"‖RTPI)`), Relative Target Port Identifier (RTPI), Target Port Group (TPGT). |
| 0x86 | Extended INQUIRY Data | 🟩 Yes | O | 64-byte page; all capability bits clear. TPGS for ALUA discovery lives in INQUIRY std-data byte 5, not here. |
| 0x8F | Third Party Copy | 🟩 Partial | O | The VAAI Hardware Accelerated Copy + Hyper-V ODX capability page. Sub-descriptors: 0x0000 ROD Device Type Specific Limits (MAX RANGE DESCRIPTORS = 8, DEFAULT INACTIVITY TIMEOUT = 300 s, MAX INACTIVITY TIMEOUT = 600 s, MAX TOKEN TRANSFER SIZE = 1 GiB / sector_size, OPTIMAL TRANSFER COUNT = 256 MiB / sector_size), 0x0001 SUPPORTED COMMANDS (opcodes 0x83 sa 0x00 / 0x10 / 0x11, 0x84 sa 0x00 / 0x03 / 0x07), 0x0004 PARAMETER DATA (max 2 target descriptors, max 1 segment descriptor, max 128-byte descriptor list, no inline data), 0x0008 SUPPORTED DESCRIPTORS (target type 0xE4, segment type 0x02, ODX block-device range type 0x00), 0x8001 GENERAL COPY OPERATIONS (1 concurrent copy, 16 MiB max segment, page-size log2 data granularity). |
| 0xB0 | Block Limits | 🟩 Partial | O | MAXIMUM COMPARE AND WRITE LENGTH = sectors-per-page (16 by default), OPTIMAL TRANSFER LENGTH GRANULARITY = sectors-per-page, MAXIMUM UNMAP LBA COUNT = 0xFFFFFFFF, MAXIMUM UNMAP BLOCK DESCRIPTOR COUNT = 4095, OPTIMAL UNMAP GRANULARITY = sectors-per-page, UGAVALID=1 with alignment LBA = 0. MAXIMUM WRITE SAME LENGTH = 0 (no specific limit); WSNZ=0. MAXIMUM TRANSFER LENGTH / OPTIMAL TRANSFER LENGTH / MAXIMUM PREFETCH LENGTH all zero. |
| 0xB2 | Logical Block Provisioning | 🟩 Partial | O | LBPU=1, LBPRZ=001 (unmapped LBAs read zeros), PROVISIONING TYPE=010 (thin). LBPWS / LBPWS10 / ANC_SUP / DP all zero. THRESHOLD EXPONENT = 0; no soft-threshold notification. |

---

## MODE pages — thurvsa block volume

| Page | Name | Status | Spec | Notes |
|-----:|------|--------|:----:|-------|
| 0x08 | Caching | 🟩 Yes | O | WCE=1 (write-back cache is real — `PageCache` flushes asynchronously via the storage uploader; SBC-3 §6.4.6.4 mandates WCE=1 when cached writes can be lost on power-cycle, so a compliant initiator issues SYNCHRONIZE CACHE on `sync(1)` / `umount`). RCD=1 (no read cache), DRA=1 (no read-ahead). Block descriptor reflects the volume's `(NUMBER OF LOGICAL BLOCKS, LOGICAL BLOCK LENGTH)` — short form by default, long form with MS10 LLBAA=1. WORM volumes flip WP=1 in the DEVICE-SPECIFIC PARAMETER byte. |
| 0x0A | Control | 🟩 Yes | O | SPC-4 baseline body (TST=0, D_SENSE=0, QUEUE ALG MOD=0). |
| 0x3F | All Pages | 🟩 Yes | O | Concatenation of supported pages above. |

None of these pages have host-tunable fields, and the MODE SENSE /
MODE SELECT behavior reflects that. A PC=Changeable request returns
an all-zero mask, and PC=Current, PC=Default, and PC=Saved are all
equivalent because there is no MODE SELECT saved-state surface to
make them differ. MODE SELECT 6 / 10 (0x15 / 0x55) is still accepted,
but only as a no-op confirmation: the parameter list has to re-assert
exactly the values MODE SENSE just returned. PF=1 is required, SP=1
is rejected with SAVING PARAMETERS NOT SUPPORTED, and since every
Changeable bit is zero an initiator cannot flip WCE, RCD, DRA, or
D_SENSE.

---

## At-rest encryption (AES-256-GCM)

Unlike SSC-4, which gives tape a real SCSI surface for encryption in
SECURITY PROTOCOL OUT, SBC-3 defines no equivalent for block devices.
thurvsa therefore implements at-rest encryption **entirely
daemon-side**, with nothing exposed on the wire. The operator opts in
at `volume create`; the daemon then either mints a per-volume AES-256
key itself or takes one supplied via `--key-file`, and from that
point on every page is AES-256-GCM encrypted before it reaches the
chunk pool and backend upload pipeline. Because none of this is visible
over SCSI, the SCSI surface is unchanged.

**Operator surface:**

| Flag | Effect |
|------|--------|
| `--encrypt` | Enables at-rest encryption; required together with `--keystore`. The daemon mints a fresh 32-byte AES-256 key via `OsRng` and wraps it with the keystore backend at create time (unless `--key-file` supplies one). |
| `--keystore NAME` | Keystore backend (`keystore.backends:` entry) that wraps the volume DEK. Required when `--encrypt` is given. |
| `--key-file PATH` | Operator supplies the DEK in PATH (64 hex chars + optional newline). Requires `--encrypt`. |
| `--dek-source MODE` | `daemon` (default) or `backend` — where DEK entropy comes from. Requires `--encrypt`. |

The key is read once, at create time, and then persisted by the
daemon — the operator's `--key-file` path is never consulted again.
There is no rotate-in-place: to rotate or to restore a key the
operator backs up `<data_dir>/keys/` alongside the volume manifest,
and a cross-region DR scenario additionally needs the keystore itself
restored separately. The gap is tracked in the issue tracker.

**Key custody:**

- Files at `<data_dir>/keys/<volume_uuid_hex>.key`, mode 0600, owned
  by the `thurvsa` system user. Directory 0700.
- Format: 64 hex chars + newline.
- The manifest records `encryption: { algorithm: "aes_256_gcm" }`
  and nothing else. A stolen manifest is useless without the
  keystore.

**On the data path** ([`../../core/block/src/uploader.rs`](../../core/block/src/uploader.rs)):

- **Write:** plaintext page → AES-256-GCM encrypt with IV =
  `derive_iv(crypto_uuid, page_id, iv_salt)` → ciphertext+tag
  (page_size + 16 B) → BLAKE3-hash → chunk pool insert → backend
  upload. `crypto_uuid` is the volume's crypto identity (`dek_uuid()`:
  its own `uuid`, or the inherited source identity for a clone — issue
  #86). `iv_salt` is a fresh random per-seal value persisted in the
  page's `pages.idx` record (issue #87), so every rewrite and every
  divergent clone write gets a unique nonce.
- **Read:** chunk pool / storage fetch → ciphertext+tag → AES-256-GCM
  decrypt with the IV re-derived from the page record's stored
  `iv_salt` → plaintext page → SCSI READ buffer.
- The IV itself is never stored; only its `iv_salt` input lives on
  disk (in `pages.idx`). A pre-salt (v1) record reads `iv_salt = 0`,
  reproducing the original IV so existing encrypted volumes keep
  decrypting. Same re-derivation pattern as VTL tape AME
  ([`../../core/stream/src/block_index.rs`](../../core/stream/src/block_index.rs)).
- Key zeroized on `VolumeWriter::Drop` (volume close, daemon
  shutdown).

**Threat model.** The protection this layer offers is specific: it
keeps ciphertext meaningful only against an attacker who has read
access to the storage backend or the daemon's data directory but *not*
the daemon's key file. It does **not** defend against a fully
compromised thurvsad host, where the running daemon holds the
key in memory anyway. Stronger custody — KMIP, an external KMS, or
per-volume HSM custody — is forward work, tracked in TODO §
Encryption-key management.

**Dedup interaction.** Each encrypted volume uses its own key and its
own per-page IVs, so the same plaintext page on two volumes always
serializes to different ciphertext and the BLAKE3 hashes never
collide across volumes. This means `--dedup global` on an encrypted
volume is accepted but achieves no actual cross-volume sharing — and
that is intentional, because sharing encrypted chunks between volumes
would punch a hole straight through the encryption boundary.

**EXTERNAL mode.** SSC-4's SECURITY PROTOCOL OUT has an `EXTERNAL`
encryption mode for ciphertext pass-through; thurvsa has no analog
for it. It neither advertises the mode nor would honor a request to
pass ciphertext through unencrypted-by-the-daemon.

---

## Deliberate non-conformance — thurvsa block

The departures below are specific to the thurvsa block target. The
cross-cutting ones that thurvsa shares with VTL are not repeated
here — they are in
[Part 1 (SPC-4 / SAM-5 / iSCSI)](#part-1-spc-4-sam-5-and-iscsi) §
"Deliberate non-conformance — shared".

| Item | Why |
|------|-----|
| Legacy SPC-2 RESERVE / RELEASE (6 / 10) absent | SBC-3 doesn't require them; Windows / VMware / Linux clusters use the SCSI-3 PR family exclusively. |
| PROUT REGISTER AND MOVE (SA 0x07) rejected | thurvsa is single-port; the multi-port SA has no analog. |
| MAXIMUM WRITE SAME LENGTH = 0 | No specific limit advertised. The host-side block layer or VAAI module sets its own ceiling. |
| LBP soft-threshold notification absent | THRESHOLD EXPONENT = 0 in VPD 0xB2; thin-provisioning is bounded by the storage backend's capacity, not a local pool watermark. |
| TASK MANAGEMENT FUNCTIONS via REPORT SUPPORTED TMF only | ATS / ATSS / CTSS / LURS / ITNRS advertised via MAINTENANCE IN SA 0x0D; the actual TMF dispatch is shared-iscsi's responsibility. |

---

## VAAI / ODX — what we model and what we don't

The VAAI XCOPY and Hyper-V ODX offload primitives share opcode `0x83`
(EXTENDED COPY) and `0x84` (RECEIVE COPY RESULTS); the rows above
list every SA we honor. The shaped table below catalogues the deliberate
gaps so an operator considering a hypervisor migration knows what falls
back to host-side READ/WRITE.

### Implemented

| Primitive | Spec | Notes |
|-----------|:----:|-------|
| VAAI Hardware Accelerated Copy (XCOPY LID1) | `0x83` sa `0x00` | Cross-LUN supported; zero-copy hash-rebind fast path requires matching backend + `DedupScope` namespace and (for encrypted volumes) matching crypto identity + page offset, else recrypts via the bytes path (issue #88). |
| XCOPY LID4 | `0x83` sa `0x01` | Same block-to-block subset as LID1 behind the 48-byte LID4 header; LIST FORMAT 0x01, no header CSCDs / inline data. No production initiator issues it. |
| VAAI Block Zero (WRITE SAME 10/16, NDOB / zero pattern) | `0x41` / `0x93` | UNMAP-bit zero patterns route through `cache.unmap_bytes`. |
| VAAI Atomic Test & Set (COMPARE AND WRITE) | `0x89` | Atomic against concurrent CAW on the same LUN. |
| VAAI Thin Provisioning Stun (advertise + UNMAP) | VPD 0xB0 / 0xB2, `0x42` | LBPME=1, LBPRZ=001, PROVISIONING TYPE=thin. |
| Hyper-V ODX POPULATE TOKEN | `0x83` sa `0x10` | 8 BDRDs max; default 5 min / max 10 min inactivity timeout. |
| Hyper-V ODX WRITE USING TOKEN | `0x83` sa `0x11` | Cross-LUN via page-hash rebind when sound; cross-crypto-identity / cross-offset / enc↔unenc recrypts the pinned snapshot chunks instead (issue #88); mismatched pool refuses with INVALID FIELD IN PARAMETER LIST. |
| Hyper-V ODX RECEIVE ROD TOKEN INFORMATION | `0x84` sa `0x07` | Sync-inline outcome surfacing; first poll always returns Done. |
| Hyper-V ODX CANCEL ROD TOKEN | `0x83` sa `0x12` | Frees the token's chunk pins + job ahead of the TTL; cancelling an unheld token is a GOOD no-op. |

### Deliberately deferred

| Primitive | Spec | Why deferred |
|-----------|:----:|--------------|
| VAAI Full Copy with inline data | `0x83` sa `0x00`, INLINE DATA LENGTH ≠ 0 | Inline-data segment descriptors are rejected; no host issues them. |
| VAAI UNMAP via WRITE SAME ANCHOR=1 | `0x41` / `0x93` ANCHOR bit | Anchored unmaps require backend-level reservations we don't model. |
| ODX vendor-specific ROD types (zero-fill, anchor) | various ROD TYPE values | Only the default point-in-time copy ROD type is honored; zero-fill is reachable through WRITE SAME instead. |
| ODX persisted tokens across daemon restart | n/a | Deliberate, not an effort gap: the ROD token table is in-memory, and a daemon restart drops the iSCSI / NVMe-TCP session, so the host aborts the in-flight ODX and falls back to a buffered copy before any restored token could be consumed — persisting it would rehydrate state nothing reads. This is also the industry norm: ROD tokens are ephemeral by design (Microsoft documents ~30-60 s typical lifetimes, SPC-4 lets the copy manager invalidate a token at any time, and Windows silently falls back to host copy then retries ODX after a short window). Real arrays — including dedup all-flash arrays — hold tokens in volatile memory and drop them on controller reboot. Tracked here for completeness. |
| ODX with mismatched source / destination pool | n/a | `WRITE USING TOKEN` refuses when source `(backend, DedupScope namespace)` differs from destination's. The slow-path copy-via-host-memory equivalent is reachable through XCOPY. |
| ODX with mismatched `page_size_bytes` between source and destination volumes | n/a | Hash-rebind only makes sense at equal page sizes. Mismatch refuses with INVALID FIELD IN PARAMETER LIST. |
| True async copy-status polling | n/a | Mostly deliberate: the common offload is a metadata-only page-hash rebind (microseconds, zero data movement) — nothing to report progress on — so XCOPY and ODX complete synchronously and RRTI returns Done on the first poll with ESTIMATED STATUS UPDATE DELAY = `0` ("poll immediately, already done"), which is spec-valid. The exception is the cross-crypto-identity / cross-offset / enc↔unenc **recrypt** path (issue #88), which moves real bytes (decrypt + re-encrypt per page). That still runs synchronously-inline, bounded by the advertised MAX TOKEN TRANSFER SIZE (1 GiB) — fast for pool-resident chunks, but a large recrypt of storage-only source chunks could approach an initiator command timeout. Making the recrypt path genuinely async (RRTI "in progress" + progress polling) so large cross-identity offloads stay within timeout is tracked as issue #89. Tracked here for completeness. |
| Extended XCOPY beyond 16 MiB / segment, > 8 ODX range descriptors, > 2 CSCD descriptors | advertised limits | Caps published in VPD 0x8F descriptors 0x0004 / 0x0000 / 0x8001; over-cap requests reject deterministically. |

---

## How this table stays honest

The block-side tables map back to the SBC-3 dispatcher at
[`../../scsi/sbc/src/dispatcher.rs`](../../scsi/sbc/src/dispatcher.rs)
(`SbcScsiDispatcher::dispatch`), which fans each opcode out to a
dedicated module:

- `scsi/sbc/src/data_path.rs` — WRITE / READ / VERIFY /
  SYNCHRONIZE CACHE / WRITE SAME / COMPARE AND WRITE / UNMAP.
- `scsi/sbc/src/mode_sense.rs` — MODE SENSE / SELECT.
- `scsi/sbc/src/reservations.rs` — PRIN / PROUT.
- `scsi/sbc/src/inquiry.rs` — INQUIRY std + VPD pages.
- `scsi/sbc/src/sizing.rs` — READ CAPACITY 10/16, REPORT LUNS, LUN
  encoding.
- `scsi/sbc/src/probes.rs` — host-probe stubs (REQUEST SENSE,
  START STOP UNIT, PREVENT/ALLOW, LOG SENSE).
- `scsi/sbc/src/maintenance.rs` — MAINTENANCE IN SAs 0x0C / 0x0D.

The data path those opcodes drive — the per-volume in-memory cache
and the storage-upload pipeline — is in
[`../../core/block/src/cache.rs`](../../core/block/src/cache.rs) and
[`../../core/block/src/uploader.rs`](../../core/block/src/uploader.rs).

The same rule closes the document as opened it: a new SBC-3 opcode,
VPD page, or mode page is reflected in this table in the same commit
that adds it. SPC-4 / SAM-5 / iSCSI / CHAP changes go in Part 1; NVMe
in [`CONFORMANCE_NVME.md`](CONFORMANCE_NVME.md).
