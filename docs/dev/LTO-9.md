# LTO-9

The VTL ships emulating **LTO-8 only**, with the LTO-7-RO descriptor
retained in REPORT DENSITY SUPPORT for real-LTO-8-drive parity. This
document covers what wiring LTO-9 support would require at the SCSI
level, why the technical case for doing so is weaker for a VTL than for
physical hardware, and why the work is deferred past 1.0.0 GA.

## What LTO-9 actually changes at the SCSI level

Relative to LTO-8, LTO-9 introduces:

1. **New density code** — `0x60` (LTO-9) vs `0x5E` (LTO-8). Advertised
   in INQUIRY VPD, REPORT DENSITY SUPPORT (SSC-4 §7.13), mode-page block
   descriptors, and MAM attributes.
2. **New capacity** — 18 TB native (vs LTO-8 at 12 TB).
3. **Compatibility matrix shift** — LTO-9 drives **read** LTO-8 only
   (LTO-7 read dropped); **write** LTO-9 only.
4. **RAO** (Recommended Access Ordering) — MAINTENANCE IN/OUT opcodes
   `0x9E` / `0x9F`. Backup software hands the drive a list of file marks
   to read; the drive returns a seek-optimized ordering. New in LTO-9.
5. **Initial Capacity Scaling (ICS)** — fresh LTO-9 cartridges start
   reporting **400 GB** native capacity and expand to 18 TB after first
   format. A physical-media lifetime quirk; backup software must accept
   the small initial capacity, run an explicit re-format, or
   special-case "shrunk" tapes.
6. **SSC-5 features** — LTO-9 falls under SSC-5 rather than SSC-4.
   Differences are minor (some encryption-mode bits, a couple of new log
   pages) but the conformance target would shift.

Everything else — INQUIRY layout, AES-256-GCM tape encryption (SECURITY
PROTOCOL IN/OUT `0xA1` / `0xA2`), MAM attribute schema, filemark / block
read-write protocol, persistent reservations, partition support — is
**identical** between LTO-8 and LTO-9 at the SCSI surface.

## Why this matters less for a VTL than for physical hardware

1. **The cartridge is virtual.** "Native capacity" is a number the
   daemon advertises, not a magnetic-substrate property. The difference
   between LTO-8/12 TB and LTO-9/18 TB changes the backup software's
   cartridge-planning math, but the data sits in the same dedup-capable
   chunk pool either way.
2. **ICS is meaningless.** Initial Capacity Scaling exists to extend
   substrate wear life — a VTL has no substrate. The implementation
   choice is either to pretend ICS does not apply and advertise 18 TB
   from day one, or to fake the initial 400 GB and accept a re-format
   flow. Either way it is window dressing.
3. **Backward-read compatibility is irrelevant.** LTO-9's narrower read
   compatibility matrix only matters when legacy physical cartridges
   share the same library. A VTL's cartridges all come from the same
   chunk pool, so the question never arises.
4. **RAO buys very little.** Real drives need seek optimization because
   the head physically traverses media. A VTL's equivalent of a seek is
   a backend-chunk fetch, and RAO's reordering does not optimize that. A
   passthrough implementation that returns the initiator's own order
   would go unnoticed by any real workload.
5. **Most enterprise backup products don't care.** NetBackup, Veeam,
   Commvault, Bareos, and Spectrum Protect certify against SSC-4,
   standard density codes, and standard mode pages. They gate on
   drive-model certification, not on whether that model reports LTO-8
   vs LTO-9.

Where LTO-9 does matter, even for a VTL:

- **Operator UX** — some backup-product UIs list LTO-9 explicitly;
  picking "LTO-8" might look outdated. Pure perception, no technical
  impact.
- **Compliance / procurement** — rare cases where a procurement spec
  says "must report LTO-9 density."
- **RAO-aware HPC / film-archive workflows** — a small set of
  restore-heavy pipelines use RAO; a passthrough would degrade their
  restore performance but not break anything.

## What it would take to wire up LTO-9

### Mechanical lookup-table additions (~40 LOC, half a day)

| Location | Current | Add |
|---|---|---|
| `core/mediachanger/src/library/mod.rs::default_firmware_for_lto` | `match { 7=>"TVL7", 8=>"TVL8", _=>"TVL0" }` | `9 => "TVL9"` (or whatever per-gen string convention) |
| `core/stream/src/cartridge/mod.rs::lto_default_capacity_gb` | `7=>6000, 8=>12000, _=>0` | `9 => 18000` |
| `scsi/ssc/src/dispatch/handlers.rs` REPORT DENSITY SUPPORT | Hard-codes `0x5E` (LTO-8) + `0x5C` (LTO-7) descriptor pair, capacity-MB match on those two codes | Add `0x60` (LTO-9) descriptor, capacity 18 000 000 MB; advertise `0x60` (RW, primary) + `0x5E` (RO, secondary) per LTO-9 compat matrix |
| `core/mediachanger/src/library/mod.rs` validator + `vtl/cli/src/cli.rs` clap range | validator `!= 8` rejects; clap `range(8..=8)` | `8..=9` (validator, CLI clap range) |

These changes are isolated, mechanical, and covered by the existing test
suites. Extending the match arms is the full change.

### Real feature work (where the actual cost lives)

1. **RAO opcodes (`0x9E` / `0x9F`)** — new dispatch handlers in
   `scsi/ssc/src/dispatch/`. The simplest option is a passthrough that
   returns the initiator's input order verbatim. An honest alternative is
   a no-op stub returning CHECK CONDITION with ILLEGAL REQUEST / INVALID
   COMMAND. Either is small (~50 LOC), but the implementation should be
   tested against a backup product that actually issues RAO before being
   declared done.
2. **Initial Capacity Scaling** — touches the capacity model
   end-to-end: manifest schema (initial vs current capacity), MODE SENSE
   capacity reporting, READ/WRITE ATTRIBUTE (MAM current vs maximum
   capacity), backup-software re-format flow. ~200-400 LOC and a
   manifest-schema bump. The honest alternative is to always advertise
   the full 18 TB and document the deviation in `CONFORMANCE_SCSI.md` —
   a one-line policy statement.
3. **SSC-5 conformance target** — the SSC-4 deltas are small enough that
   this is mostly a `CONFORMANCE_SCSI.md` update, not code work. Do it
   alongside (1) and (2).
4. **Cartridge convert (LTO-8 → LTO-9, LTO-9 → LTO-10)** — when a newer
   generation lands, operators with existing LTO-8 cartridges need a
   relabel path without re-uploading every chunk. Real LTO hardware
   cannot do this because the substrate is fixed; a VTL can, since
   `lto_generation` is just a manifest field. The proposed verb is
   `thurvtl cartridge convert <barcode> --to-generation 9`, which
   rewrites the manifest's `lto_generation` and `capacity_gb` to the new
   native size atomically. The verb would refuse on WORM cartridges,
   loaded cartridges, and cartridges under legal hold, and would emit
   an audit `cartridge.converted` row with from/to generation. This is
   not a 1.0.0 feature — the convert verb only makes sense once LTO-9
   cartridge creation is supported.

## Recommendation: not in 1.0.0

VTL 1.0.0 ships LTO-8 only, with the LTO-7-RO secondary descriptor in
REPORT DENSITY SUPPORT for real-drive backwards-read parity and no LTO-7
cartridge creation. The reasoning:

- **The technical floor is already shipped.** SSC-4 + LTO-8 is the
  intersection where every mainstream backup product certifies and every
  audit framework knows how to validate.
- **Half-implementing LTO-9 is worse than not implementing it.**
  Advertising LTO-9 in INQUIRY but returning CHECK CONDITION on RAO
  triggers edge cases in backup-software error-handling paths that are
  difficult to predict and test.
- **The deferred work is small and well-scoped.** The table above is the
  full change list; nothing in the current design forecloses LTO-9, so
  it can ship in a point release without touching anything structural.
- **Deferring focuses 1.0.0 testing on what matters** — storage-backend
  reliability, dedup correctness, encryption, and the iSCSI/SCSI
  conformance surface for LTO-8.

## Managing LTO-8 stability when LTO-9 lands

LTO-9 work will land in the same files that carry LTO-8 logic today:
`core/stream/src/cartridge/mod.rs`,
`scsi/ssc/src/dispatch/handlers.rs`,
`core/mediachanger/src/library/mod.rs`, and the SCSI dispatcher. An
operator who is happy with LTO-8 and upgrades to an LTO-9-capable release
will be running new code under their existing LTO-8 chassis. Two options
were considered:

**(1) Code-level isolation — declined.** This approach would use feature
flags or separate LTO-8 and LTO-9 module trees with dispatch via an
`LtoSpec` trait. The problem is that tape emulation is monolithic at the
SCSI level — the dispatcher, mode pages, log pages, density descriptors,
and capacity reporting all share code paths. Forking by generation
duplicates approximately 60% of `scsi/ssc/` and `core/stream/`, producing
two copies to maintain in parallel. Most of the LTO-8 vs LTO-9 changes
are additive (new match arms, new opcodes), so there is duplication cost
without isolation benefit.

**(2) Release-stream isolation — required.** The semver-major boundary is
the right mechanism: 1.x = LTO-8, 2.x = LTO-8 + LTO-9. When LTO-9 work
begins, a `release/1.x` maintenance branch is cut from the last 1.x tag
and LTO-9 development continues on `main` toward 2.0.0. Operators who
want LTO-8 stability stay on 1.x and receive bug fixes and security
patches backported from `main`. This is the standard model used by OS
distributions, language toolchains, and stable infrastructure projects
for major-feature additions.

**Plan of record:**

- When LTO-9 work starts — first action is `git switch -c release/1.x`
  from the last 1.x tag. That branch becomes the LTO-8-stable channel;
  main becomes the 2.x development line.
- Keep changes additive where possible. The one anticipated non-additive
  refactor is REPORT DENSITY SUPPORT becoming gen-aware (today hardcoded
  to LTO-8 + LTO-7 RO regardless of chassis generation). Structure that
  refactor so the LTO-8 chassis emits the same descriptor bytes it does
  on 1.x.

What we explicitly *don't* do: feature flags for LTO-8 vs LTO-9 on the
same release.

## Trigger conditions for revisiting

LTO-9 should be reopened when any of the following holds:

- Someone asks for it with a concrete reason (a procurement spec, an
  audit framework, or a certification matrix requirement).
- A mainstream backup product starts gating on LTO-9 in its
  hardware-compatibility matrix (none do today).
- A restore-heavy workflow (HPC, film archive) wants RAO and is prepared
  to validate the implementation against their software.

Until then, the cap stays at LTO-8 and the validator rejects any
`lto_generation != 8`. This document keeps the reasoning on record and
the implementation steps pre-scoped so that whenever the work does start,
no discovery phase is needed.
