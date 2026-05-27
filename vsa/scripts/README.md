# VSA test scripts

Each script self-elevates via `sudo` (NOPASSWD assumed) where it
needs root, exits 0 on pass / 1 on fail, and auto-cleans unless
`--keep-data` is passed. Header docblocks are the source of truth
for prereqs and flags; this index is the bird's-eye view.

Naming follows `test-<category>-<topic>.sh`:

| Category | Purpose |
|---|---|
| `smoke` | Lightweight entry gate, no real data path |
| `proto-<transport>` | RFC-level transport conformance |
| `scsi-*`, `iscsi-multi-pdu-*` | SCSI command set + iSCSI wire-format |
| `fs-<transport>[-storage]` | ext4 over a block device; `-storage` adds real S3/GCS/Azure |
| `app-<name>` | Real applications driving the storage end-to-end |
| `crash-*` | Power-loss / SIGKILL durability invariants |
| `keystore[-<backend>]` | Keystore backend coverage |
| *flat* | Single-script categories (`monte-carlo`, `pipeline-layers`, `multi-initiator`, `multi-volume-dedup`) |

## Scripts

| Script | What it proves |
|---|---|
| `test-smoke.sh` | HTTP health, admin socket, iSCSI INQUIRY — no kernel initiator, no data path. |
| `test-proto-iscsi.sh` | iSCSI protocol-layer conformance (login, CmdSN/StatSN, digests) via libiscsi. |
| `test-proto-nvmetcp.sh` | NVMe/TCP host round-trip (handshake, identify, properties, I/O, disconnect). |
| `test-scsi-conformance.sh` | Per-CDB SBC-3 compliance (INQUIRY, READ/WRITE, CAW, UNMAP, persistent reservations). |
| `test-iscsi-multi-pdu-readin.sh` | iSCSI Data-In chunking when a single READ-16 response exceeds MaxRecvDataSegmentLength. |
| `test-fs-iscsi.sh` | ext4 durability + daemon-restart persistence over iSCSI. |
| `test-fs-nvmetcp.sh` | NVMe/TCP variant of `test-fs-iscsi`. |
| `test-fs-iscsi-storage.sh` | `test-fs-iscsi` + real S3/GCS/Azure backend — upload pipeline, dedup, refetch. |
| `test-fs-nvmetcp-storage.sh` | NVMe/TCP variant of `test-fs-iscsi-storage`. |
| `test-fs-storage-failures.sh` | Backend failure injection (auth, timeout, throttling) via the daemon's test-mode hooks. |
| `test-app-postgres.sh` | PostgreSQL OLTP + TPC-B invariant survives SIGKILL + WAL replay. |
| `test-app-vm.sh` | Ubuntu 26.04 guest boots from a VSA volume; cloud-init fixture survives clean shutdown and crash-replay. |
| `test-crash-audit-append.sh` | BLAKE3-chained audit log stays valid under SIGKILL mid-append. |
| `test-crash-page-flush.sh` | Fsynced data survives SIGKILL after the fence; un-fsynced data may not. |
| `test-multi-initiator.sh` | iSCSI persistent reservations enforce exclusivity across two initiator IQNs. |
| `test-multi-volume-dedup.sh` | 20-volume fleet exercises shared-pool dedup stats + chunk-pool bookkeeping. |
| `test-monte-carlo.sh` | Seeded random filesystem ops with boundary-biased sizes; transport-agnostic (`--transport iscsi\|nvmetcp`). |
| `test-pipeline-layers.sh` | Matrix of `{dedup, encrypt, storage-zstd}` combinations — five-row layer comparison. |
| `test-keystore.sh` | DEK wrap/unwrap/migrate against each `KeyStoreBackend` (local, awskms, vault, azurekv, gcpkms). |
| `test-keystore-kmip.sh` | KMIP backend integration against a locally-spun PyKMIP server. |

Run from the repo root. Remote-backend variants (`*-storage*`, the
keystore matrix) require `THURVSA_TEST_BACKEND` /
`THURVSA_TEST_KEYSTORE` matching a non-`local` entry in the conffile.
