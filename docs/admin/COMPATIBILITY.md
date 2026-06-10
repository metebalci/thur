# Compatibility

What Thur has been tested against, and why standard initiators and
applications work even when they aren't in the test matrix.

## How compatibility works

Thur presents **generic, spec-conformant** devices, not impersonations of
a specific chassis. Backup software and operating systems identify a
device by its *class* — they read the INQUIRY peripheral-device-type and
probe supported opcodes — not by a vendor/model string. Thur VTL answers
as an SMC-3 medium changer (LUN 0) plus SSC-4 LTO-8 drives (LUN ≥ 1);
Thur VSA answers as SBC-3 direct-access LUNs. Any initiator or application
that drives those standard classes works, regardless of whether it is
listed below.

INQUIRY identity is deliberately generic: vendor `MB`, product
`THUR VTL` (changer) / `Ultrium 8-SCSI` (drive). The deliberate
divergences from physical LTO hardware are catalogued in
[`../reference/CONFORMANCE_SCSI.md`](../reference/CONFORMANCE_SCSI.md)
§ Behavioral model & deliberate divergences — read it before assuming a
hardware-specific feature is present.

## Tested in CI

These run end-to-end against a live daemon on every change (`*/scripts/`):

| Workload | Application | Transport | What it exercises |
|---|---|---|---|
| **Bareos 21** (director / SD / FD, SQLite catalog) | VTL | iSCSI | Real backup + restore across a 2-drive / 6-cartridge chassis, byte-for-byte restore diff. |
| **PostgreSQL** on ext4 | VSA | iSCSI / NVMe/TCP | WAL fsync ordering, OLTP, SIGKILL crash + journal replay + TPC-B invariant. |
| **Ubuntu 26.04 LTS** (full VM, UEFI/OVMF) | VSA | iSCSI / NVMe/TCP | Boots a real OS from a volume; hard-reset mid-write + ext4 journal replay + fsck. |
| **sg3_utils / libiscsi** conformance | both | iSCSI | SPC-4 / SBC-3 / SSC-4 / SMC-3 opcode and protocol conformance. |
| **nvme-cli** protocol | VSA | NVMe/TCP | NVM command set + NVMe/TCP transport conformance. |

The full suite catalogue is in
[`../dev/TESTCOVERAGE.md`](../dev/TESTCOVERAGE.md).

## Initiators expected to work

Any standards-compliant initiator. The ones in regular use:

- **Linux** — `open-iscsi` (iSCSI), `nvme-cli` / native NVMe/TCP. The
  `iscsiadm` cheatsheet is [`ISCSIADM.md`](ISCSIADM.md).
- **Windows** — the built-in iSCSI Initiator (Control Panel). VTL devices
  appear under Tape drives + Medium Changers; VSA under Disk Management.
- **VMware ESXi** — software iSCSI adapter (VSA block LUNs).
- **Hyper-V / Windows Server** — iSCSI Initiator.

NVMe/TCP requires a host with the `nvme_tcp` kernel module (Linux 5.x+);
Windows NVMe/TCP support is initiator-dependent.

## Backup applications (Thur VTL)

Bareos is the CI-tested reference. Any application that drives a standard
SMC-3 changer + SSC-4 LTO library should work without a proprietary agent
— this includes Bacula, Amanda, and the major commercial suites (Veeam,
Veritas NetBackup, Commvault) when configured for a generic LTO-8 tape
library. These are not in the automated matrix; validate your specific
version against a non-production library first, and confirm the
application is set to LTO-8 (Thur ships a clean LTO-8 drive — LTO-7 media
creation is refused; see the behavioral-model doc).

## Hypervisors & guest OSes (Thur VSA)

A VSA volume is an ordinary thin-provisioned block device — partition,
format, and mount it like any disk. The Ubuntu VM CI test boots a full OS
from a volume; in practice any guest OS / hypervisor that consumes a
standard iSCSI or NVMe/TCP block target works (Linux, Windows, ESXi
datastores, Hyper-V). 4 KiB logical sectors, default 64 KiB page.

## Kubernetes

The Thur VSA CSI driver provisions, attaches, snapshots, clones, expands,
and deletes volumes for k8s workloads, with per-node CHAP isolation. See
[`CSI.md`](CSI.md).

## Caveats

- **LTO-8 only.** Thur emulates LTO-7/8; LTO-5/6 and LTO-9/SSC-5 are out
  of scope and declined at the CLI. Rationale: [`../dev/LTO-9.md`](../dev/LTO-9.md).
- **Authentication.** Under CHAP (iSCSI) / TLS-PSK (NVMe/TCP), the
  initiator must be configured with matching credentials, and VSA
  sessions need per-volume admission grants. See
  [`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).
- **Multipath / clustering.** Persistent reservations are implemented on
  both LUN types; a clustered host arbitrates correctly. iSCSI
  reservation keying (`iqn-isid` vs `iqn`) is tunable — see
  [`CONFIGURATION.md`](CONFIGURATION.md).
- **Advertised address.** Behind NAT / a container bridge, set the
  advertised address so discovery hands initiators a reachable IP
  ([`CONNECTING.md`](CONNECTING.md)).
