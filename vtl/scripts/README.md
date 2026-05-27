# VTL test scripts

Each script self-elevates via `sudo` (NOPASSWD assumed) where it
needs root, exits 0 on pass / 1 on fail, and auto-cleans unless
`--keep-data` is passed. Header docblocks are the source of truth
for prereqs and flags; this index is the bird's-eye view.

Naming follows `test-<category>-<topic>.sh`:

| Category | Purpose |
|---|---|
| `smoke` | Lightweight entry gate, no SCSI data path |
| `proto-<transport>` | RFC-level transport conformance |
| `scsi-conformance` | Per-CDB SMC + SSC conformance |
| `backup-*` | Tape backup workflows (with/without real storage / failures / resume) |
| `app-<name>` | Real applications driving the tape product end-to-end |
| `lifecycle-*` | CLI / metadata lifecycle ops (cartridge migrate, DR restore, soak) |
| `crash-*` | Power-loss / SIGKILL durability invariants |
| *flat* | Single-script categories (`monte-carlo`, `pipeline-layers`) |

## Scripts

| Script | What it proves |
|---|---|
| `test-smoke.sh` | HTTP health, CLI, iSCSI discovery — no SCSI data path, no tape writes. |
| `test-proto-iscsi.sh` | iSCSI protocol-layer conformance (login, CmdSN/StatSN, digests) via libiscsi. |
| `test-scsi-conformance.sh` | Per-CDB SSC + SMC conformance (INQUIRY, MOVE MEDIUM, READ/WRITE, REWIND, SPACE, encryption). |
| `test-backup-workflow.sh` | Full backup workflow over `/dev/nstN` (tar format) + multi-cartridge swap + byte-for-byte verify. |
| `test-backup-storage.sh` | `test-backup-workflow` + real S3/GCS/Azure backend — upload pipeline, dedup, refetch. |
| `test-backup-storage-failures.sh` | Backend failure injection (auth, timeout, throttling) on the backup upload path. |
| `test-backup-storage-resume.sh` | Boot-time orphan-chunk scan + recovery after SIGKILL mid-PUT. |
| `test-app-bareos.sh` | Real Bareos director driving 2-drive chassis + autochanger; concurrent jobs + restore-and-diff. |
| `test-lifecycle-cartridge-migrate.sh` | CLI surface for `cartridge migrate / archive / restore-archive` (preconditions, dry-run, on-disk state). |
| `test-lifecycle-many-cartridges.sh` | Metadata lifecycle stress: create/destroy ~30 cartridges; inventory + audit chain stay valid. |
| `test-lifecycle-dr-restore.sh` | Daemon-down `library restore` CLI surface (dry-run, empty-bucket discovery, audit replay). |
| `test-crash-audit-append.sh` | BLAKE3-chained audit log stays valid under SIGKILL mid-append. |
| `test-crash-chunk-seal.sh` | Tape blocks fsynced via filemark survive SIGKILL; un-fsynced may not. |
| `test-monte-carlo.sh` | Seeded random tape ops with boundary-biased record sizes and load-cycle churn. |
| `test-pipeline-layers.sh` | Matrix of `{dedup, DCE, encrypt, storage-zstd}` combinations — five-row layer comparison. |

Run from the repo root. Remote-backend variants (`*-storage*`,
`backup-storage*`) require `THURVTL_TEST_BACKEND` matching a
non-`local` entry in the conffile.
