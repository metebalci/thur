# Thur VSA Operations

Day-to-day management of block volumes: create, resize, snapshot, clone,
restore, encrypt, destroy. All `volume` commands are **daemon-routed** —
they talk to `/run/thurvsa/admin.sock` and refuse with a clear message
when the daemon is down. `thurvsa volume <cmd> --help` is the source of
truth for flags; this page is the operator workflow.

Connecting hosts and rescanning for new LUNs is in
[`CONNECTING.md`](CONNECTING.md); at-rest encryption setup in
[`ENCRYPTION.md`](ENCRYPTION.md); the block SCSI surface in
[`../reference/CONFORMANCE_SCSI.md`](../reference/CONFORMANCE_SCSI.md) § Part 3.

## Create

```bash
thurvsa volume create myvol --size 100G          # thin-provisioned
thurvsa volume list
thurvsa volume info myvol
```

Volumes are thin-provisioned: `--size` is the *addressable* capacity;
only written data consumes backend storage (after dedup + compression).
`--backend` is resolved daemon-side — omit it when exactly one
`storage.backends:` entry exists. 4 KiB sectors, default 64 KiB page.

After `create`, rescan the host (`iscsiadm -m session --rescan` /
`nvme ns-rescan`) to see the new LUN; under CHAP / TLS-PSK also grant
admission first (see [Admission](#admission-chap--tls-psk)).

## Resize (online)

`volume resize NAME --size N` changes logical capacity live: the daemon
flips the size, persists `manifest.json`, and signals connected hosts to
re-read capacity (NVMe Namespace Attribute Changed AER / iSCSI CAPACITY
DATA HAS CHANGED unit attention). A host-side rescan may still be needed
for the OS to pick it up.

- **Grow** is metadata-only over the sparse page table — always safe.
- **Shrink** is non-destructive by construction: it refuses on a WORM
  volume, while a persistent reservation is held, or when allocated data
  sits past the new end. Free that range from the host first (shrink the
  filesystem, then `fstrim` / `blkdiscard` to UNMAP the freed pages).
- `--shrink-to-fit` snaps to the smallest size that keeps all allocated
  data, so you needn't compute the exact byte count (resize the
  filesystem down first so it fits). `--size` and `--shrink-to-fit` are
  mutually exclusive; exactly one is required.

## Snapshots

A snapshot is a frozen point-in-time copy of a volume's page table,
sharing chunks copy-on-write. It is **not** host-visible — to read its
data, clone it.

```bash
thurvsa volume snapshot create myvol snap1     # instant; briefly pauses host I/O
thurvsa volume snapshot list myvol
thurvsa volume snapshot destroy myvol snap1
```

`snapshot create` flushes the volume's cache and freezes its page table —
near-instant, with a brief host-I/O pause during the index copy.

## Clones

A clone is a **new writable volume** seeded from a snapshot (or a live
volume), sharing chunks until it diverges on write:

```bash
thurvsa volume clone myvol myvol-copy --from-snapshot snap1 [--lun N]
thurvsa volume clone myvol myvol-live                       # from live contents
```

The clone is a new LUN: it needs a host rescan and, under CHAP / TLS-PSK,
its own admission grant — it does **not** inherit the source's grants.
Cloning an encrypted volume is supported: the clone inherits the source's
crypto identity and shares its DEK (refcounted, so destroying the source
while a clone exists keeps the clone readable).

## Restore in place

`volume snapshot restore` rolls an **existing** volume back to a snapshot,
discarding every write since. The volume keeps its identity (UUID, LUN,
name, DEK); only the page table is rewound.

```bash
thurvsa volume snapshot restore myvol snap1 --force
thurvsa volume snapshot restore myvol snap1 --force --resize   # also roll size back
```

- It is destructive, so `--force` is required.
- **Quiesce the host first.** There is no active-session check — the
  target cannot tell whether a host has the LUN mounted, and restoring
  under a live, mounted filesystem corrupts it. Unmount / stop the
  workload before restoring.
- Refuses while a persistent reservation is held.
- By default page-table-only; refuses if the volume was resized since the
  snapshot. `--resize` rolls the logical size back too (refused on a WORM
  volume — its size is grow-only).
- Diverged chunks become orphans the next `system gc` reclaims.
- Not crash-atomic, but safe: if the daemon dies mid-restore the snapshot
  is untouched — just re-run.

## Encryption

Encrypt at rest with `--encrypt` + `--keystore` at create time. The full
keystore matrix (local sidecar, AWS KMS, Vault, Azure Key Vault, GCP KMS,
KMIP) and the daemon-down `volume key migrate` workflow are in
[`ENCRYPTION.md`](ENCRYPTION.md) § VSA keystore backends.

## Destroy

```bash
thurvsa volume destroy myvol
```

Destroy is metadata-first: the volume's chunks become orphans that the
next `system gc` reclaims (shared chunks referenced elsewhere are kept).

## Admission (CHAP / TLS-PSK)

When `iscsi.auth.method: CHAP` or `nvmetcp.tls.mode` is on, volume access
is **mandatory per-identity admission** — a CHAP user / TLS-PSK host sees
only the volumes granted to it:

```bash
thurvsa iscsi users grant USER --volume myvol
thurvsa nvmetcp psks grant --host-nqn NQN --volume myvol
```

With auth off, sessions see every volume. Details in
[`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).

## Health and accounting

```bash
thurvsa system stats              # dedup + capacity analytics
thurvsa volume info myvol         # per-volume allocated/logical
thurvsa system verify             # chunk-pool + page-table integrity
thurvsa system gc                 # reclaim orphaned chunks
```
