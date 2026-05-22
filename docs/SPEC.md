# Thur VTL Technical Specification

This document is the external technical reference for Thur VTL: it
pins down the SCSI device model, the iSCSI/SCSI surface, the on-disk
layout, and the cloud object layout. Think of it as the wire-level and
file-format contract — the bytes a host or a tool will actually see —
whereas CLAUDE.md covers the internal architecture. If what you need
is the behavioral picture instead — the device model, the deliberate
divergences from typical LTO hardware, and per-opcode conformance —
that lives in [`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md).

---

## SCSI Device Model

### Standards Conformance

When a host probes the device, these are the standards revisions it can
rely on the implementation to conform to:

| Surface | Standards |
|---------|-----------|
| Tape drives (LTO-8) | [SPC-4 r37](https://www.t10.org/cgi-bin/ac.pl?t=f&f=spc4r37.pdf), [SSC-4 r03](https://www.t10.org/cgi-bin/ac.pl?t=f&f=ssc4r03.pdf), [SAM-5 r21](https://www.t10.org/cgi-bin/ac.pl?t=f&f=sam5r21.pdf) |
| Medium changer | SPC-4, [SMC-3 r16](https://www.t10.org/cgi-bin/ac.pl?t=f&f=smc3r16.pdf), SAM-5 |
| iSCSI transport | [RFC 7143](https://datatracker.ietf.org/doc/html/rfc7143) (iSCSI Protocol, obsoletes [RFC 3720](https://datatracker.ietf.org/doc/html/rfc3720)), [RFC 7144](https://datatracker.ietf.org/doc/html/rfc7144) (iSCSI SCSI Features Update), [RFC 3721](https://datatracker.ietf.org/doc/html/rfc3721) (Naming and Discovery) |
| Authentication | CHAP per [RFC 1994](https://datatracker.ietf.org/doc/html/rfc1994), bound to iSCSI per [RFC 7143 §11](https://datatracker.ietf.org/doc/html/rfc7143#section-11) |

The conformance target is **SPC-4 / SSC-4 / SAM-5** — deliberately
*not* the later -5 / -5 / -6 revisions. The reason is that
LTO-8 drives, which is what Thur VTL emulates, advertise these
versions. LTO-9 firmware moves up to SPC-5 / SSC-5 and adds RAO along
with extra VPD and mode-page surfaces; none of that is in scope here.
You will sometimes see citations elsewhere in this document point at
SPC-5 or SSC-5 — that happens only where the field layout is identical
across revisions and the newer draft is simply easier to reference.
What the device actually advertises is the SPC-4 / SSC-4 / SAM-5 set
above.

### Library

| Field | Value |
|------|-------|
| Identity | Generic SMC-3 medium changer (not modeled after any specific physical chassis) |
| Vendor string (LUN 0) | `MB      ` (8 bytes, from `shared_naming::VENDOR_INQUIRY`) |
| Product ID (LUN 0) | `THUR VTL        ` (16 bytes, from `shared_naming::TAPE_LIBRARY_PRODUCT`) |
| Device type (LUN 0) | `0x08` (Medium Changer) |
| Serial (LUN 0, VPD 0x80) | `<chassis_serial>_LL<NN>` where `chassis_serial` is the persisted 14-byte string in `library.json` (default `TVLxxxxxxxxxxx` minted at init) and `NN` is the 1-based partition index — `_LL01` on unpartitioned libraries, `_LL02` for the second partition, etc. Sessions bound to different partitions see distinct serials. |
| Firmware revision | from `LibraryTopology.firmware`; default `TVL<gen>` (`TVL8`). Override via `library init --firmware <CODE>` for backup-software compatibility matrices that gate on a specific vendor firmware string. |

### Drives

| Field | Value |
|------|-------|
| Vendor string | `MB` (from `shared_naming::VENDOR_INQUIRY`) |
| Product ID | `Ultrium {N}-SCSI` (N = LTO generation; standard LTO Consortium product-family naming, not vendor-branded) |
| Device type | `0x01` (Sequential Access Device) |
| Serial | per-drive `mfg_serial` persisted in `inventory.json` (10-byte string), falling back to `DRV-NNN` (zero-padded LUN index) |
| Firmware revision | from `LibraryTopology.firmware`; default `TVL<gen>` |

### LTO Capacities

| LTO Gen | Native |
|---------|--------|
| LTO-8  | 12 TB  |

VTL ships as a clean LTO-8 drive: both `library init --lto-generation`
and `cartridge create --lto-generation` accept `8` only. The flag
exists at all only for forward-compatibility with LTO-9/10 (see
[`docs/LTO-9.md`](LTO-9.md)). REPORT DENSITY SUPPORT still
advertises the LTO-8 plus LTO-7-RO descriptor pair, which matches the
backwards-read advertisement a real LTO-8 drive makes — but LTO-7
media itself is never actually modeled.

Capacity enforcement happens in `Cartridge::write_data` and
`write_filemark`, both of which gate at the cartridge's **effective
capacity**. Effective capacity is `capacity_gb` scaled by whatever SET
CAPACITY proportion the host has set (CDB 0x0B bytes 2-3, persisted as
`set_capacity_proportion`). Two thresholds matter as a write run fills
the tape. At 95% the next write that would succeed instead raises
Early Warning — CHECK CONDITION + NoSense + EOM=1 + ASC/ASCQ
0x00/0x02 — and that latch is sticky once per pass to BOM, so the host
sees it once rather than on every block. At 100% writes are refused
outright with EndOfMedium (CHECK CONDITION + VolumeOverflow + EOM=1 +
0x00/0x02). Any of rewind, locate-to-BOM, erase, or SET CAPACITY
clears the Early Warning latch.

### Configuration Limits

| Element | Min | Max | Default |
|---------|----:|----:|--------:|
| Cartridge storage slots | 1 | 65535 | 40 |
| Mail slots (import/export) | 0 | 65535 | 0 |
| Tape drives | 1 | 255 | 3 |
| LTO generation | 7 | 8 | (no default — required at `library init`) |

The chassis topology lives in `<data_dir>/library/library.json`. That
file is created by `thurvtl library init` and can only be mutated
by `library modify` while the daemon is stopped. The contents of the
slots and drives — what cartridge sits where — live separately in
`<data_dir>/library/inventory.json`.

---

## iSCSI Configuration

| Param | Value |
|-------|-------|
| Target IQN | `iqn.2025-10.com.metebalci:thurvtl` (configurable via `iscsi.target_iqn`) |
| Portal Group Tag | 1 |
| Listen | `0.0.0.0:3260` (configurable via `iscsi.listen`) |
| Max sessions | 10 (configurable) |
| Session timeout | 300 s (configurable) |
| HeaderDigest | None or CRC32C (negotiated) |
| DataDigest | None or CRC32C (negotiated) |
| MaxRecvDataSegmentLength | 128 KiB; PDUs whose `DataSegmentLength` exceeds this cap are rejected pre- and post-login (memory-DoS guard) |
| ImmediateData / InitialR2T | `Yes` / `No` |
| FirstBurstLength | 128 KiB (= `MaxRecvDataSegmentLength`); the unsolicited window is sized to fit entirely in the SCSI Command PDU's data segment |
| MaxBurstLength | 16 MiB; one R2T-solicited burst can carry a full max-block tape WRITE without subdivision |
| MaxCmdSN window | `ExpCmdSN + 32`; non-immediate PDUs outside this window drop the connection rather than scramble tape semantics |
| AuthMethod | None or CHAP. Algorithms: `CHAP_A=5` MD5 (RFC 1994), `CHAP_A=6` SHA-1, `CHAP_A=7` SHA-256, `CHAP_A=8` SHA3-256 (the last three are de-facto extensions matching Linux LIO / open-iscsi numbering). Target advertises an `allowed_algorithms` list (default `[SHA3-256, SHA-256, SHA-1, MD5]`) and picks the strongest algorithm common to its list and the initiator's `CHAP_A` offer. Mutual CHAP supported and uses the same negotiated algorithm. Mutual-CHAP failure paths emit a Login Response with Status-Class=0x02 / Status-Detail=0x01 before closing, instead of a TCP RST |

### Host-to-target writes (Data-Out / R2T)

A WRITE-class command can carry more payload than fits in the
immediate data segment of the SCSI Command PDU. When the Expected Data
Transfer Length exceeds that segment, the rest of the data is moved
with the RFC 3720 §10.7-10.8 Data-Out / R2T flow, which works like
this:

- The SCSI Command's first `min(EDTL, MaxRecvDataSegmentLength)`
  bytes ride in the immediate data segment, as they always do.
- If the SCSI Command carries `F=0`, the target accepts unsolicited
  Data-Out PDUs (TTT=0xFFFFFFFF) until cumulative bytes reach
  `FirstBurstLength` or a Data-Out PDU sets `F=1`. Because the
  target is configured with `FirstBurstLength == MaxRecvDataSegmentLength`,
  this unsolicited phase is empty in practice for any initiator that
  fills the immediate segment — there is no room left for an
  unsolicited burst.
- Whatever data is still missing after the unsolicited burst, the
  target asks for explicitly: it issues R2T PDUs (opcode 0x31) in
  `MaxBurstLength` increments. Each R2T carries the Command's ITT, a
  fresh Target-Transfer-Tag (top bit set, never 0xFFFFFFFF), the
  current `BufferOffset`, the Desired Data Transfer Length, and a
  monotonic R2TSN. An R2T does not advance StatSN — it carries the
  StatSN of the next Status-bearing PDU instead.
- The initiator answers each R2T with one or more Data-Out PDUs whose
  ITT matches the Command's, whose TTT matches the outstanding R2T's,
  whose `BufferOffset` advances monotonically, and with `F=1` set on
  exactly the last Data-Out of each R2T burst. The target treats any
  deviation from this — a mismatched ITT/TTT/BufferOffset, an F-bit
  raised before the burst ends, a burst overrun, or a stray Data-Out
  at the top of the FFP loop — as a protocol violation and drops the
  connection. Recovery is left to the initiator: reconnect and retry
  the SCSI command.

### LUN Layout

- **LUN 0**: Medium Changer (always present)
- **LUN 1 … N**: Tape Drives (N = `library.num_drives`)

### SMC Element Addressing

The element addresses are fixed at `thurvtl library init` and
persisted in `library.json` alongside the slot and drive counts. They
are immutable after init, and for a good reason: barcoded inventory
entries reference these addresses directly, so changing them would
orphan every element currently loaded. The defaults below follow the
conventional element-address layout.

| Element type | Default first address | Count | `library init` flag |
|--------------|----------------------:|------:|---------------------|
| Medium Transport (robot) | `0`    | 1 | `--transport-base` |
| Data Transfer (drives)   | `1`    | `num_drives` | `--data-transfer-base` |
| Import/Export (mail)     | `101`  | `num_mail_slots` | `--import-export-base` |
| Storage (slots)          | `1001` | `num_storage_slots` | `--storage-base` |

Before it writes `library.json`, `library init` validates that the
four `[base, base+count)` ranges neither overlap each other nor run
past the end of the u16 SMC address space. If either check fails, it
returns `LibraryConfig` and writes nothing.

---

## SCSI Command Surface

### Common (all LUNs)

| Opcode | Command | Notes |
|-------:|---------|-------|
| 0x00 | TEST UNIT READY | |
| 0x03 | REQUEST SENSE | Fixed + descriptor format |
| 0x12 | INQUIRY | Standard + VPD pages |
| 0xA0 | REPORT LUNS | LUN 0 (changer) + LUN 1..N (drives) |
| 0x1A | MODE SENSE(6) | |
| 0x5A | MODE SENSE(10) | |
| 0x15 | MODE SELECT(6) | |
| 0x55 | MODE SELECT(10) | |
| 0x4D | LOG SENSE | |
| 0x4C | LOG SELECT | |
| 0x1E | PREVENT/ALLOW MEDIUM REMOVAL | cdb[4] bit 0 (data-transport) gates SCSI UNLOAD / MOVE MEDIUM-from-drive (refused with ILLEGAL REQUEST + ASC/ASCQ 0x53/0x02); bit 1 (mechanical) tracked but no enforcement target. State is per-I_T_L nexus (per TSIH+LUN), volatile, cleared on session close. |
| 0x1C | RECEIVE DIAGNOSTIC RESULTS | Pages 0x00 (Supported, lists [0x00, 0x10]) and 0x10 (Self-Test Results, SPC-4 §7.2.21 layout) |
| 0x1D | SEND DIAGNOSTIC | SELFTEST=1 routes by LUN: LU0 = library + inventory + cloud-backend health (full `validate_cloud_backend` probe); LU1+ = loaded-cartridge `manifest.json`. |

### Dispatch-level behavior

Two rules cut across every opcode and are therefore applied at the top
of `handle_scsi_command`, before per-opcode dispatch ever runs:

- **Unit Attention preemption.** Whenever a Unit Attention is pending
  for a given `(TSIH, LUN)`, every opcode is short-circuited to CHECK
  CONDITION with sense key 0x06 and the queued ASC/ASCQ — every opcode
  except the three the host needs precisely to discover what changed:
  INQUIRY (0x12), REQUEST SENSE (0x03), and REPORT LUNS (0xA0). This
  is exactly the behavior backup software depends on: after a
  MOVE/EXCHANGE MEDIUM it expects `0x06/0x28/0x00` (MEDIUM MAY HAVE
  CHANGED) and uses it as the cue to re-read inquiry data and element
  status.
- **Error → sense mapping.** When a handler returns a `Thur VTLError`,
  it is routed through `error_to_sense`. The point is to give the host
  a sense code that actually describes what went wrong — WORM,
  legal-hold, EOD, filemark, decryption, backpressure, not-ready, and
  so on — rather than collapsing everything into the default ILLEGAL
  REQUEST / INVALID COMMAND OPERATION CODE.

### Tape drive (SSC)

| Opcode | Command | Notes |
|-------:|---------|-------|
| 0x01 | REWIND | |
| 0x05 | READ BLOCK LIMITS | |
| 0x08 | READ(6) | Variable-block |
| 0x0A | WRITE(6) | Variable-block |
| 0x0B | SET CAPACITY | 6-byte CDB. CDB[2..4] = CAPACITY PROPORTION VALUE (16-bit BE; 0 = full native, 65535 = full native, intermediate = fraction). Persisted in the cartridge manifest as `set_capacity_proportion`. Erases the cartridge and rewinds to BOM, then gates subsequent WRITE / WRITE FILEMARKS at the host-set effective capacity: 95% raises Early Warning (CHECK CONDITION + NoSense + EOM=1 + ASC/ASCQ 0x00/0x02), 100% returns EndOfMedium (CHECK CONDITION + VolumeOverflow + EOM=1 + 0x00/0x02). EW is sticky-once-per-pass; rewind / locate-to-BOM / erase / SET CAPACITY clears the latch. IMMED bit ignored. |
| 0x10 | WRITE FILEMARKS(6) | Per-iteration errors (WORM / legal-hold) propagate as CC + sense |
| 0x11 | SPACE(6) | Records / filemarks / EOD; signed 24-bit count (sign-extended) |
| 0x13 | VERIFY(6) | |
| 0x19 | ERASE | |
| 0x1B | LOAD/UNLOAD (START STOP UNIT) | LOAD bit (cdb[4] bit 0) honored: 1 = rewind/load, 0 = unload (drops cartridge, emits CartridgeUnloaded) |
| 0x2B | LOCATE(10) | CP bit honored for partition switch |
| 0x34 | READ POSITION | Service Action 0x00 / 0x01: Short Form (20-byte response, 32-bit LBA, BPU=1 + zero LBAs when `position > u32::MAX`). 0x06: Long Form (32-byte response, 32-bit partition + 64-bit block / file / set numbers; MPU=1 since file/set numbers aren't tracked). 0x08: Extended Form (32-byte response, 8-bit partition + 24-bit buffer-block count + 64-bit first/last LBA + 64-bit buffer-byte count; LOCU+BYCU=1 since the virtual drive doesn't expose a host-visible write buffer). Other service actions → CHECK CONDITION + INVALID FIELD IN CDB. |
| 0x44 | REPORT DENSITY SUPPORT | LTO-7 / LTO-8 descriptors |
| 0x80 | WRITE FILEMARKS(16) | TRANSFER LENGTH at cdb[12..16] (4-byte BE u32) per SSC-4 §7.4 |
| 0x82 | ALLOW OVERWRITE | Volatile, cleared on UNLOAD |
| 0x8C | READ ATTRIBUTE | Barcode, serial, capacity |
| 0x8F | VERIFY(16) | |
| 0x91 | SPACE(16) | |
| 0x92 | LOCATE(16) | CP bit honored for partition switch |
| 0xA2 | SECURITY PROTOCOL IN | LTO AME pages 0x0020/0x0021/0x0100/0x0200 |
| 0xB5 | SECURITY PROTOCOL OUT | LTO AME page 0x0010 (Set Data Encryption) |

### Medium changer (SMC)

| Opcode | Command | Notes |
|-------:|---------|-------|
| 0x07 | INITIALIZE ELEMENT STATUS | |
| 0x16 | RESERVE (6) | Accepted as no-op — VTL doesn't track reservation state, returned for backup-software compatibility |
| 0x17 | RELEASE (6) | Accepted as no-op |
| 0x37 | INITIALIZE ELEMENT STATUS WITH RANGE | |
| 0x56 | RESERVE (10) | Accepted as no-op |
| 0x57 | RELEASE (10) | Accepted as no-op |
| 0xA5 | MOVE MEDIUM | Slot ↔ slot, slot ↔ drive, slot ↔ I/E |
| 0xA6 | EXCHANGE MEDIUM | Composed from two MOVE MEDIUMs |
| 0xB5 | REQUEST VOLUME ELEMENT ADDRESS | Stub (returns 8-byte empty header) |
| 0xB6 | SEND VOLUME TAG | Accepted as no-op |
| 0xB8 | READ ELEMENT STATUS | All four element types; honors VOLTAG / DVCID / Mixed bits |

#### READ ELEMENT STATUS descriptor extensions

These follow SMC-3. The base element descriptor is 12 bytes; when the
host requests them, the following extensions are appended in a fixed
order:

- **VOLTAG** (CDB byte 1 bit 4): adds a 36-byte volume-tag block (32-byte
  barcode space-padded + 4 reserved).
- **DVCID** (CDB byte 6 bit 0): on Data Transfer descriptors only. Adds
  a 38-byte block — 4-byte SMC-3 descriptor header + 34-byte ASCII
  identifier (8-byte vendor `MB`, 16-byte product `Ultrium <gen>-SCSI`,
  10-byte per-drive serial).
- **Mixed** (CDB byte 6 bit 7, vendor-specific extension): adds an
  8-byte Mixed-Media extension carrying Media Domain (`0x4C` LTO,
  `0x57` LTO-WORM, `0x43` cleaning, `0x7F` unknown) +
  ASCII Media Type ('7'-'9' data, 'X'-'Z' WORM).

#### Element descriptor flags

Storage / Import-Export / Data Transfer descriptors carry the
following bits (byte 2):

| Bit | Storage | Import/Export | Data Transfer |
|----:|---------|---------------|---------------|
| 0   | Full | Full | Full |
| 1   | — | ImpExp (always 1) | — |
| 2   | Except (always 0) | Except | Except |
| 3   | Access (always 1 — VTL elements always reachable by the robot) | Access (mirrors `accessible` flag on the slot) | Access (always 1) |
| 4   | — | ExEnab (1) | — |
| 5   | — | InEnab (1) | — |

Byte 9 low nibble = Medium Type (`0x00` unspecified, `0x01` data,
`0x02` cleaning, `0x04` WORM); derived from the cartridge barcode
suffix.

### thurvsa block (SBC-3) — `iqn.2025-10.com.metebalci:thurvsa`, port 3260

This is the sibling product: a direct-access block target that draws
on the same cloud chunk pool. Internally, volumes are page-grained
with a default 64 KiB page, but to the host they advertise plain 4 KiB
sectors over SBC-3 — the paging is invisible at the SCSI surface. The
iSCSI target IQN is configurable via `iscsi.target_iqn`, and when the
volume is served over NVMe/TCP instead (`transport: nvmetcp`), the
subsystem NQN is configurable via `nvmetcp.subnqn`. Both default to
the per-product identity and are validated at startup.

| Opcode | Command | Notes |
|-------:|---------|-------|
| 0x00 | TEST UNIT READY | Returns GOOD against any registered LUN. |
| 0x03 | REQUEST SENSE | Returns NoSense (key 0x00) — autosense is delivered on the iSCSI SCSI Response PDU on every CHECK CONDITION, so there's nothing pending to report. CDB byte 1 bit 0 (DESC) selects descriptor format (response code 0x72, 8 bytes) vs fixed format (0x70, 18 bytes). Allocation-length truncation honored. Succeeds against unmapped LUNs (SPC-4 §6.39 — initiators probe the response shape this way). |
| 0x12 | INQUIRY | Standard data + VPD pages 0x00 / 0x80 / 0x83 / 0xB0 / 0xB2. Vendor `MB` (`shared_naming::VENDOR_INQUIRY`), product `THUR VSA` (`shared_naming::DISK_PRODUCT`), revision `0001`; serial / device-id derived from the volume UUID. VPD 0xB0 (Block Limits) advertises MAXIMUM COMPARE AND WRITE LENGTH = sectors-per-page (16 by default), OPTIMAL TRANSFER LENGTH GRANULARITY = sectors-per-page, MAXIMUM UNMAP LBA COUNT = 0xFFFFFFFF, MAXIMUM UNMAP BLOCK DESCRIPTOR COUNT = 4095, OPTIMAL UNMAP GRANULARITY = sectors-per-page, UGAVALID=1 with alignment LBA = 0, MAXIMUM WRITE SAME LENGTH = 0 (no specific limit), WSNZ=0 (zero + non-zero patterns both supported). VPD 0x83 carries one T10 vendor-ID descriptor. VPD 0xB2 (Logical Block Provisioning) sets LBPU=1, LBPRZ=001 (unmapped reads zero), PROVISIONING TYPE=010 (thin). Unmapped LUNs return the SPC-4 "no LUN" pattern (peripheral qualifier 0b011 + type 0x1F). |
| 0x15 | MODE SELECT(6) | Validates the parameter list against the current MODE SENSE values; every Changeable bit is zero so the host can't actually flip WCE / RCD / DRA / D_SENSE. Round-trips clean when the host re-asserts what it just read. PF=0 → INVALID FIELD IN CDB (we don't speak the SCSI-1 vendor format). SP=1 → SAVING PARAMETERS NOT SUPPORTED (sense key 0x05, ASC/ASCQ 0x39/0x00). Block descriptor (if present) must match the volume's `(NUMBER OF LOGICAL BLOCKS, LOGICAL BLOCK LENGTH)`. Unknown / SPF=1 page or any body mismatch → INVALID FIELD IN PARAMETER LIST. |
| 0x1A | MODE SENSE(6) | Caching (0x08) + Control (0x0A) + all-pages (0x3F) alias. WCE=1 / RCD=1 / DRA=1. The in-memory write-back cache is real and is lost on daemon crash, so SBC-3 §6.4.6.4 mandates WCE=1 — without it a compliant initiator (Linux block layer) elides SYNCHRONIZE CACHE on `sync(1)` / `umount` and silently loses host-acked writes. WORM volumes flip WP=1 in the DEVICE-SPECIFIC PARAMETER byte. |
| 0x1B | START STOP UNIT | Accept-and-GOOD regardless of PowerCondition / NO_FLUSH / LOEJ / START bits — thurvsa volumes don't model power states. Linux's `sd_mod` issues this during attach / suspend / shutdown. |
| 0x1E | PREVENT/ALLOW MEDIUM REMOVAL | Accept-and-GOOD regardless of bits 1-0 — thurvsa has no removable media, no enforcement target. |
| 0x25 | READ CAPACITY (10) | Caps the last-LBA field at `0xFFFFFFFF` to force initiators onto READ CAPACITY (16) for big volumes. |
| 0x28 | READ (10) | Sector-grain LBA + transfer length supported end-to-end via the per-volume `PageCache` (core-block::cache). Sub-page reads pull the affected page(s) from the cache or fall through to `VolumeWriter::read_page` (cloud / pool / sparse-hole zero) and slice. Unallocated pages return zeros. Reservation-gated. |
| 0x2A | WRITE (10) | Sector-grain via the cache: load the affected page(s), splice in host bytes at sector grain, mark dirty for asynchronous flush. Full-page writes skip the load. WORM volumes refuse with WRITE PROTECTED. Reservation-gated. |
| 0x2F | VERIFY (10) | Per SBC-3 §5.46. CDB byte 1 bits 2-1 hold BYTCHK: 00b = no compare (read each block to surface medium errors — sparse-hole pages succeed), 01b = compare against Data-Out (Data-Out is `blocks * sector_bytes`; mismatch → CHECK CONDITION + MISCOMPARE). 10b/11b (LB protection) rejected with INVALID FIELD IN CDB. VRPROTECT (bits 7-5) must be 0. Reservation-gated as a read-side opcode. |
| 0x35 | SYNCHRONIZE CACHE (10) | Real fence — awaits the cache's flush of every dirty page whose id falls in the requested LBA range through to cloud-ack via `VolumeWriter::write_page`. NUMBER OF BLOCKS = 0 means "from LBA to end of medium" per SBC-3 §5.21. Out-of-range → LBA OUT OF RANGE. Reservation-gated as a write-side opcode (SBC-3 §5.10). |
| 0x41 | WRITE SAME (10) | SBC-3 §5.49 — replicate one logical block of Data-Out across the requested LBA range. CDB byte 1: WRPROTECT (7-5) must be 0; ANCHOR (4) / PBDATA (2) / LBDATA (1) rejected. UNMAP (3) honored: when set with a zero pattern, route via `cache.unmap_bytes` (cheaper); other patterns expand the single-block payload across the range and route via `cache.write_bytes` in 16 MiB sector-aligned chunks. Data-Out length must equal one logical block. NUMBER OF BLOCKS = 0 is a no-op per §5.49.2. WORM refuses with WRITE PROTECTED. Reservation-gated. |
| 0x42 | UNMAP | Thin-provisioning hint. Parameter list = 8-byte header + N × 16-byte UNMAP BLOCK DESCRIPTOR `{LBA u64, blocks u32, reserved 4}`. Sector-grain descriptors supported via the cache: full-page descriptors drop the cached entry and clear the page-index slot synchronously; sub-page descriptors zero the affected sectors in the cached page and mark dirty so the next flush commits the partial erase. Out-of-range → LBA OUT OF RANGE. ANCHOR=1 rejected (no separate "deallocated" state vs "never allocated"). Two-phase commit: validate every descriptor before any state mutation, so a malformed list leaves the volume untouched. WORM volumes refuse with WRITE PROTECTED. Reservation-gated as a write-side opcode. |
| 0x4D | LOG SENSE | Page 0x00 (Supported Log Pages) only, listing just 0x00 itself — the SBC-3 / SPC-4 SAS-vintage log pages (temperature, retry counters, etc.) don't apply to a virtual block target. Other page codes / non-zero subpages → INVALID FIELD IN CDB per SPC-4 §7.2.5. |
| 0x55 | MODE SELECT(10) | Same semantics as 0x15. Honors the LONGLBA bit on header byte 4 to pick the long-form vs short-form block descriptor. |
| 0x5A | MODE SENSE(10) | Same coverage as 0x1A. |
| 0x5E | PERSISTENT RESERVE IN | Service actions 0x00 READ KEYS, 0x01 READ RESERVATION, 0x02 REPORT CAPABILITIES, 0x03 READ FULL STATUS. PR_GENERATION counter; in-memory state. REPORT CAPABILITIES advertises TYPE_MASK = `0xEA, 0x01` (WR_EX, EX_AC, WR_EX_RO, EX_AC_RO, WR_EX_AR, EX_AC_AR), TMV=1, PTPL_C=0. READ FULL STATUS renders an iSCSI format-0 TransportID per registrant (initiator IQN, NUL-padded). See *Persistent reservations (thurvsa)* below for state model. |
| 0x5F | PERSISTENT RESERVE OUT | Service actions 0x00 REGISTER, 0x01 RESERVE, 0x02 RELEASE, 0x03 CLEAR, 0x04 PREEMPT, 0x05 PREEMPT AND ABORT (collapses to PREEMPT — no taskman hook), 0x06 REGISTER AND IGNORE EXISTING KEY. SA 0x07 REGISTER AND MOVE rejected. APTPL=1 / SPEC_I_PT=1 / ALL_TG_PT=1 in the parameter list reject as INVALID FIELD IN PARAMETER LIST. SCOPE must be 0x00 (LU_SCOPE) for SAs other than REGISTER variants. |
| 0x88 | READ (16) | 64-bit LBA / 32-bit transfer length. Same sector-grain + reservation rules as READ (10). |
| 0x89 | COMPARE AND WRITE | SBC-3 §5.2 atomic test-and-set. CDB byte 13 holds NUMBER OF LOGICAL BLOCKS (8-bit). Data-Out PDU is `2 * blocks * sector_bytes`: first the compare buffer, then the write buffer. On byte-for-byte match the write half is committed; on diff the device returns CHECK CONDITION + MISCOMPARE (sense key 0x0E, ASC/ASCQ 0x1D/0x00) and the write half is suppressed. Sub-page CAW (1-sector VMFS heartbeat) supported end-to-end via the cache. Read+compare+write triple is atomic against other CAWs on the same LUN via a per-LUN async mutex (`CawLocks`); concurrent regular WRITE on the same range is host-side undefined behavior. WORM volumes refuse with WRITE PROTECTED. Reservation-gated as a write-side opcode. |
| 0x8A | WRITE (16) | 64-bit LBA / 32-bit transfer length. Same sector-grain + WORM + reservation rules as WRITE (10). |
| 0x8F | VERIFY (16) | Same semantics as VERIFY (10), 64-bit LBA + 32-bit transfer length. |
| 0x91 | SYNCHRONIZE CACHE (16) | Same semantics as 0x35. |
| 0x93 | WRITE SAME (16) | Same semantics as WRITE SAME (10) plus the NDOB bit (CDB byte 1 bit 0): NDOB=1 means "no Data-Out, zero-fill" — the host signals the implicit zero pattern without sending it on the wire. NUMBER OF BLOCKS = 0 means "from LBA to end of medium" per SBC-3 §5.50. Reservation-gated. |
| 0x9E sa 0x10 | READ CAPACITY (16) | Full 8-byte last LBA. Byte 14 sets LBPME=1 (thin-provisioning management enabled — see VPD 0xB2 for details) and LBPRZ=1 (unmapped LBAs read as zeros). Lowest aligned LBA = 0. |
| 0xA0 | REPORT LUNS | SAM-5 single-level + flat-space encoding. |
| 0xA3 sa 0x0C | MAINTENANCE IN — REPORT SUPPORTED OPERATION CODES | Lists every routed CDB in ascending order. Per-entry shape: 1-byte OPERATION CODE, 1 reserved, 2-byte SERVICE ACTION (= 0 for opcodes that don't carry one in this dispatcher), 1 reserved, 1 byte CTDP/SERVACTV (= 0), 2-byte CDB LENGTH (= 0xFFFF "ask via SA-specific report"). VAAI / Hyper-V probe this to discover offload primitives. REPORTING_OPTIONS=1/2/3 (single-opcode forms) not implemented. |
| 0xA3 sa 0x0D | MAINTENANCE IN — REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS | 4-byte response: byte 0 ATS / ATSS / CTSS / LURS = 0xD8, byte 1 ITNRS = 0x80. Mirrors the iSCSI session-layer's TMF handler (every TMF unconditionally returns Function Complete on a single-initiator-per-LUN virtual target). |

Unknown opcodes → CHECK CONDITION + ILLEGAL REQUEST + INVALID
COMMAND OPERATION CODE.

#### Persistent reservations (thurvsa)

The state model is per-LUN. Each LUN carries a set of registrations,
each keyed by an I_T nexus `(tsih, initiator_iqn)`; at most one active
reservation, described by `{holder, key, type}`; and a `PR_GENERATION`
counter (SPC-4 §6.13.1.1) that wraps on overflow.

A registration key is a 64-bit value the initiator picks. The same key
value may legitimately appear on more than one nexus — that is how
cooperating MPIO endpoints share a key.

The reservation TYPE byte takes the following SBC-3 values:

  | Type | Name | Read gate | Write gate |
  |-----:|------|-----------|------------|
  | 0x01 | Write Exclusive | (none) | holder only |
  | 0x03 | Exclusive Access | holder only | holder only |
  | 0x05 | Write Exclusive — Registrants Only | (none) | any registrant |
  | 0x06 | Exclusive Access — Registrants Only | any registrant | any registrant |
  | 0x07 | Write Exclusive — All Registrants | (none) | any registrant |
  | 0x08 | Exclusive Access — All Registrants | any registrant | any registrant |

  A "Read gate (none)" entry means reads are always allowed, no matter
  which nexus issues them. When an access *is* denied, the device
  returns SCSI status 0x18 (RESERVATION CONFLICT) and no sense data —
  per SPC-4 §6.16 the status code alone carries the signal.

Two reservation TYPEs in the table — the All-Registrants (`AR`)
variants — behave differently when the holder goes away. For an AR
type, dropping the holder's I_T nexus (a logout, a TCP drop, or
`on_session_close`) does not release the reservation; the recorded
holder simply rotates to one of the surviving registrants. Every
non-AR type releases its reservation when the holder disappears.

The PROUT parameter list has a fixed 24-byte baseline:

```text
byte  0..7    RESERVATION KEY (u64 BE)
byte  8..15   SERVICE ACTION RESERVATION KEY (u64 BE)
byte 16..19   scope-specific address (obsolete; must be 0)
byte 20       SPEC_I_PT (bit 3) | ALL_TG_PT (bit 2) | APTPL (bit 0)
              — every bit must be 0; non-zero rejects with
                INVALID FIELD IN PARAMETER LIST
byte 21       reserved
byte 22..23   obsolete (extent length)
```

On the PRIN side, the response headers for READ KEYS, READ
RESERVATION, and READ FULL STATUS are 8 bytes: `PR_GENERATION (u32
BE)` followed by `ADDITIONAL LENGTH (u32 BE)`. REPORT CAPABILITIES is
a fixed 8-byte payload with byte 0 = 0x00, byte 1 = 0x08, byte 3 =
0x80 (TMV=1), and bytes 4..5 = TYPE_MASK.

Reservation state is held in memory only, so a daemon restart drops
every registration. That is not a hidden behavior — REPORT
CAPABILITIES advertises `PTPL_C = 0` precisely so that initiators know
to re-register on reconnect.

### Format / partitioning

| Opcode | Command | Notes |
|-------:|---------|-------|
| 0x04 | FORMAT MEDIUM | FORMAT field 0x01 applies the partition layout staged by MODE SELECT 0x11 |

### VPD Pages (INQUIRY 0x12)

| Page | Name | Notes |
|-----:|------|-------|
| 0x00 | Supported VPD Pages | Page list differs per LUN: changer advertises 00, 80, 83, 85, C0; tape advertises 00, 80, 83, B0, B1, B2, B3, B4. |
| 0x80 | Unit Serial Number | Changer: `<chassis_serial>_LL<NN>` where `NN` is the 1-based partition index (sessions bound to different partitions see distinct serials). Drive LUNs: the per-drive `mfg_serial` from `inventory.json` (10-byte string, persisted across daemon restarts). |
| 0x83 | Device Identification | Three descriptors: (1) NAA-3 (Locally Assigned, 8-byte binary) — top nibble `0x3`, remaining 60 bits = BLAKE3(`chassis_serial \| lun \| partition_name`) so identity is globally distinct and stable across daemon restarts; (2) T10 vendor-ID-based identifier (ASCII); (3) Logical Unit Group (codeset 1, association 10, designator type 6) — 4-byte group ID = first 4 bytes of BLAKE3(`chassis_serial \| partition_name`). Drives in the same partition share the same group ID; backup software uses it to auto-correlate "these LUNs belong to one logical library." Multipath stacks bind to NAA. |
| 0x85 | Management Network Address (changer only) | ASCII URL pointing at the daemon's HTTP listener. |
| 0xB0 | Sequential-Access Device Characteristics (SSC-5 §8.5.4) | Tape-drive LUNs only. Byte 4 bit 7 = WORMM, set when the loaded cartridge is WORM (`manifest.worm == true`). |
| 0xB1 | Manufacturer-Assigned Serial Number | Tape-drive LUNs only. 32-byte ASCII serial padded with spaces, conceptually fixed at production. Reads from `DriveInfo::mfg_serial` in `inventory.json` (10-byte `TVL` + 7 hex chars, minted once per drive at library init / `library modify --drives N`). Pre-field libraries fall back to the deterministic literal `THUR-MFG-NNN`. The same string is reported by LOG SENSE page 0x14 parameter 0x0040 so hosts correlating the two sources see one identity per drive LUN. |
| 0xB2 | TapeAlert Supported Flags (SSC-3 TapeAlert supplement) | Tape-drive LUNs only. 8-byte bitmap covering TapeAlert flags 1..=64 — bit 7 of byte 4 = flag 1, bit 0 of byte 11 = flag 64. Thur VTL advertises all 64 flags (`0xFF` ×8), matching what LOG SENSE 0x2E already exposes. A healthy virtual drive reports flag value 0 for each parameter; the bitmap tells hosts every standard SSC-3 alert flag is interpretable if anything were ever raised. |
| 0xB3 | Automation Device Serial Number (SSC-5 §8.5.6 companion to 0xB4) | Tape-drive LUNs only. 32-byte ASCII serial padded with spaces, identifying the *automation device* (chassis / library) this drive is housed in. Reads from `LibraryTopology::chassis_serial` (14-byte string, persisted in `library.json`). Distinct from VPD 0xB1 (per-drive serial) and from the changer LUN's VPD 0x80 (which carries the `_LL<NN>` partition suffix on top of the same chassis serial). Pre-field libraries fall back to the legacy literal `THUR-CHG-001`. Backup software pairs 0xB3 + 0xB4 to identify "which library does this drive belong to and at which element". |
| 0xB4 | Data Transfer Device Element Address (SPC-4 §7.8.7 / SSC-5 §8.5.7) | Tape-drive LUNs only. One designation descriptor (codeset=binary, association=target device, designator type=vendor specific) carrying a 4-byte big-endian element address — high two bytes zero, low two bytes = `data_transfer_start + (lun - 1)`. Lets the host correlate a drive's INQUIRY identity with its slot in the changer's element table. |
| 0xC0 | Firmware Build Information (changer only) | ASCII build descriptor `thurvtl <firmware>`, padded to 64 bytes. Vendor-specific page-code range (0xC0-0xFF) — backup software ignores it unless explicitly probing. |

### Mode Pages — tape drive (LUN ≥ 1)

| Page | Name | Notes |
|-----:|------|-------|
| 0x01 | Read-Write Error Recovery | |
| 0x02 | Disconnect-Reconnect (SPC-3 §7.4.5) | 16-byte page (2-byte header + 14-byte body, page-length=0x0E). Originally a parallel-SCSI knob; per SPC-3 Annex G the legacy fields are ignored on transports that don't support disconnect/reconnect — which includes iSCSI, the only transport Thur VTL ships. All body bytes zero. PC=Changeable reports zero — no fields host-tunable. Backup software polls during drive-capability sweeps. |
| 0x0A / 0xF0 | Control Data Protection (SPC-4 §7.5.7) | SPF=1 subpage. 16-byte page (4-byte header + 12-byte body). Drive advertises `LBP_INFO_LENGTH = 4`, `LBP_METHOD = 0x01` (CRC32C, Castagnoli polynomial 0x1EDC6F41). Host enables LTO-7+ Logical Block Protection by writing non-zero `LBP_W` (body byte 0 bits 7..5) and/or `LBP_R` (bits 4..2). When `LBP_W` is set, WRITE(6/16) with WRPROTECT > 0 validates the trailing 4-byte CRC32C and returns ABORTED COMMAND + 0x10/0x05 on mismatch; when `LBP_R` is set, READ(6/16) with RDPROTECT > 0 appends a freshly-computed trailer to the response. CRC is recomputed on every read from BLAKE3-verified plaintext — there is no separate stored guard. |
| 0x0F | Data Compression | DCC=1 always; DCE follows runtime drive state. MODE SELECT toggles DCE. Initial DCE per cartridge load comes from `drive.compression.default`; algorithm choice from `drive.compression.algorithm` (`lz4` default, or `zstd`; `sldc` is reserved but the codec isn't shipped — activating DCE with `algorithm=sldc` silently rewrites to lz4 and logs a warning, so writes never trap). Zstd level via `drive.compression.zstd_level`. Algorithm code in the page is reported as 0 (vendor-specific) — neither LZ4 nor zstd nor SLDC has a SCSI-registered code. |
| 0x10 | Device Configuration | |
| 0x10 / 0x01 | Device Configuration Extension (SSC-5 §8.3.4.5) | SPF=1 subpage. 32-byte page (4-byte header + 28-byte body). Round-tripped through MODE SELECT; SP=1 persists across cartridge swaps via `<data_dir>/library/drive_state.json`. Two host-set bits are enforced at WRITE / WRITE FILEMARKS time: body byte 0 high nibble = WRITE MODE (LTO-7+ Append-Only when set to 1) and body byte 2 bit 0 = WRE (LTO-8+ Encrypt-Only). Programmable Early Warning size (PEWS) bytes round-trip but aren't enforced — Thur VTL uses a fixed 95% EW trigger from `effective_capacity_bytes()` instead. |
| 0x11 | Medium Partition | MODE SELECT stages a `PendingPartitionLayout`; FORMAT MEDIUM (FORMAT=0x01) applies it. |
| 0x1A | Power Condition (SPC-4 §7.5.13) | 12-byte page (2-byte header + 10-byte body, page-length=0x0A). All body bytes zero — virtual drive never auto-idles or auto-standbys; Idle/Standby condition timers = 0. PC=Changeable reports zero — no fields host-tunable. |
| 0x1C | Informational Exceptions Control (SPC-5 §7.5.10) | 12-byte page (4-byte header + 8-byte body, page-length=0x0A). DExcpt=0 (exception generation enabled, but the virtual drive never raises any), MRIE=6 ("Only Report on Request" — least intrusive default). Interval Timer / Report Count = 0. Hosts that want TapeAlert poll LOG SENSE 0x2E directly. |

#### MODE SELECT round-trip + SP=1 persistence (tape, LUN ≥ 1)

SPC-4 requires that any byte MODE SENSE advertises as tunable — meaning
the per-page Changeable mask has the corresponding bit set — must
survive a round trip through MODE SELECT. The host writes a value and
must see exactly those bytes back on the next MODE SENSE under
PC=Current, and also under PC=Saved if it requested SP=1. The
implementation delivers this by storing the raw page bodies, and works
in the following steps:

- **Parsing.** `parse_mode_pages` walks the parameter list and
  detects the SPF bit on each page header. An SPF=0 page uses a 2-byte
  header (`page_code, page_length`); an SPF=1 subpage uses a 4-byte
  header (`page_code|0x40, subpage_code, page_length BE16`).
- **Storage.** Every page is captured as a `(page_code, subpage_code,
  body)` triple and merged into the cartridge's volatile
  `mode_pages_state`, a `Vec<SavedModePage>`. Fields that actually
  drive behavior — the page 0x0F DCE bit, the page 0x11 partition
  layout — are *additionally* applied through their dedicated setters.
  Doing both keeps the runtime truth and the saved raw body coherent
  with each other.
- **SP=1 persistence — per-drive, library-wide sidecar.** When the
  host sets the Save Pages bit (MODE SELECT(6/10) CDB byte 1 bit 0),
  the raw page bodies are also mirrored to
  `<data_dir>/library/drive_state.json` with a tmp-then-rename atomic
  write. The file is keyed by drive id rather than cartridge barcode,
  which mirrors real LTO hardware: there, saved mode pages live in
  drive NVRAM and survive cartridge swaps. `DriveManager` owns the
  file — it loads it at startup and rewrites it on every SP=1. The
  state is deliberately kept *out* of any `manifest.json`. The
  manifest rides the cloud-backup pipeline and may end up on a
  retention-locked backend, whereas drive-side config has to stay
  freely re-writable; for that reason it gets the same local-only
  treatment as `lru.idx`. A missing or corrupt file simply yields
  empty state, and the page builders then emit defaults.
- **MODE SENSE replay.** When building a page under PC=Current or
  PC=Saved, the page builders consult `mode_pages_state` first and
  fall back to defaults only if nothing is saved. Page 0x0F is the one
  exception: the runtime DCE bit overrides whatever the saved body
  says, because the saved DCE position is only advisory — the drive's
  live compression state is the truth.
- **PS bit.** Every response page header carries PS=1, signaling that
  every page is saveable.
- **Changeable masks.** Under PC=Changeable the tape pages (0x01,
  0x10/0x00, 0x10/0x01, 0x1A, 0x1C) advertise `0xFF` for each
  round-trippable byte. Page 0x0F's mask covers DCE, DDE/RED, and the
  algorithm codes, while DCC stays fixed. Page 0x02 reports an
  all-zero changeable mask: its parallel-SCSI fields are ignored on
  iSCSI per SPC-3 Annex G, so there is nothing for the host to tune.

### Mode Pages — medium changer (LUN 0)

These pages follow SPC-4 / SMC-3. Requesting `page_code = 0x3F`
returns every SPF=0 page at once, and adding `subpage_code = 0xFF`
folds in the SPF=1 subpages as well.

| Page | Subpage | Name | Notes |
|-----:|--------:|------|-------|
| 0x0A | 0x01 | Control Extension | SCSIP=1 (SET TIMESTAMP precedence), TCMOS=0, IALUAE=0. |
| 0x1C | 0x00 | Tape Alert | MRIE=0 (host polls LOG SENSE 0x2E). |
| 0x1D | 0x00 | Element Address Assignment | All counts/start-addresses driven by live library topology. |
| 0x1E | 0x00 | Transport Geometry | Single transport element, no media rotation. |
| 0x1F | 0x00 | Device Capabilities | Advertises which MOVE MEDIUM / EXCHANGE MEDIUM combinations the changer supports. MT cannot store cartridges; ST/I/E/DT can move to {DT,I/E,ST}. |

### Log Pages (LOG SENSE 0x4D)

LOG SENSE is LUN-gated — the changer (LUN 0) and a tape drive (LUN ≥ 1)
expose different sets of pages. In practice, real backup software
polls just 00, 0D, and 2E on the changer.

#### Tape drive (LUN ≥ 1)

| Page | Name |
|-----:|------|
| 0x00 | Supported Log Pages |
| 0x02 | Write Errors |
| 0x03 | Read Errors |
| 0x06 | Non-Medium Errors |
| 0x0C | Sequential Access Device (SSC-5 §8.5; bytes-transferred counters + partition-capacity hints, all zero on a virtual drive) |
| 0x0D | Temperature |
| 0x11 | DT Device Status (legacy enum name `TapeUsage`; SSC-4 §8.2.3 calls it DT Device Status) |
| 0x12 | Tape Alert Response (SSC-3 §8.2.4; host-poll companion to 0x2E. Empty parameter list — virtual drive has no alert history) |
| 0x14 | Device Statistics (SSC-5 §8.5; lifetime drive counters, all zero on a virtual drive). Parameter 0x0040 (Drive Manufacturer's Serial Number) mirrors INQUIRY VPD 0xB1. |
| 0x16 | Last n Error Events (SSC-5 §8.6; empty parameter list — virtual drive doesn't fault) |
| 0x17 | Volume Statistics (SSC-5 §8.7; per-mounted-volume counters, Validity=0 / counters=0) |
| 0x1A | Power Condition Transitions (SPC-4 §7.3.16; six 4-byte counters for transitions into Active / Idle_a / Idle_b / Idle_c / Standby_y / Standby_z, all zero on a virtual drive) |
| 0x1B | Data Compression (SSC-5; replaces deprecated 0x32). Read/Write compression ratios reported as 0x0100 (1:1) — the SCSI surface doesn't see drive compression as a payload-size change. Cumulative byte counters all zero. |
| 0x2E | TapeAlert |
| 0x30 | Tape Usage (legacy; deprecated by SSC-5 in favor of 0x14 + 0x0C, but legacy backup software still polls). All counters zero on a virtual drive. |
| 0x31 | Tape Capacity (legacy; per-partition remaining/maximum MB, all zero — same shape 0x0C reports) |
| 0x32 | Data Compression (legacy; deprecated by SSC-5 in favor of 0x1B). Same per-counter shape as 0x1B with the older parameter codes pre-LTO-7 backup software keys on. 1:1 ratio (0x0100), all byte counters zero. |

#### Medium changer (LUN 0)

| Page | Name |
|-----:|------|
| 0x00 | Supported Log Pages |
| 0x0D | Temperature |
| 0x2E | TapeAlert |

### Diagnostic Pages (RECEIVE DIAGNOSTIC RESULTS 0x1C)

`SEND DIAGNOSTIC` (0x1D) does real work only when the host sets
`SELFTEST=1` in CDB byte 1. The probe itself runs in the iSCSI request
loop, *before* SCSI dispatch — this is what lets the cloud-backend
health check be an async operation rather than blocking the synchronous
handler. The handler then translates the freshest stored result into a
terminal SCSI status:

- A **Pass** result becomes SCSI status GOOD.
- A **Fail** result becomes CHECK CONDITION + sense key 0x04 (HARDWARE
  ERROR) + ASC/ASCQ 0x40/0x80 (DIAGNOSTIC FAILURE ON COMPONENT 80h, in
  the vendor range).
- **Other CDB byte 1 values** — the default no-op probe, parameter-list
  tests, and the foreground/background extended self-test codes
  0b001..0b110 — return GOOD without recording anything.

What the probe actually checks depends on the LUN:

- **LU0 (changer)**: parse `<data_dir>/library/library.json` +
  `<data_dir>/library/inventory.json`, confirm every barcode in
  inventory has a readable `<data_dir>/tapes/<barcode>/manifest.json`,
  then run `validate_cloud_backend` against every named
  `cloud.backends:` entry (auth + write + delete probe — the same
  routine `thurvtl system cloud check` runs).
- **LU1+ (drive)**: if a cartridge is loaded, re-read its
  `manifest.json` and confirm it parses; if no cartridge is loaded,
  GOOD trivially.

The daemon keeps a per-LUN ring buffer of the 20 most recent results
in its `DiagnosticStore`; the store is volatile. RECEIVE DIAGNOSTIC
RESULTS reads back from this buffer.

| Page | Name |
|-----:|------|
| 0x00 | Supported Diagnostic Pages — lists `[0x00, 0x10]`. |
| 0x10 | Self-Test Results — SPC-4 §7.2.21 layout. Page length 0x0190 (= 20 entries × 20 bytes). |

A request with `PCV=0` (cdb[1] bit 0) returns page 0x00. Any
unsupported page code is rejected with CHECK CONDITION + ILLEGAL
REQUEST + INVALID FIELD IN CDB (0x05 / 0x24 / 0x00).

#### Self-Test Results page (0x10) layout

```
Byte | Field                                | Notes
-----+--------------------------------------+--------------------------
   0 | PAGE CODE = 0x10                     |
   1 | reserved                             |
 2-3 | PAGE LENGTH (BE) = 0x0190 (= 400)    | always 20 entries × 20 bytes
 4-23 | Parameter 0001h (most recent)       | 20-byte entry, see below
24-43 | Parameter 0002h                     |
... | ...                                    |
384-403 | Parameter 0014h (oldest)          |
```

Each 20-byte entry:

```
Byte | Field
-----+------------------------------------------------------------------
 0-1 | PARAMETER CODE (BE) = 0x0001..0x0014 (slot id, 1-based)
   2 | PARAMETER CONTROL BYTE = 0x03 (FORMAT=11b bounded, LP=1 binary list)
   3 | PARAMETER LENGTH = 0x10 (16)
   4 | bits 7..4 = SELF-TEST CODE (host-issued, 0 for default probe)
     | bits 3..0 = SELF-TEST RESULTS VALUE (0 = pass, 7 = test failed)
   5 | SELF-TEST NUMBER (vendor, 0)
 6-7 | ACCUMULATED POWER ON HOURS (BE, 0 — virtual drive)
 8-15 | ADDRESS OF FIRST FAILURE (8-byte BE LBA, 0 — N/A)
  16 | bits 3..0 = SENSE KEY (0 on pass; 0x04 on diag failure)
  17 | ASC (0x40 on diag failure)
  18 | ASCQ (0x80 on diag failure)
  19 | VENDOR SPECIFIC (0)
```

When a LUN has fewer than 20 recorded results, the unused slots still
carry their parameter header (code, control, length) but zero-fill the
16-byte data block.

### Encryption (SECURITY PROTOCOL OUT/IN, protocol 0x20)

Encryption is exposed as LTO Application-Managed Encryption (AME),
with AES-256-GCM applied per block. The page codes follow SPC-4
§7.6.4 / SSC-5 §4.2.20. SP-IN and SP-OUT share a single page-code
namespace and are disambiguated only by direction — for example, SPSP
`0x0010` means "Data Encryption Capabilities" on SP IN but "Set Data
Encryption" on SP OUT.

| SPSP | Direction | Name |
|-----:|-----------|------|
| 0x0000 | IN  | Tape Data Encryption In Support (lists every implemented SP-IN page) |
| 0x0001 | IN  | Tape Data Encryption Out Support (lists implemented SP-OUT pages) |
| 0x0010 | IN  | Data Encryption Capabilities (advertises AES-256-GCM) |
| 0x0010 | OUT | Set Data Encryption (host supplies key) |
| 0x0011 | IN  | Supported Key Formats (plaintext only) |
| 0x0020 | IN  | Data Encryption Status |
| 0x0021 | IN  | Next Block Encryption Status |

Set Data Encryption accepts only two `ENCRYPTION_MODE` values: `0x00`
(DISABLE) and `0x02` (ENCRYPT). The third standard value, `0x01`
(EXTERNAL, used by inline encryption appliances), is refused with
CHECK CONDITION — and the SP-IN Data Encryption Capabilities page
advertises CAP_C=00 (no EXTERNAL) so the host knows this in advance.

The drive key is **never persisted**. It lives only in volatile drive
state and is wiped on UNLOAD, or earlier if the host issues an
explicit DISABLE via Set Data Encryption. The per-block AES-GCM IV is
not stored either — it is **derived** at both encrypt and decrypt time
from `BLAKE3(cartridge_uuid ‖ chunk_id_le ‖ offset_le)[..12]`. This
mirrors how real LTO drives work: they derive IVs from position and
never write them to the medium. The 16-byte authentication tag is
concatenated onto the ciphertext in the chunk file, the standard
`ciphertext ‖ tag` shape, and the block-index record's `len` field is
sized to include those 16 bytes. Reading an encrypted block without
the correct key returns CHECK CONDITION + DATA PROTECT + ASC/ASCQ
0x74/0x0C (LOGICAL UNIT ENCRYPTION KEY MISMATCH).

### WORM cartridges

WORM (Write Once Read Many) is a per-cartridge flag. It is set once,
at create time via `cartridge create --worm`, persisted as
`manifest.worm`, and sticky for the rest of the cartridge's lifetime —
it cannot be turned off. The host learns a cartridge is WORM from
INQUIRY VPD page 0xB0 (the WORMM bit, byte 4 bit 7).

Enforcement happens in the cartridge layer, which returns
`Thur VTLError::WormViolation`. The iSCSI layer maps that to CHECK
CONDITION + DATA PROTECT key 0x07 + ASC/ASCQ 0x30/0x0C ("WRITE
PROTECTED — WORM MEDIUM"). The per-operation behavior is:

| Operation | WORM behavior |
|-----------|---------------|
| WRITE(6/16) at LBA != EOD | refused (0x30/0x0C) |
| WRITE FILEMARKS(6/16) at LBA != EOD | refused (0x30/0x0C) |
| ERASE (0x19) | refused outright (0x30/0x0C) |
| FORMAT MEDIUM (0x04, any FORMAT field) | refused outright (0x30/0x0C) |
| ALLOW OVERWRITE (0x82) | refused outright (0x30/0x0C) |
| WRITE(6/16) at EOD | allowed (append-only) |
| READ family | allowed |

### Legal hold (host-visible write-protect)

Legal hold is anchored in the cloud, not on the host. The hold state
of each object is recorded with the provider's native primitive — S3
`PutObjectLegalHold`, GCS `eventBasedHold`, Azure `Set Blob Legal
Hold`. The `manifests/<barcode>/manifest-latest.json` key acts as the
**sentinel** that answers "is this cartridge held?". To keep that
sentinel meaningful under a partial failure, the hold is applied
body-first then sentinel-last when setting it, and sentinel-first then
body-after when clearing it.

The daemon does not poll the sentinel. It reads it exactly **once, at
drive-load time** — in the iSCSI MOVE MEDIUM (0xA5) post-hook — and
stamps a volatile `legal_held` flag on the loaded cartridge. That flag
is pinned for the lifetime of the load and is cleared on UNLOAD via
`Drop`; the next load re-reads the sentinel afresh. While the flag is
set, the cartridge layer returns `Thur VTLError::LegalHoldViolation`,
which the iSCSI layer maps to CHECK CONDITION + DATA PROTECT key 0x07
+ ASC/ASCQ 0x27/0x00 ("WRITE PROTECTED"). Note this is the plain
write-protected code, **not** the WORM-specific 0x30/0x0C — the
distinction is deliberate, because a legal hold is operator-applied
preservation rather than the sticky-at-create write-once semantics of
WORM. The per-operation behavior is:

| Operation | Legal-hold behavior |
|-----------|---------------------|
| WRITE(6/16) | refused (0x27/0x00) |
| WRITE FILEMARKS(6/16) | refused (0x27/0x00) |
| ERASE (0x19) | refused (0x27/0x00) |
| FORMAT MEDIUM (0x04) | refused (0x27/0x00) |
| ALLOW OVERWRITE (0x82) | refused (0x27/0x00) |
| READ family | allowed |
| MOVE MEDIUM (load/unload) | allowed |

Both `legal-hold set` and `legal-hold clear` refuse to act on a
cartridge that is currently in a drive — they look this up in
`<data_dir>/library/inventory.json`. The operator has to `unload`
first, and the audit log records the refusal either way. A WORM
cartridge can still be placed under legal hold (this is how cloud
preservation is extended past the Object Lock retention window).
When a cartridge is both WORM and held, the WORM SCSI gate runs ahead
of the legal-hold gate, so a refused write surfaces the WORM ASC/ASCQ
(0x30/0x0C) to the host.

There is one race the load-time sentinel read does not cover: the
bucket being flipped to held while the cartridge is already loaded.
The daemon's auto-hold-on-upload worker is the safety net for that.
On each upload request it re-reads the sentinel, and if the cartridge
is held, it applies the per-object hold to every object it just PUT —
every freshly-PUT chunk, every freshly-PUT index page object
(`manifests/<barcode>/<label>/page-<NNNNNN>.dat`), and the new
manifest backup objects (the versioned key plus the sentinel,
sentinel-last). Under normal operation this path rarely fires, because
the hold-while-loaded refusal already means no fresh writes happen
during a hold.

A second, independent layer of immutability sits on top of legal hold:
cloud-side retention, driven by the bound backend's `retention_mode`.
Thur VTL never sets per-object retention itself; instead the
bucket-level Object Lock or retention policy auto-applies retention to
every PUT. At startup the daemon validates the configured
`retention_mode` against the bucket's actual `lock_state`. The check
is bidirectional and a mismatch is fatal:

| Configured `retention_mode` | Bucket `lock_state` | Result |
|----------------------------|---------------------|--------|
| `none`                     | `Off`               | OK |
| `none`                     | `Governance` / `Compliance` | fail-to-start |
| `governance`               | `Governance`        | OK |
| `governance`               | `Off`               | fail-to-start |
| `governance`               | `Compliance`        | fail-to-start |
| `compliance`               | `Compliance`        | OK |
| `compliance`               | `Off` / `Governance` | fail-to-start |

This query runs for **every** backend, no matter what `retention_mode`
is declared, so a misconfiguration in either direction gets caught.
There is one graceful-degradation case: if the management-plane query
*itself* fails — because the principal lacks
`s3:GetBucketObjectLockConfiguration`, `storage.buckets.get`, or the
'Storage Account Reader' role — the daemon emits a WARN log and
proceeds without verification. This is intentional: a non-WORM
operator who never grants management-plane IAM keeps working, while a
WORM operator who grants the IAM gets the hard verification they want.
Any mismatch that *does* get verified is fatal.

Each provider's `lock_state` is derived as follows:
- **S3**: `GetObjectLockConfiguration` → `ObjectLockEnabled` + default
  `Rule.DefaultRetention.Mode` (`GOVERNANCE` | `COMPLIANCE`).
- **GCS**: bucket `retentionPolicy.retentionPeriod` + `isLocked` flag.
  `isLocked == true` → `Compliance`.
- **Azure**: ARM management-plane GET on
  `/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Storage/storageAccounts/{account}/blobServices/default/containers/{container}/immutabilityPolicies/default`.
  `properties.state == "Unlocked"` → `Governance`,
  `properties.state == "Locked"` → `Compliance`. 404 → `Off`.
  Requires `subscription_id` and `resource_group` in the backend
  config (both fields validated at parse time when `retention_mode != none`)
  and AAD auth — SAS auth cannot query the management plane (see
  [`AUTH.md`](AUTH.md) § Azure). Required IAM:
  `Microsoft.Storage/storageAccounts/blobServices/containers/immutabilityPolicies/read`
  — assign the built-in **Storage Account Contributor** role at the
  storage-account scope.
- **Local**: always `Off`. WORM cartridges cannot target a local backend.

For a locked bucket (governance or compliance), `validate_cloud_backend`
skips its usual write/delete probe — writing a test object you can
never delete would just litter the bucket — and falls back to a
list-only reachability check, which is enough.

The IAM requirements split across two planes. Data-plane permissions —
for chunks and manifests — are needed on every cloud. Management-plane
permissions are needed only when the bidirectional retention check
runs, and only for the `lock_state` query. An operator who cannot
grant the management-plane permissions can opt out entirely with the
top-level `cloud.skip_retention_mode_check: true` flag (default
`false`). With it set, `lock_state` is never queried, Azure backends
with `retention_mode != none` no longer require `subscription_id` or
`resource_group`, and only data-plane permissions are needed. The cost
of opting out is the loss of the boot-time check that catches the
"operator declared WORM but the bucket isn't actually locked" case;
`retention_mode` is still consulted for the `cartridge create --worm`
CLI gate.

| Provider | Data plane (chunks / manifests) | Management plane (lock_state, WORM only) |
|---|---|---|
| **AWS S3** | `s3:ListBucket`, `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` | `s3:GetBucketObjectLockConfiguration` |
| **GCS** | `roles/storage.objectAdmin` | `storage.buckets.get` (granted by `roles/storage.legacyBucketReader` — minimal — or `roles/storage.admin` which is a superset of both this and `objectAdmin` and works as a single-role grant) |
| **Azure** | **Storage Blob Data Contributor** on the storage account (or container) | **Storage Account Contributor** on the storage account |

The data-plane and management-plane roles are independent of each
other. On Azure in particular, **Storage Blob Data Contributor** (data
plane) and **Storage Account Contributor** (management plane) are two
*different* roles: a WORM operator needs both, while a non-WORM
deployment needs only the data-plane grant.

#### Example IAM policies

**S3** — a single inline policy covers both planes. Replace the bucket
names with your own, and supply one resource pair per bucket:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
        "s3:DeleteObject",
        "s3:ListBucket",
        "s3:GetBucketObjectLockConfiguration"
      ],
      "Resource": [
        "arn:aws:s3:::your-bucket-name",
        "arn:aws:s3:::your-bucket-name/*"
      ]
    }
  ]
}
```

Both ARNs per bucket are required because S3 scopes the two kinds of
operation differently: bucket-level ops (`ListBucket`,
`GetBucketObjectLockConfiguration`) target `arn:aws:s3:::your-bucket`,
while object-level ops (`PutObject`/`GetObject`/`DeleteObject`) target
`arn:aws:s3:::your-bucket/*`.

**GCS** — assign one of:
- `roles/storage.admin` (single role, covers both planes; broader
  than necessary), OR
- `roles/storage.objectAdmin` + `roles/storage.legacyBucketReader`
  (least-privilege split).

**Azure** — assign both at the storage-account scope:
- **Storage Blob Data Contributor** (data plane)
- **Storage Account Contributor** (management plane, WORM only)

---

## On-Disk Layout

```
<data_dir>/
├── .daemon.lock                # PID lockfile while daemon is running
├── library/
│   ├── library.json            # Topology (slots, drives, LTO gen) — immutable at runtime
│   └── inventory.json          # Slot/drive contents — fs2-locked, atomic writes
├── tapes/
│   ├── BARCODE1/
│   │   ├── manifest.json       # Creation-frozen identity: chunking mode, cartridge UUID, capacity, backend, WORM, dedup
│   │   ├── runtime.json        # Daemon-mutated runtime: partitions, active partition, byte counters, index_epoch, SET CAPACITY
│   │   ├── chunks.idx          # Per-cartridge chunk index (64-byte records)
│   │   ├── blocks-p0.idx       # Block index for partition 0 (16-byte records)
│   │   ├── blocks-p1.idx       # Block index for partition 1 (LTFS only)
│   │   └── .staging/
│   │       └── chunk-<id>.dat  # Active (unsealed) chunk only
│   └── BARCODE2/
│       └── …
├── chunks/                     # Per-backend content-addressed pools
│   ├── primary/                # Names match `cloud.backends:` entries
│   │   ├── 00/
│   │   │   ├── cd/
│   │   │   │   └── 00cdef…ab.dat   # `<aa>/<bb>/<full_blake3>.dat`, two 2-hex-char shards (65 536-way fanout)
│   │   └── …
│   └── archive/
│       └── …
└── audit/                      # Append-only event journal (always-on)
    ├── chain.state             # {last_seq, last_hash, last_file}
    ├── audit-2026-05-03.jsonl  # Today's file: one JSONL entry per line
    ├── audit-2026-05-02.jsonl.zst  # Rotated yesterday: zstd-compressed (default)
    └── pending/                # CLI daemon-down audit queue (drained on next start)
        ├── 2026-05-03T11-04-29.001Z-7e3a91bc.json
        └── failed/             # Quarantine for entries replay couldn't append
```

Sealed chunks live in **per-backend content-addressed pools**. The
path is `<data_dir>/chunks/<backend>/<aa>/<bb>/<full_blake3>.dat` for
`--dedup global` cartridges, where the pool is shared, or
`<data_dir>/chunks/<backend>/<barcode>/<aa>/<bb>/<full_blake3>.dat` for
`--dedup local` cartridges, which get a per-cartridge namespace. The
leading `<backend>` segment names the cloud backend the cartridge is
bound to — a configured `cloud.backends:` entry. The `<aa>` and `<bb>`
segments are the first two and next two hex characters of the chunk's
BLAKE3 hash; this gives a 65 536-way fanout, which keeps any single
leaf directory from growing unbounded. While a chunk is still being
written, it lives separately at `<root>/.staging/chunk-<id>.dat`, and
it stays there until it rolls — meaning the chunk-size threshold is
hit — or the cartridge unloads (`Cartridge::Drop` → `flush_and_seal`).

See [`DEDUP.md`](DEDUP.md) for the full content-addressed-dedup model
(pool layout, the `--dedup local|global` scope, cross-backend rules,
chunking-mode interactions, encryption / compression trade-offs).

### Library Topology — `library.json`

```json
{
  "version": 1,
  "num_storage_slots": 40,
  "num_mail_slots": 0,
  "num_drives": 3,
  "lto_generation": 8,
  "firmware": null,
  "chassis_serial": "TVL3F8A2C7B1E0D",
  "partitions": []
}
```

`firmware` is optional; leaving it `null` selects the per-LTO default
(`TVL7` or `TVL8`). `chassis_serial` is a 14-byte uppercase string —
a 3-character `TVL` prefix followed by 11 hex chars, so 44 bits of
entropy — minted at `library init` and persisted across restarts. It
shows up in two places: as INQUIRY VPD `0xB3` (Automation Device
Serial) on every drive LUN, and as the prefix of VPD `0x80` (Unit
Serial) on the changer LUN, where the full value is
`<chassis_serial>_LL<NN>` with `NN` the 1-based partition index. A
pre-field library that has no `chassis_serial` falls back to the
literal `THUR-CHG-001`, which keeps existing backup-software catalogs
matching. An operator with a rare migration need — re-platforming, a
DR restore — can edit `library.json` directly. The `partitions` field
is an optional array of `LibraryPartition` records: empty or missing
means the library is unpartitioned, and when it is present, every
storage slot, mail slot, and drive must belong to exactly one
partition.

```json
{
  "name": "alpha",
  "storage_slots": { "start": 0,  "end": 20 },
  "mail_slots":    { "start": 0,  "end": 2  },
  "drives":        [0, 1]
}
```

In a partition record, `storage_slots` and `mail_slots` are half-open
`[start, end)` ranges over the 0-indexed chassis-level address space,
while `drives` is an explicit list of 0-indexed drive ids — a
partition's drive subset need not be contiguous. Partition names are
1-64 characters and unique within a library. Per-partition CHAP
credentials are deliberately *not* stored here; they live in
`thurvtl.yaml` under `iscsi.auth.users[].partition`, so that the
secrets stay in the same config file the operator already manages.

### Library Inventory — `inventory.json`

```json
{
  "version": 1,
  "storage_slots": [{ "id": 1, "occupied": true, "barcode": "TAPE001", "home_slot": null }, …],
  "mail_slots":    [],
  "drives":        [
    {
      "id": 1,
      "occupied": false,
      "barcode": null,
      "home_slot": null,
      "mfg_serial": "TVLA1B2C3D"
    }, …
  ]
}
```

`mfg_serial` is a 10-byte string — a 3-character `TVL` prefix plus 7
hex chars, so 28 bits of entropy — minted once when the drive is added
(at `library init` or `library modify --drives N`) and persisted
across restarts. The same value is surfaced through INQUIRY VPD `0xB1`
(Manufacturer-Assigned Serial), through LOG SENSE page `0x14`
parameter `0x0040`, and as the drive LUN's VPD `0x80` Unit Serial —
all three must report the same string. A pre-field inventory with no
`mfg_serial` falls back to the literal `THUR-MFG-NNN`, with `NNN` the
LUN.

Write paths to `library.json` and `inventory.json` are guarded by an
in-process write-lock plus atomic-rename, so the daemon and the
chassis-assembly CLI commands (`library init`, `library modify`,
`library partition …`) cannot tear either file. The older
cross-process `fs2` lock was retired in 2026-05, because the daemon is
now the sole live writer and a cross-process lock is no longer needed.

### Cartridge Manifest — `manifest.json` (identity)

```json
{
  "label": "TAPE001",
  "uuid": "8e2a4f0d6c1b9a3e7f10428953dcaef5",   // 16 random bytes, hex; sticky
  "chunk_size_bytes": 0,           // 0 on FastCDC tapes; legacy field
  "chunking": { "mode": "fast_cdc", "min": 1048576, "avg": 8388608, "max": 33554432 },
  "capacity_gb": 12000,
  "lto_generation": 8,             // drive-compat generation; currently 8 only
  "backend": "primary",            // sticky cloud-backend name; required
  "worm": false,                   // sticky WORM flag; default false
  "dedup": "global",               // sticky dedup scope; "global" or "local"
  "encryption": {                  // optional; absent on plaintext cartridges
    "algorithm": "aes_256_gcm",
    "keystore_backend": "kms-prod",     // name from keystore.backends: in YAML conffile
    "wrapped_dek": "<base64-ciphertext>"  // absent for the `local` backend (sidecar holds it)
  }
}
```

`manifest.json` is **creation-frozen**: it is written once at
`cartridge create` and the hot path never rewrites it. Only identity
and sticky fields live in it. The only writers after creation are
operator-driven identity mutations — `cartridge migrate` rewrites
`backend`; archive provenance stamping adds `archived_from_backend`
and `archived_at`; and `cartridge key migrate`, run daemon-down,
rewrites `encryption.keystore_backend` and `encryption.wrapped_dek`.

The `encryption` block is **opt-in** per cartridge. It is absent on
plaintext cartridges, which is the default unless the operator passes
`--encrypt --keystore NAME`. When the block is present, every chunk in
the pool is AES-256-GCM ciphertext over the plaintext: the 16-byte GCM
tag is appended to the chunk file, the pool hash is computed over the
ciphertext, and the IV is derived at read time as `derive_iv(uuid,
chunk_id, 0)`. The keystore lifecycle and the AME composition rules
are in [`AUTH.md`](AUTH.md) § *VTL keystore backends*.

Everything that mutates at runtime — the partition layout, the FETB
counter, the index-backup epoch, the host-set capacity proportion, the
pending partition layout — is kept out of the manifest and lives in a
sibling `runtime.json` sidecar (described in the next section).
Neither the per-block nor the per-chunk index is stored in either file
either (see "Block Index Files" and "Chunk Index File" below).
Keeping all of that out is what makes the manifest **O(1) in size** no
matter how many chunks or blocks the cartridge holds — it never grows
per-write.

### Cartridge Runtime State — `runtime.json` (sidecar)

```json
{
  "partitions": [
    { "capacity_mib": 0 }
  ],
  "active_partition": 0,
  "pending_partition_layout": null,
  "set_capacity_proportion": 65535, // 16-bit fraction; 65535/MAX = full native
  "index_epoch": {                  // per-index-file restore stamp; see "Index page backup"
    "chunks":    { "pages": 1,  "page_size": 1048576, "epoch": 7,  "file_size": 4128 },
    "blocks-p0": { "pages": 4,  "page_size": 1048576, "epoch": 7,  "file_size": 3145760 }
  },
  "host_bytes_written":   5368709120, // lifetime FETB; pre-dedup, pre-compression; reset on ERASE
  "host_bytes_read":      4294967296, // lifetime plaintext bytes served to the host on READ
  "backend_bytes_written": 2147483648, // lifetime on-wire bytes PUT to cloud; post-dedup, post-compression
  "backend_bytes_read":    1073741824  // lifetime bytes fetched from cloud on a chunk cache miss
}
```

`runtime.json` is rewritten at every runtime-mutating boundary — a
cross-partition LOCATE, MODE SELECT 0x11, FORMAT MEDIUM, ERASE, SET
CAPACITY, a manifest backup — through `Cartridge::persist_runtime`,
which does an atomic tmp+fsync+rename. It is rewritten once more when
the cartridge is unloaded (a MOVE MEDIUM out of a drive drops the
in-memory `Cartridge`), so the byte counters below survive a pure
sequential restore — a workload that triggers none of those SCSI
boundaries. A daemon crash still loses counter movement since the
last persist. The reason these fields are split out of the manifest
is precisely that the identity file then stays byte-stable after
creation, so an out-of-band identity mutation like `cartridge
migrate` cannot race against a hot-path persist.

`runtime.json` carries four lifetime byte counters. `host_bytes_written`
is the cartridge's FETB counter: incremented in `Cartridge::write_data`
by the **plaintext** byte length — counted before drive-side
compression and before chunk dedup. `host_bytes_read` is its read-side
mirror, incremented by the plaintext length handed back to the host on
each READ (post-decrypt, post-decompress; filemark reads do not count).
`backend_bytes_written` is the on-wire bytes PUT to cloud — post-dedup,
post-compression, the real backend storage cost — and is bumped as each
chunk upload outcome is applied (a cross-namespace dedup hit performs
no PUT and so does not count). `backend_bytes_read` is the bytes
fetched from cloud on a chunk cache miss, bumped by the live-session
prefetch hook and the async refetch path. The gap between a host
counter and its backend counterpart is the dedup + compression saving
on the write side and the cache hit rate on the read side. All four
are monotonic — never decremented on an overwrite, a rewind, or ALLOW
OVERWRITE. ERASE and FORMAT MEDIUM reset all four to 0, because the
medium is now logically blank. Restore-archive preserves the source
cartridge's values. The FETB telemetry sampler walks every
`tapes/<barcode>/runtime.json` every 6 hours and emits the
`host_bytes_written` sum as a `fetb.sample` audit event.

`uuid` is sticky: 16 random bytes drawn from the OS CSPRNG at create
time and never modified afterward. It is mixed into the per-block
AES-GCM IV derivation, which guarantees that two cartridges loaded
with the same key can never share an IV — real LTO drives do the
analogous thing, mixing a per-tape nonce into their position-based IV.

`backend` is sticky — set at create time and never modified. Its
value names a configured `cloud.backends:` entry, and the daemon
refuses to start if any cartridge references a backend that is not
configured. A manifest with an empty or missing `backend` cannot be
opened at all: `Cartridge::open` errors. Every cloud operation —
upload, manifest backup, prefetch, refetch — routes through the named
backend, and the chunk-pool sharding under
`<data_dir>/chunks/<backend>/...` makes that routing physical on disk.

`worm` is sticky — set at create time via `cartridge create --worm`.
When it is `true`, the cartridge enforces append-only semantics:
writes are allowed only at EOD, and ERASE, FORMAT MEDIUM, and ALLOW
OVERWRITE are all refused. See "WORM cartridges" below.

`lto_generation` carries the cartridge's **drive-compat generation**.
Because VTL ships as a clean LTO-8 drive, cartridges are always
`lto_generation = 8`, which the daemon validator enforces. The field
is kept in the schema only for forward-compatibility with LTO-9/10
(see [`LTO-9.md`](LTO-9.md)); LTO-8 capacity is 12 TB. The
`load_cartridge` gate refuses any cartridge whose `lto_generation`
exceeds the chassis-wide drive generation, returning CHECK CONDITION
+ ILLEGAL REQUEST + ASC/ASCQ 0x30/0x00 ("INCOMPATIBLE MEDIUM
INSTALLED").

`lto_generation` is chosen at `cartridge create` time in two steps:

1. explicit `--lto-generation 8`,
2. otherwise: falls back to the library's configured generation.

The barcode itself is just a label — the generation is never inferred
from its suffix. A cartridge carries its generation in the manifest,
not in its label.

#### Block Index Files — `blocks-p<N>.idx`

The block index is a per-partition file of fixed-size records at
`<cartridge_root>/blocks-p<N>.idx`. Every block or filemark write
places its record positionally — `pwrite_at(HEADER_SIZE + lba *
RECORD_SIZE)` — so the index grows in place by LBA and the manifest
itself never grows per-write.

**Header (32 bytes, written once at file create)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..4` | `magic` | ASCII `NVBI` |
| `4..8` | `version` | u32 LE; current `1` |
| `8..12` | `record_size` | u32 LE; current `16` |
| `12..32` | reserved | zeroed |

**Record (16 bytes per LBA)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..4` | `chunk_id` | u32 LE; per-cartridge chunk sequence |
| `4..8` | `offset` | u32 LE; byte offset within chunk file |
| `8..12` | `len` | u32 LE; on-disk byte length (includes 16 B GCM tag if encrypted; 0 for filemark) |
| `12` | `flags` | bit 0 = filemark (1) / data (0); bits 1-3 = encryption algo (0 = none, 1 = AES-256-GCM); bits 4-6 = compression algo (0 = none, 1 = lz4, 2 = zstd, 3 = sldc); bit 7 = reserved |
| `13..16` | reserved | zeroed |

The next LBA is derived from the file size: `next_lba = (file_size −
HEADER_SIZE) / RECORD_SIZE`. Even the worst case is small — at LTO-8,
200 M blocks works out to roughly 3.2 GB per cartridge, under 0.1 % of
a full tape's data.

The following are deliberately **not** stored, with the reasoning for
each:

| Item | Reason |
|------|--------|
| Per-block IV | Real LTO drives derive IV from the block's recorded position. Same here: `IV = BLAKE3(uuid ‖ chunk_id_le ‖ offset_le)[..12]`. Reproducible at decrypt time. |
| AES-GCM auth tag | Concatenated into the chunk file's bytes (the standard GCM `ciphertext ‖ tag` form). `len` already includes those 16 B. |
| Per-block plaintext checksum | Real LTO doesn't expose one to the host. Drive-internal ECC + recorded-block CRC handle integrity. Our equivalent: chunk-level BLAKE3 (`ChunkRec.hash`) plus AES-GCM auth tag for encrypted blocks plus codec frame CRC for compressed plaintext blocks. |
| Uncompressed size | lz4-frame and zstd self-frame their content; the decompressor returns the right number of bytes without an out-of-band hint. |

#### Chunk Index File — `chunks.idx`

The chunk index is a per-cartridge file of fixed-size records at
`<cartridge_root>/chunks.idx`. Records are positional, indexed by
`chunk_id` — `pwrite_at(HEADER_SIZE + chunk_id * RECORD_SIZE)` — so
every per-chunk mutation, whether marking a chunk uploaded or
transitioning its `location`, is a single O(1) `pwrite`.

The last-accessed timestamps that drive disk-cache LRU eviction are
kept out of this file, in a separate `lru.idx` sidecar (see below).
They are a local cache hint, not part of the cloud-replicated index.

There is one `chunks.idx` per cartridge, not one per partition.
`chunk_id`s span partitions, so unlike `blocks-pN.idx` the file cannot
be partitioned along with them.

**Header (32 bytes, written once at file create)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..4` | `magic` | ASCII `NVCI` |
| `4..8` | `version` | u32 LE; current `1` |
| `8..12` | `record_size` | u32 LE; current `64` |
| `12..32` | reserved | zeroed |

**Record (64 bytes per `chunk_id`)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..4` | `size` | u32 LE; sealed on-disk byte count. Width matches `BlockRec.offset` (u32) — a chunk can never hold more than 4 GiB |
| `4..36` | `hash` | 32-byte raw BLAKE3 of sealed chunk bytes; valid iff `hash_present` flag is set (zeroed for unsealed staging chunks) |
| `36` | `flags` | bit 0 = `hash_present` (sealed); bit 1 = `uploaded`; bits 2-3 = `location` (0=LocalOnly, 1=CloudOnly, 2=Both); bits 4-6 = compression algo (0=none, 1=lz4, 2=zstd, 3=sldc); bit 7 reserved |
| `37..64` | reserved | 27 B, zeroed |

The next id is derived the same way the block index derives its next
LBA: `next_id = (file_size − HEADER_SIZE) / RECORD_SIZE`. The
`chunk_id` is purely positional — per-cartridge monotonic 0, 1, 2, …,
never stored explicitly — exactly like the LBA in `blocks-pN.idx`.
ERASE and FORMAT MEDIUM truncate the records region with `ftruncate`.
The worst case is again small: at LTO-8, roughly 1.5 M chunks at the
8 MiB FastCDC average is about 96 MB per cartridge, under 0.001 % of a
full tape's data.

The following are deliberately **not** stored, with the reasoning for
each:

| Item | Reason |
|------|--------|
| `chunk_id` | Derivable from offset (positional, like LBA in `blocks-pN.idx`). |
| `compressed_size` | Was a write-only field in the legacy JSON manifest — set by the upload worker but never read. Compression metrics are emitted directly from the backend at upload time. |
| `last_accessed` | Lives in the local-only `lru.idx` sidecar (see below) so the read path's `touch` doesn't dirty cloud-replicated metadata pages. |

The 64-byte record breaks down as 4 bytes of size, 32 of hash, 1 of
flags, and 27 reserved — so only about 37 bytes are load-bearing
today. The slack is intentional headroom: future flag bits or fields
can be added without bumping the on-disk format.

#### LRU Sidecar — `lru.idx`

`lru.idx` is a per-cartridge file of fixed-size records at
`<cartridge_root>/lru.idx`. It is positional and mirrored 1:1 with
`chunks.idx` — one u64 LE epoch-seconds value per `chunk_id`, updated
on every read or write of the corresponding chunk, and consumed by
`DiskCacheManager` for LRU eviction.

This file is **local-only**. It never gets a `.dirty` sidecar, is
never enumerated by `index_backup.rs`, and is never restored on a
cold-bucket DR. A fresh host simply opens it zero-filled to match
`chunks.idx.next_id()`; the first eviction cycle then picks the oldest
chunks uniformly, and subsequent cycles converge on real recency as
touches arrive. A reset or a corrupt header just causes it to be
rebuilt empty.

**Header (32 bytes)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..4` | `magic` | ASCII `TVLI` |
| `4..8` | `version` | u32 LE; current `1` |
| `8..12` | `record_size` | u32 LE; current `8` |
| `12..32` | reserved | zeroed |

**Record (8 bytes per `chunk_id`)**

| Bytes | Field | Notes |
|-------|-------|-------|
| `0..8` | `last_accessed` | u64 LE; epoch seconds; 0 = never touched (sorts oldest-first for eviction) |

#### `ChunkingMode`

```json
{ "mode": "fixed", "size_bytes": 8388608 }
{ "mode": "fast_cdc", "min": 1048576, "avg": 8388608, "max": 33554432 }
```

The chunking mode is sticky for the cartridge's lifetime. New
cartridges default to `fast_cdc` with a 1 MiB / 8 MiB / 32 MiB
min/avg/max.

#### Migration

A legacy fixed-chunking manifest — one with no `chunking` field at all
— is interpreted as `Fixed { size_bytes: chunk_size_bytes }`. Older
cartridges than that cannot be opened: a manifest predating block-index
files (it has inline `partitions[i].blocks` and no `uuid` field), or
one predating chunk-index files (it has an inline `chunks` array), is
rejected outright. The clean break is intentional — the remedy is to
recreate the cartridge.

---

## Audit Log

The audit log is an append-only event journal under
`<data_dir>/audit/`. It is always on and always tamper-evident: the
hash-chained mode is the *only* mode, and there is no knob to disable
it.

### Files

| File | Purpose |
|---|---|
| `audit-YYYY-MM-DD.jsonl` | One entry per line, today's file (or any non-rotated file). |
| `audit-YYYY-MM-DD.jsonl.zst` | Rotated file, zstd-compressed level 3 (default; `audit.compress_rotated: false` keeps plain). |
| `chain.state` | `{last_seq, last_hash, last_file}` JSON; rewritten after every append. |
| `pending/<RFC3339>-<rand>.json` | Daemon-down CLI audit queue (`library init` / `library modify`); drained at next daemon start. |
| `pending/failed/` | Quarantine for queued entries the daemon couldn't append (chain broken, malformed JSON). Investigated out-of-band. |

Rollover is daily, at UTC midnight: it happens on the first append
after the date has changed. Compression of the just-rotated file runs
synchronously, on that same rollover path.

### Entry schema

```json
{
  "seq": 12345,
  "ts": "2026-05-03T14:22:01.123456Z",
  "actor": {
    "kind": "cli|daemon|rest|system",
    "user": "root",            // optional
    "addr": "127.0.0.1"        // optional
  },
  "op": "cartridge.create",
  "params": { "barcode": "TAPE001", "backend": "primary", "worm": false },
  "result": "ok|error",
  "error": null,                // populated when result=error
  "prev_hash": "blake3:…",
  "entry_hash": "blake3:…"
}
```

### Hash chain

Each entry's hash is `entry_hash = blake3(canonical_json({seq, ts,
actor, op, params, result, error, prev_hash}))` — that is, over every
field *except* `entry_hash` itself. "Canonical JSON"
here means serde_json with the struct field order fixed by
declaration, and `serde_json::Map` keeping nested object keys sorted
(it is BTreeMap-backed by default), so the byte serialization is
deterministic. The next entry's `prev_hash` is set equal to this
`entry_hash`, which is what links the chain. The genesis entry has no
predecessor, so its `prev_hash` is `blake3:0000…0000` (the
`GENESIS_PREV_HASH` constant). The chain does not restart at a daily
rollover — a new file's first entry chains off the previous file's
last `entry_hash`.

### Chain reset (sentinel discontinuity)

An `audit.chain_reset` entry records operator recovery after a verify
failure — `thurvtl system audit rotate --accept-break`, which
writes `params.trigger: "break_recovery"`.

It writes:
- `op: "audit.chain_reset"`
- `prev_hash: "blake3:reset:<old_last_hash_hex>"` — sentinel that
  intentionally breaks linkage
- `params: {old_last_hash, trigger, …extra}`
- `entry_hash`: computed normally

Entries after the reset chain off the reset entry's own `entry_hash`,
in the normal way. The discontinuity itself remains visible forever:
`verify` recognizes the sentinel shape and so accepts the chain_reset
entry's non-matching `prev_hash`, but it deliberately does not "heal"
the prior tail — the break stays on the record.

### Logged operations (current set)

Daemon-side: `daemon.start`, `daemon.stop`.

CLI-side: `cartridge.create`, `cartridge.import`,
`cartridge.export`, `cartridge.legal_hold.set`,
`cartridge.legal_hold.clear`, `library.init`, `library.modify`,
`library.load`, `library.unload`, `library.move`, `gc.run`.

iSCSI / SCSI-side (actor `kind:"iscsi"`, `user` = initiator IQN if
the initiator advertised one, `addr` = peer ip:port):
- `iscsi.move_medium` — SCSI MOVE MEDIUM (CDB 0xA5). Backup-software
  driven drive load/unload via the changer. `params`:
  `{action:"load|unload|move", transport, src, dst, invert, barcode?}`.
  `barcode` is set on unload (recorded before the drive slot is cleared);
  null on load/move.
- `iscsi.encryption.set_key` — SECURITY PROTOCOL OUT (CDB 0xB5)
  protocol 0x20 / SPSP 0x0010 with a Set Key payload. **Metadata only:**
  `params: {drive, lun, algorithm_index}`. The key bytes are *never*
  written to the audit log; the algorithm index is the only contents
  of the SP-OUT payload that gets recorded.
- `iscsi.encryption.clear_key` — SECURITY PROTOCOL OUT 0x20/0x0010
  with a Clear payload. `params: {drive, lun}`.
- `iscsi.drive_compression` — MODE SELECT(6) / MODE SELECT(10)
  carrying page 0x0F with the DCE bit. `params:
  {drive, lun, dce, cdb:"MODE_SELECT_6|MODE_SELECT_10"}`. Only fires
  when page 0x0F is actually present in the parameter list (not on
  every MODE SELECT).
- `iscsi.chap.success` — CHAP login succeeded. `params:
  {chap_user, initiator, algorithm}`. `algorithm` is the negotiated
  digest name (`"MD5"` / `"SHA-1"` / `"SHA-256"` / `"SHA3-256"`).
- `iscsi.chap.failure` — CHAP login failed. `params:
  {chap_user?, initiator?, reason}`. Reasons:
  `"invalid_response"` (CHAP_R didn't match), `"verify_error"`
  (verifier returned an error), `"skipped_security_stage"` (initiator
  tried to enter OPNEG without completing CHAP).

Special: `audit.chain_reset` (reset entry).

Read paths — INQUIRY, READ, TEST UNIT READY, and the like — are not
logged at all, because their volume would drown the signal. The iSCSI
surface logs only mutations and authentication outcomes.

### Rate-limited events (rollup entries)

A small set of host-driven failure events is rate-limited, to keep a
misconfigured initiator that retries forever from flooding the chain.
The mechanism is a 60 s window per event class: the first event in a
window is emitted as a normal audit entry, and every subsequent event
with the same `(op, peer, key-fields)` tuple inside that window is
silently counted instead of written. When the window expires, a flush
task — which ticks every 10 s — appends a single rollup entry whose
`params` carry the suppression count. On shutdown the daemon drains
any in-flight windows, so the trailing count still makes it into the
chain.

Currently rate-limited:
- `iscsi.chap.failure` — bucket key `(op, peer_addr, chap_user, reason)`
- `iscsi.move_medium` with `params.refused` set
  (`partition_fence` / `medium_removal_prevented`) — bucket key
  `(op, peer_addr, refused_reason)`. The Ok success path and the
  generic `Err` path (no `refused` field) are not rate-limited.

A rollup entry has the same `op` as the original, `actor` cloned from
the first emission, `result: "error"` with detail
`"<N> additional event(s) suppressed in <W>s window"`, and
`params` shape:
```json
{
  "suppressed_count": 7,
  "window_seconds": 60,
  "key": "iscsi.chap.failure:10.0.0.5:42501:user1:invalid_response"
}
```

A chain reader can tell a rollup apart unambiguously: the rollup
carries a `suppressed_count` field, whereas the original first
emission has the ordinary per-op shape and no such field. The
implementation is `core-mediachanger::AuditRateLimiter` — a 60 s
window that fails open if its mutex is poisoned — flushed by
`run_audit_ratelimit_flush` in the daemon.

### CLI exit codes

`thurvtl system audit verify` reports its result through the exit
code:
- 0 — chain valid
- 1 — chain break detected
- 2 — IO / file-missing error

---

## Cloud Object Layout

Everything Thur VTL writes to the cloud lives under a single
`<prefix>/` in an S3 or GCS bucket, or an Azure Blob Storage
container. The keyspace is laid out like this:

```
<prefix>/
├── chunks/
│   ├── 00/
│   │   └── cd/
│   │       └── 00cdef…ab.dat        # <aa>/<bb>/<full_blake3>.dat
│   └── …
└── manifests/
    └── BARCODE1/
        ├── manifest-latest.json
        ├── manifest-2026-05-02T08-12-30Z.json
        ├── chunks/                   # Delta pages of chunks.idx
        │   ├── page-000000.dat
        │   └── …
        ├── blocks-p0/                # Delta pages of blocks-p0.idx
        │   ├── page-000000.dat
        │   └── …
        └── …                         # Last 10 manifest versions kept
```

- **Chunk key.** A `--dedup global` cartridge writes chunks to
  `<prefix>/chunks/<aa>/<bb>/<full_blake3>.dat`. Each backend has its
  own bucket (and optional `prefix`), so the cloud key is
  backend-flat: the on-disk per-backend sharding under
  `<data_dir>/chunks/<backend>/...` does not bleed into the cloud key
  shape. A `--dedup local` cartridge instead writes under a
  per-cartridge namespace,
  `<prefix>/chunks/<barcode>/<aa>/<bb>/<full_blake3>.dat`. The dedup
  model and the upload worker's HEAD-check behavior are in
  [`DEDUP.md`](DEDUP.md).
- **Manifest key.** `<prefix>/manifests/<barcode>/manifest-latest.json`
  is the always-current pointer, and alongside it sit timestamped
  versions kept for rollback — the last 10 are retained. Each of these
  objects is not the on-disk manifest verbatim but a **bundle** that
  combines the cartridge's identity manifest with its runtime sidecar:

  ```json
  {
    "manifest": { ...identity (see "Cartridge Manifest" above)... },
    "runtime":  { ...runtime (see "Cartridge Runtime State" above)... }
  }
  ```

  A cold-bucket restore parses this bundle and writes both
  `manifest.json` and `runtime.json` to disk locally. The key path is
  unchanged from earlier formats — only the object body shape changed.
- **Index page keys.** The `chunks.idx` and each `blocks-p<N>.idx` are
  backed up as deltas under
  `<prefix>/manifests/<barcode>/chunks/page-<NNNNNN>.dat` and
  `<prefix>/manifests/<barcode>/blocks-p<N>/page-<NNNNNN>.dat`. Each
  object is a 1 MiB slice of the flat-record index file, addressed by
  its sequence number — so the same key is simply overwritten on the
  next mutation. The six-digit zero-padded numbering covers a roughly
  1 TB worst case at 1 MiB granularity. Page `NNNNNN` covers bytes
  `[NNNNNN * 1 MiB, (NNNNNN + 1) * 1 MiB)` of the index file (header
  plus records area). Not every page is uploaded each pass: a
  per-file `DirtyPageTracker` (sidecar `<index>.dirty`) flags which
  pages changed, and only those go up on a manifest-backup pass —
  typically just the trailing page on an append-dominated workload,
  plus whatever page held an in-place mutation. The `index_epoch` map
  records, for each file label (`chunks`, `blocks-p0`, …), the page
  count, the page size, a monotonic upload epoch, and the logical byte
  size at snapshot time, which is exactly what a cold-bucket restore
  needs to know what to fetch and how large to grow each file. The
  sentinel, `manifest-latest.json`, is written **last** in every pass
  — the same ordering rule as legal-hold — so that a torn upload
  leaves the sentinel still pointing at the previous, page-consistent
  epoch.
- **Compression.** When `cloud.compression.algorithm != none`, chunk
  bytes are compressed on the way up (zstd level 3 by default, with
  lz4 also available) and the algorithm chosen is recorded in the
  per-cartridge manifest. Compression runs **post-dedup** — only after
  the chunk has sealed and a hash exists. The marker itself
  (`compression: zstd|lz4|none`, plus `compression_level` for zstd) is
  stored in S3 object metadata, GCS custom metadata, or Azure blob
  metadata.

The `local` backend uses this same key shape, just rooted at
`cloud.local.root_dir` on the filesystem instead of in a bucket.

Azure Blob Storage works the same way: blob names are the identical
key strings, `<prefix>/` and all, with the container playing the role
of the bucket. The authentication mechanics — per-backend `auth:`
blocks versus the default credential chain — are in
[`AUTH.md`](AUTH.md).

---

## Cross-region DR — `thurvtl library restore`

This verb brings a fresh host up from a cold mirror bucket. It can
only replicate what the bucket actually holds, which is the cartridge
state — manifests, index pages, and chunks. The chassis topology in
`library.json` is **not** cloud-replicated, so the operator has to
declare it themselves with `library init` on the new host. Restored
cartridges are seated into storage slots sequentially in barcode-sort
order; if a specific layout matters, run `changer move` afterward.

The command is daemon-down — the daemon must not be running:

```
thurvtl library restore --backend NAME
                            [--barcodes B1,B2,...]
                            [--dry-run]
                            [--allow-existing]
```

- `--backend NAME` — required when `cloud.backends:` declares more
  than one backend; inferred when exactly one is configured.
- `--barcodes` — optional comma-separated allowlist; default is every
  barcode whose `manifest-latest.json` sentinel is reachable under
  `manifests/`.
- `--dry-run` — lists what would be restored without writing anything
  under `<data_dir>/tapes/`. No audit entry; no inventory mutation.
- `--allow-existing` — skip a barcode whose local cartridge directory
  already exists. Without this flag, a pre-existing local directory is
  a fatal per-cartridge error.

### Phases

The restore runs in four phases:

1. **Discovery.** `CloudBackend::list_objects("manifests/")`
   enumerates every key under that prefix. A barcode that has a
   `manifest-latest.json` sentinel is kept; anything without one — an
   in-flight upload, a torn write — is surfaced as an orphan hint
   rather than restored.
2. **Per-cartridge restore.** Each selected barcode goes through the
   single-cartridge cold-bucket path
   (`Cartridge::open_with_cloud_async` → the missing-locally branch of
   `load_manifest_async`), which fetches `manifest-latest.json`, then
   every index page enumerated in `index_epoch`, and writes both to
   `<data_dir>/tapes/<barcode>/`. A failure on one cartridge does
   **not** abort the batch — every cartridge is attempted and its
   outcome reported individually. Chunks are *not* downloaded in this
   phase; they lazy-load on the first host read, through
   `read_block_async`'s cloud-refetch path.
3. **Inventory rebuild.** The cartridges that restored successfully,
   sorted by barcode, are seated into storage slots via
   `Library::add_or_create_tape` — which short-circuits its create
   path when the cartridge directory already exists, leaving only the
   slot assignment to do. If the cartridge count exceeds the free slot
   count the restore refuses, and the error names the exact remediation
   (`--slots >= N`).
4. **Audit footprint.** Each invocation writes one `library.restore`
   audit entry, queued under `<audit_dir>/pending/` and replayed into
   the chain on the next daemon start. It is suppressed on
   `--dry-run`. The payload carries `backend`, `discovered`,
   `selected`, `restored`, `failed`, `skipped_existing`,
   `filtered_out`, and `allow_existing`.

### Operator runbook (cold-bucket DR)

On a fresh host, a cold-bucket recovery is three commands:

```
# 1. Bring up chassis topology (operator's call, not cloud-replicated).
thurvtl library init --slots N --drives M --lto-generation 7|8

# 2. Restore cartridges from the mirror.
thurvtl library restore --backend mirror

# 3. Start serving.
systemctl start thurvtld
```

This assumes the cloud provider has already replicated the source
bucket to the mirror region out-of-band — S3, GCS, and Azure all offer
bucket-level cross-region replication. Thur VTL itself does not drive
cross-bucket replication; that is a separate feature ("cartridge
replication", see `ROADMAP.md`).

### Exit codes

- 0 — every selected cartridge restored, inventory rebuilt cleanly.
- 1 — at least one cartridge failed to restore, or the slot-overflow
  guard fired (cartridge count > library's free slot count). Audit
  entry is recorded with the failure detail.

### What this verb does NOT cover

- **Chassis topology.** The operator runs `library init` first; the
  topology knobs (`--slots / --drives / --lto-generation`) are never
  pulled from cloud state.
- **Daemon-routed warm-host restore.** `library restore` refuses to
  run if a daemon is alive on the data dir. Refreshing a single
  cartridge's metadata against a live daemon is a different operation
  altogether — closer to `system verify --repair`, which is not
  currently shipped.
- **Eager chunk pre-fetch.** The restore is metadata-only by design;
  the first host read is what pulls chunks in.
- **App-driven cross-region mirroring** — synchronous writes to two
  buckets. That is the cartridge-replication feature in `ROADMAP.md`.

---

## Cartridge migration — `thurvtl cartridge migrate`

Migration moves a single cartridge from one cloud backend to another.
The cartridge keeps its barcode and its logical identity — the only
field that actually changes is `manifest.backend`. It runs as a
daemon-routed admin job (kind `cartridge.migrate`), backed by
`core_stream::cartridge_migrate::run_migrate`.

There are two modes:

- **`move`** (the default) — copy the chunks and manifest backups from
  the source to the target, flip the manifest, then delete the source
  objects.
- **`rebind`** — a pointer rewrite only. It HEAD-verifies the target
  before the flip (unless `--no-verify`) and never touches the source.
  This mode is for operators who already run bucket-level replication
  out-of-band.

### CLI surface

```
thurvtl cartridge migrate <BARCODE> --target-backend <NAME>
    [--mode move|rebind]   (default: move)
    [--no-verify]          (rebind only; skip HEAD pass)
    [--dry-run]
```

### Move mode — phases

1. **Discover chunks.** Walk `chunks.idx`; every record that has a
   hash contributes one `(hash, cloud_key)` pair. The cloud key shape
   is the backend-independent one from § Cloud Object Layout.
2. **Copy chunks.** Issue a HEAD on the target first — this is what
   makes a retry idempotent — and if the chunk is missing,
   `source.download_chunk(key)`, then BLAKE3-verify, then
   `target.upload_chunk(key, bytes)`.
3. **Copy manifest backups.** Every key under `manifests/<barcode>/`
   on the source is copied across: JSON keys via `upload_manifest` /
   `download_manifest`, binary index pages via `upload_chunk` /
   `download_chunk`.
4. **Move local pool files.** Files at
   `<data_dir>/chunks/<source>/[<ns>/]<aa>/<bb>/<hash>.dat` are
   renamed under `<data_dir>/chunks/<target>/[<ns>/]<aa>/<bb>/<hash>.dat`.
5. **Commit.** An atomic temp+rename of `manifest.json` with `backend`
   set to the new name. This is *the* commit point — see crash
   semantics below.
6. **Delete source (best-effort).** Manifest backups are always
   deleted. Chunks are deleted only under `Local` dedup; under
   `Global` dedup a chunk may still be referenced by a sibling
   cartridge on the source backend, so the chunks are left for
   `system gc` to reclaim as orphans. A failure here becomes a
   warning, not a migration failure.

### Rebind mode — phases

1. **Discover chunks.** Same as move.
2. **Verify** (unless `--no-verify`). HEAD every chunk key on the
   target, plus `manifests/<barcode>/manifest-latest.json`. Any single
   miss aborts the rebind with `RebindTargetMissing { keys }` (the key
   list is capped at 16); because nothing has been mutated yet, the
   abort is clean. The source backend is never contacted in this mode.
3. **Move local pool files.** Same as move.
4. **Commit.** Same as move.

### Refuse-gates

- Daemon must be running (admin socket is the only entry point).
- Cartridge must not be loaded in any drive
  (`find_drive_for_loaded_cartridge` on `inventory.json`).
- Target backend must exist in `cloud.backends:`.
- Source ≠ target.
- WORM cartridges require the target's `retention_mode` to be
  `governance` or `compliance`.

### Audit

One entry per invocation:

- `cartridge.migrated` — move mode. Params: `barcode`, `mode: "move"`,
  `from_backend`, `to_backend`, `chunks_total`, `chunks_copied`,
  `bytes_copied`, `manifest_objects_copied`, `source_objects_deleted`,
  `local_files_moved`, `source_delete_warnings`, `dry_run`.
- `cartridge.rebound` — rebind mode. Same params, plus
  `chunks_verified`.

Both Ok and Err paths audited (failures carry `result: Error(reason)`).

### Crash semantics

The manifest flip in phase 5 is the single commit point, and that
makes a crash recoverable from either side of it:

- A crash **before** the flip leaves orphan chunks on the target, the
  source intact, and no on-disk manifest change. To recover, either
  re-run migrate — the HEAD-then-copy idempotency makes the second
  pass a no-op for chunks already uploaded — or run `system gc` on the
  target to reclaim the orphans.
- A crash **after** the flip but **before** the source-delete leaves
  orphan chunks on the source, with the manifest correctly pointing at
  the target. To recover, run `system gc` on the source.

### Exit codes

- 0 — success (or dry-run plan generated).
- 1 — migration failed mid-run (chunk verify mismatch, target unreachable, …).
- 2 — refuse-gate triggered or bad params (loaded, WORM/retention mismatch, unknown backend, …).

---

## Cartridge archive — `thurvtl cartridge archive`

Archiving snapshots a cartridge onto a different cloud backend as a
frozen, self-contained blob. The contrast with migrate is that archive
leaves the source cartridge entirely untouched — its manifest,
indexes, local pool, and bound backend all stay intact — so the same
cartridge can have several archives coexisting under distinct labels.

It runs as a daemon-routed admin job (kind `cartridge.archive`),
backed by `core_stream::cartridge_archive::run_archive`.

### CLI surface

```
thurvtl cartridge archive <BARCODE> --target-backend <NAME>
    [--label LABEL]   (defaults to `archive-<ISO-8601-UTC>`)
    [--dry-run]
```

### Object layout on the target backend

```
archives/<barcode>/<label>/manifest.json
archives/<barcode>/<label>/chunks.idx
archives/<barcode>/<label>/blocks-p<N>.idx
archives/<barcode>/<label>/chunks/<aa>/<bb>/<hash>.dat
```

The archive is self-contained: its chunks live under the archive
prefix, not in the target's regular `chunks/` pool, so an archive of
cartridge X cannot collide with a *live* cartridge X bound to the same
backend. The archive's own `manifest.json` is the source manifest plus
two provenance fields — `archived_from_backend`, the source's bound
backend, and `archived_at`, an ISO-8601 UTC timestamp.

### Phases

1. **Validate.** The label must be 1-64 characters, alphanumeric with
   `-` and `_` allowed; the target backend must be named in
   `cloud.backends:`; source and target must differ; and
   `archives/<barcode>/<label>/manifest.json` must not already exist
   on the target.
2. **Walk `chunks.idx`** to collect every sealed chunk's hash.
3. **Copy chunks.** For each hash, prefer the local pool and fall back
   to the source backend's cloud key
   (`chunks/[ns/]<aa>/<bb>/<hash>.dat`). BLAKE3-verify the bytes, then
   `target.upload_chunk` them under the archive prefix.
4. **Snapshot index files.** Read `chunks.idx` and every
   `blocks-p<N>.idx` from disk and upload each as a single binary blob
   under the archive prefix.
5. **Stamp + upload manifest.** Insert the `archived_from_backend` and
   `archived_at` fields, then PUT `manifest.json` last — sentinel-last,
   because it is the manifest's presence that makes the archive
   discoverable at all.

### Refuse-gates

- Daemon running.
- Cartridge not loaded in any drive.
- Target backend named in `cloud.backends:`.
- Source ≠ target.
- Label is non-empty + matches the allowed character set.
- Archive at the same `(barcode, label)` doesn't already exist.
- WORM cartridges require the target's `retention_mode` to be
  governance or compliance.

### Audit

`cartridge.archived` — `barcode`, `from_backend`, `to_backend`,
`label`, `archived_at`, `chunks_total`, `chunks_uploaded`,
`chunks_from_local_pool`, `chunks_from_source_cloud`,
`bytes_uploaded`, `index_files_uploaded`, `dry_run`. Both Ok and Err
paths.

### Crash semantics

Because the archive sentinel (`manifest.json`) is uploaded last, a
crash mid-archive leaves orphan chunks under the archive prefix but no
discoverable sentinel. `system gc` does *not* sweep archive prefixes —
they live outside the regular `manifests/` and `chunks/` keyspaces —
so the recourse is manual: delete the partial
`archives/<barcode>/<label>/` subtree by hand and re-archive under a
fresh label. A same-label retry would be refused anyway by the
duplicate check in phase 1.

### Exit codes

Same as migrate (0 success / 1 mid-run failure / 2 refuse-gate).

---

## Restore-archive — `thurvtl library restore-archive`

This verb is the inverse of archive: it pulls a frozen archive back
into a live cartridge. It runs as a daemon-routed admin job (kind
`library.restore_archive`), backed by
`core_mediachanger::library::restore_archive::run_restore_archive`,
plus a caller-side `Library::add_or_create_tape` to seat the restored
cartridge into a storage slot.

### CLI surface

```
thurvtl library restore-archive
    --backend <NAME> --barcode <BC> --label <LABEL>
    [--as-barcode <NEW>]      (rename + fresh UUID)
    [--allow-existing]
    [--dry-run]
```

### Phases

1. **Validate.** The backend must be named in `cloud.backends:`, and
   the archive sentinel must exist on it — confirmed with a HEAD of
   `manifest.json` under the archive prefix.
2. **Plan destination.** The local cartridge directory is
   `<tapes_dir>/<local_barcode>/`, where `local_barcode` defaults to
   the source barcode unless the operator overrides it with
   `--as-barcode <NEW>`. If that directory already exists, the restore
   refuses — unless `--allow-existing` is set, in which case it simply
   treats the restore as a no-op.
3. **Download + rewrite manifest.** GET
   `archives/<barcode>/<label>/manifest.json` and rewrite it for the
   new local cartridge: `label` becomes the new barcode, `backend`
   becomes the restoring backend, a fresh `uuid` is minted at restore
   time, `index_epoch` and `pending_partition_layout` are cleared, and
   the `archived_from_*` / `archived_at` provenance fields are
   preserved.
4. **Download index files.** Fetch `chunks.idx` and every
   `blocks-p<N>.idx` — enumerated with `list_objects` on the archive
   prefix — into the new cartridge directory.
5. **Download chunks.** Walk the local `chunks.idx`, and for each
   sealed entry, download the chunk from the archive prefix and
   `ChunkPool::insert_verified_bytes` it into the local pool. Each
   `chunks.idx` record is rewritten to `LocationTag::LocalOnly,
   uploaded=false`, so that the daemon's orphan-upload sweep will
   eventually mirror each chunk into the backend's *regular*
   `chunks/[ns/]<aa>/<bb>/<hash>.dat` key — which is where the live
   cartridge will look for it on a cache eviction and cloud refetch.
6. **Seat.** The caller — the daemon handler — briefly acquires the
   library mutex and calls `Library::add_or_create_tape(local_barcode,
   backend_name)` to land the cartridge in a free storage slot. The
   mutex is released before any subsequent `JobEmitter` await.

### Refuse-gates

- Daemon running.
- Backend named in `cloud.backends:`.
- Archive sentinel exists on the backend at the named
  `(barcode, label)`.
- Local cart dir doesn't already exist (unless `--allow-existing`).

### Audit

`library.restore_archive` — `source_barcode`, `local_barcode`,
`backend`, `label`, `chunks_total`, `chunks_downloaded`,
`bytes_downloaded`, `index_files_downloaded`, `seated_in_slot`,
`skipped_existing`, `dry_run`. Both Ok and Err paths;
`skipped_existing=true` does not generate an Ok audit entry (the
operation was a no-op).

### Crash semantics

A crash mid-restore leaves a partial cartridge directory at
`<tapes_dir>/<local_barcode>/`. The next attempt will refuse because
that directory exists, so the recovery is to `rm -rf
<tapes_dir>/<local_barcode>/` first and then re-run. Nothing on the
backend can become inconsistent — the archive prefix is read-only
throughout the operation.

### Exit codes

Same as migrate (0 / 1 / 2).

---

## Configuration File

The full, key-by-key reference lives at `dist/thurvtl.defaults.yaml`,
and you can print the same content with:

```
thurvtl config defaults > dist/thurvtl.defaults.yaml
```

Required: `data_dir`, plus `cloud.backends:` with at least one named
entry.

```yaml
cloud:
  backends:
    primary: { type: s3,  bucket: thurvtl-data,    prefix: "tapes/", region: us-east-1 }
    archive: { type: gcs, bucket: thurvtl-cold,    prefix: "tapes/", project_id: ... }
    devbox:  { type: local, root_dir: "./.thurvtl/local-backend" }
```

Every entry is exactly one of `s3`, `gcs`, `azure`, or `local`,
discriminated by its `type` field, and the per-cloud knobs (for
example `endpoint_url` or `service_account_key_file`) sit inline in
the entry. Per-backend credentials — the `auth:` blocks, the `_env`
indirections, and the strict-override semantics — are documented
separately in [`AUTH.md`](AUTH.md).

A cartridge picks which entry it binds to with `--backend NAME` at
create time. That flag is optional and inferred when only one backend
is configured, and required once there are two or more; any number of
backends may be configured.

The library topology is **not** part of `thurvtl.yaml`. It is
initialized once with `thurvtl library init` and lives at
`<data_dir>/library/library.json`.

---

## Telemetry

There is a single instrumentation surface, built on the OpenTelemetry
SDK, with two readers attached to one shared `MeterProvider`:

- **Prometheus pull** — always wired, served at `GET /metrics` on the
  daemon's HTTP listener (same address as `/health` / `/sessions` /
  `/info`). `Content-Type: text/plain; version=0.0.4`. Renders via
  `opentelemetry-prometheus`.
- **OTLP push** — opt-in via `telemetry.otlp.enabled`. A periodic
  reader pushes the same instruments over OTLP to a Collector or any
  OTLP-compatible backend. Both `grpc-tonic` (default port 4317) and
  `http/protobuf` (default port 4318) transports are wired.

Both readers walk the same in-memory state, so a counter incremented
once is visible on both surfaces with no risk of double counting.

### `telemetry.otlp.*` block

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` (when `otlp` block present) | Set false to keep the block but disable export. |
| `endpoint` | string | `http://localhost:4317` | Collector / SaaS endpoint. |
| `protocol` | string | `grpc` | `grpc` / `http` (alias `http_protobuf`). |
| `interval_seconds` | u64 | `30` | Push cadence. Min 1. |
| `headers` | map<string,string> | `{}` | Per-request headers (e.g. SaaS auth). |

### Resource attributes

Every emitted metric carries OTel resource attributes:
`service.name=thurvtl`, `service.version=<crate version>`,
`service.instance.id=<hostname>` (when `/etc/hostname` is readable).

### Metric inventory

Instrument names follow the shape `<prefix>_<subsystem>_<name>`. The
prefix is per-product — `thurvtl` for this product, `thurvsa` for the
block target — and both come from `shared_naming::PRODUCT.metric_prefix`.
The table below gives only the suffix, so read every row as
`thurvtl_<row>` for this daemon.

One subtlety: where an instrument declares a unit (`s` or `By`), the
Prometheus exporter appends the conventional suffix (`_seconds` or
`_bytes`) on the way out. The instrument names themselves therefore do
**not** include the unit — baking it in would produce it twice.

| Subsystem | Instrument | Type | Unit | Attributes |
| --- | --- | --- | --- | --- |
| pool | `pool_used_bytes` | Gauge<u64> | — | `backend` |
| pool | `pool_cap_bytes` | Gauge<u64> | — | `backend` |
| pool | `pool_evictions_total` | Counter<u64> | — | `backend` |
| pool | `pool_backpressure_waits_total` | Counter<u64> | — | `backend` |
| pool | `pool_backpressure_wait` | Histogram<f64> | s | `backend` |
| cache | `cache_evictions_total` | Counter<u64> | — | `volume`, `outcome` (VSA only) |
| cloud | `cloud_requests_total` | Counter<u64> | — | `backend`, `op`, `outcome` |
| cloud | `cloud_request` | Histogram<f64> | s | `backend`, `op`, `outcome` |
| cloud | `cloud_transferred` | Counter<u64> | By | `backend`, `op`, `outcome` |
| cloud | `cloud_retries_total` | Counter<u64> | — | `backend`, `class` |
| cloud | `cloud_permanent_errors_total` | Counter<u64> | — | `backend`, `class` |
| chunk | `chunk_seals_total` | Counter<u64> | — | `backend`, `scope` |
| chunk | `chunk_dedup_hits_total` | Counter<u64> | — | `backend`, `scope` |
| chunk | `chunk_logical` | Counter<u64> | By | `backend`, `scope` |
| chunk | `chunk_unique` | Counter<u64> | By | `backend`, `scope` |
| chunk | `chunk_uploaded` | Counter<u64> | By | `backend` |
| chunk | `chunk_cloud_head_probes_total` | Counter<u64> | — | `backend` |
| chunk | `chunk_cloud_head_hits_total` | Counter<u64> | — | `backend` |
| chunk | `chunk_cloud_cache_hits_total` | Counter<u64> | — | `backend` |
| chunk | `chunk_cloud_cache_inflight_coalesced_total` | Counter<u64> | — | `backend` |
| chunk | `chunk_cloud_cache_warmup_seeded_total` | Counter<u64> | — | `backend` |
| iscsi | `iscsi_sessions_active` | Gauge<i64> | — | — |
| iscsi | `iscsi_commands_total` | Counter<u64> | — | `opcode`, `outcome` |
| iscsi | `iscsi_command` | Histogram<f64> | s | `opcode`, `outcome` |
| iscsi | `iscsi_data_in` | Counter<u64> | By | — |
| iscsi | `iscsi_data_out` | Counter<u64> | By | — |
| tape | `tape_write_buffer_used` | Gauge<u64> | By | `cartridge` |
| tape | `tape_read_buffer_used` | Gauge<u64> | By | `cartridge` |
| prefetch | `prefetch_queue_depth` | Gauge<i64> | — | — |
| prefetch | `prefetch_hits_total` | Counter<u64> | — | — |
| prefetch | `prefetch_misses_total` | Counter<u64> | — | — |
| audit | `audit_entries_total` | Counter<u64> | — | `kind` |
| audit | `audit_chain_resets_total` | Counter<u64> | — | — |
| audit | `audit_queue_drops_total` | Counter<u64> | — | — |
| alerting | `alerts_fired_total` | Counter<u64> | — | `class`, `severity`, `sink`, `outcome` |
| recovery | `orphan_scan_chunks_found_total` | Counter<u64> | — | — |
| recovery | `orphan_scan_duration` | Histogram<f64> | s | — |
| fetb | `fetb_latest_bytes` | Gauge<u64> | By | — |
| fetb | `fetb_sample_count` | Gauge<u64> | — | — |
| daemon | `daemon_start_time` | Gauge<i64> | s | — |

### Process-global handle

The core call sites — cartridge, audit, cloud, iSCSI, pool budget —
record through the `shared_telemetry::record::*` free functions, also
re-exported as `core_mediachanger::metrics::record::*`. Those functions
locate a process-global `Telemetry` that the daemon installs at boot
via `shared_telemetry::set_global`. A CLI invocation or a unit test
never installs that global, so when those callers run through the same
core paths their samples simply no-op.

### Discontinued upstream crate

`opentelemetry-prometheus = 0.31` is flagged "discontinued" upstream —
the OTel project recommends routing Prometheus output through the
Collector instead. For a self-hosted appliance the in-process bridge
is still the right shape, because it is one fewer moving part to run.
If the crate ever bit-rots against a future `opentelemetry` release,
the escape hatch is to replace it with a custom registry-walker of
roughly 200 LoC.

---

## HTTP Endpoints

### `/health` JSON

```json
{
  "status": "ok",
  "daemon": "thurvtl",
  "version": "0.1.0"
}
```

`/health` is a minimal liveness probe. The body shape is identical on
both daemons; only the `daemon` field differs (`daemon: "thurvsa"` on
the VSA side).

### `/info` JSON

VTL (`thurvtld`):

```json
{
  "slots": { "storage": 40, "mail": 5 },
  "drives": 3,
  "lto_generation": 8,
  "partitions": ["default"],
  "chassis_serial": "TVL3F8A2C7B1E0D"
}
```

VSA (`thurvsad`):

```json
{
  "volume_count": 42,
  "iqn": "iqn.2025-10.com.metebalci:thurvsa",
  "listen_address": "0.0.0.0:3260"
}
```

`/info` is a read-only summary — chassis topology on VTL, volume count
plus iSCSI coordinates on VSA. Anything finer-grained, the per-element
or per-volume detail, stays behind the peer-cred-authed admin socket
rather than this open HTTP endpoint.

---

## FETB Telemetry

The daemon samples front-end TiB — FETB, the count of bytes the host
writes into the VTL measured before dedup and before compression —
purely as an operational telemetry signal. There is no cap, no gate,
and no enforcement built on the figure.

### FETB sampler

The sampler runs every 6 hours; the interval is hardcoded. Each sample
sums `runtime.json::host_bytes_written` across every cartridge under
`<data_dir>/tapes/` — that is one `fs::read` per `runtime.json`
sidecar, with no `chunks.idx` opened. The sum is emitted as a single
`fetb.sample` audit event carrying `{ts, fetb_bytes,
cartridge_count}`. The audit log is the *only* place this is
persisted; there is no separate JSON cache. The sampler itself lives
in `shared_audit::fetb` (`take_sample`, `count_samples_in_window`,
`record_fetb_sample`, `run_fetb_sampler`).

The telemetry meter reads back the trailing 4-week (28-day) window of
`fetb.sample` audit entries. That is what sets the audit retention
floor: `audit.retention_days` must be `>= 40` (the default is 90), and
below 40 the daemon refuses to start, because the meter needs at least
4 weeks of history plus some margin.

### Telemetry metrics

| Metric | Description |
|---|---|
| `thurvtl_fetb_latest_bytes` | Latest raw FETB sample (bytes). |
| `thurvtl_fetb_sample_count` | Samples in the trailing 4-week window. |

### Audit events

| Event | When | Body |
|---|---|---|
| `fetb.sample` | Every sampler tick (every 6 h) and once at startup if the audit log carried no FETB history (bootstrap). | `ts: RFC3339`, `fetb_bytes: u64`, `cartridge_count: u64`, optional `reason: "startup-bootstrap"`. |
| `cloud.orphan_scan_started` | Boot-time scan begins walking `<data_dir>/tapes/` for sealed-but-not-uploaded chunks. One entry per daemon start. | `cartridges_scanned: u64`. |
| `cloud.orphan_scan_completed` | Same scan ends. `orphans_requeued < orphans_found` indicates one or more upload-worker dispatches failed (channel closed). | `orphans_found: u64`, `orphans_requeued: u64`, `duration_seconds: f64`. |

---

## References

- **SCSI Architecture Model (SAM-5)** — [T10 draft r21](https://www.t10.org/cgi-bin/ac.pl?t=f&f=sam5r21.pdf)
- **SCSI Primary Commands (SPC-4)** — [T10 draft r37](https://www.t10.org/cgi-bin/ac.pl?t=f&f=spc4r37.pdf) *(conformance target; SPC-5 referenced where field layout is identical)*
- **SCSI Sequential-Access Commands (SSC-4)** — [T10 draft r03](https://www.t10.org/cgi-bin/ac.pl?t=f&f=ssc4r03.pdf) *(conformance target; SSC-5 referenced where field layout is identical)*
- **SCSI Media Changer Commands (SMC-3)** — [T10 draft r16](https://www.t10.org/cgi-bin/ac.pl?t=f&f=smc3r16.pdf)
- **iSCSI Protocol** — [RFC 7143](https://datatracker.ietf.org/doc/html/rfc7143) (obsoletes [RFC 3720](https://datatracker.ietf.org/doc/html/rfc3720))
- **iSCSI SCSI Features Update** — [RFC 7144](https://datatracker.ietf.org/doc/html/rfc7144)
- **iSCSI Naming and Discovery** — [RFC 3721](https://datatracker.ietf.org/doc/html/rfc3721)
- **CHAP** — [RFC 1994](https://datatracker.ietf.org/doc/html/rfc1994) (bound to iSCSI per [RFC 7143 §11](https://datatracker.ietf.org/doc/html/rfc7143#section-11))
- **LTO Ultrium** — LTO-7 / 8 specifications (LTO Program)
- **FastCDC** — Xia et al., *FastCDC: a Fast and Efficient Content-Defined Chunking Approach for Data Deduplication*, USENIX ATC 2016
- **BLAKE3** — https://github.com/BLAKE3-team/BLAKE3
- **AWS S3 API** — https://docs.aws.amazon.com/s3/
- **Google Cloud Storage API** — https://cloud.google.com/storage/docs
- **Azure Blob Storage REST API** — https://learn.microsoft.com/rest/api/storageservices/blob-service-rest-api
