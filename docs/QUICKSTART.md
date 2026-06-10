# Quick Start

The shortest path from nothing to a working virtual device. It uses the
`local` filesystem backend (no cloud account, no credentials) so you can
see the whole loop on one machine; swap in a real object-store backend
later from [`admin/CONFIGURATION.md`](admin/CONFIGURATION.md) and
[`admin/AUTH.md`](admin/AUTH.md).

Pick the application you want:

- [Thur VTL](#thur-vtl-quick-start) — a virtual tape library for backup
  software.
- [Thur VSA](#thur-vsa-quick-start) — virtual block volumes over iSCSI or
  NVMe/TCP.

For the full installation matrix (apt/yum repo, GitHub Releases,
containers, air-gapped), see [`admin/INSTALLATION.md`](admin/INSTALLATION.md).
Concepts behind the terms used below — chunk pool, dedup, disk cache —
are in [`admin/CONCEPTS.md`](admin/CONCEPTS.md).

## Install

On any supported distro (Debian/Ubuntu, RHEL/Rocky/Alma, SLES/openSUSE):

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo bash
sudo apt install thurvtl    # or thurvsa, or both; dnf/zypper on RPM distros
```

Packages install the daemon + CLI but **do not** auto-start the daemon —
it needs configuration first. To run the daemon-routed CLI as a normal
user, join the application's group (log out and back in afterward):

```bash
sudo usermod -aG thurvtl $USER     # and/or thurvsa
```

---

## Thur VTL quick start

### 1. Configure

The packaged starter `/etc/thurvtl/thurvtl.yaml` already sets `data_dir`
(`/var/lib/thurvtl`) and a default library (40 slots, 3 drives, LTO-8),
so the **only** thing you must add is a storage backend. Uncomment the
`storage:` block and its `local` example (no cloud account needed):

```yaml
# data_dir and the library: block are already set by the starter;
# this is the one block you uncomment. Name the backend anything.
storage:
  backends:
    primary:
      type: local
      root_dir: /var/lib/thurvtl/storage-local
```

`num_slots` / `num_drives` are not locked in — edit the starter's
`library:` block and restart to resize the chassis. Growing always
succeeds; shrinking succeeds only down to what's in use — the daemon
refuses a shrink that would orphan a cartridge or a loaded drive
(`thurvtl library bounds` shows the safe-shrink floor). The daemon
materializes `<data_dir>/library/library.json` from this block on first
start and reconciles it on every restart. `thurvtl config defaults`
prints the annotated reference for every key.

### 2. Run

```bash
sudo systemctl enable --now thurvtld
sudo systemctl status thurvtld          # should be active (running)
```

### 3. Create a cartridge

Cartridges must exist before any backup software can write to them. This
is a CLI operation — no iSCSI initiator required:

```bash
thurvtl cartridge create TAPE001        # first free slot
thurvtl cartridge list
thurvtl changer inventory
```

### 4. Connect a host

From any iSCSI initiator, discover and log in; the changer and drive
LUNs appear as standard kernel devices:

```bash
sudo iscsiadm -m discovery -t sendtargets -p <target_ip>:3260
sudo iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvtl \
     -p <target_ip>:3260 --login
lsscsi -g
# [7:0:0:0]  mediumx MB      THUR VTL       NVL8  /dev/sch0
# [7:0:0:1]  tape    MB      Ultrium 8-SCSI NVL8  /dev/st0
```

### 5. Write and read

```bash
sudo mtx -f /dev/sch0 status                 # library + slot inventory
sudo mtx -f /dev/sch0 load 1 0               # load slot 1 -> drive 0
sudo tar -cvf /dev/nst0 /path/to/backup      # write
sudo tar -xvf /dev/nst0                      # restore
```

That is the full loop. Point your backup application (Bareos, Veeam,
NetBackup, …) at the library next — it sees an ordinary tape library, no
proprietary agent. Cartridge lifecycle (WORM, legal hold, encryption) is
in [`admin/CARTRIDGE.md`](admin/CARTRIDGE.md); connecting hosts in detail
in [`admin/CONNECTING.md`](admin/CONNECTING.md).

---

## Thur VSA quick start

### 1. Configure

The packaged starter `/etc/thurvsa/thurvsa.yaml` already sets `data_dir`
(`/var/lib/thurvsa`). The **only** thing you must add is a storage
backend — uncomment the `storage:` block and its `local` example (no
`library:` block; VSA serves block volumes):

```yaml
# data_dir is already set by the starter; this is the one block you
# uncomment. Name the backend anything.
storage:
  backends:
    primary:
      type: local
      root_dir: /var/lib/thurvsa/storage-local
# transports: [iscsi]        # default; use [iscsi, nvmetcp] to serve both
```

### 2. Run

```bash
sudo systemctl enable --now thurvsad
sudo systemctl status thurvsad
```

### 3. Create a volume

```bash
thurvsa volume create myvol --size 100G     # thin-provisioned
thurvsa volume list
```

### 4. Connect a host

```bash
# iSCSI (port 3260)
sudo iscsiadm -m discovery -t sendtargets -p <target_ip>:3260
sudo iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvsa \
     -p <target_ip>:3260 --login

# …or NVMe/TCP (port 4420), if nvmetcp is in transports:
sudo nvme connect -t tcp -a <target_ip> -s 4420 \
     -n nqn.2025-10.com.metebalci:thurvsa
```

### 5. Format and mount

The host now sees a thin-provisioned block device — treat it like any
disk:

```bash
lsblk                                        # find it (e.g. sdb, nvme0n1)
sudo parted -s /dev/sdb mklabel gpt mkpart primary ext4 0% 100%
sudo mkfs.ext4 /dev/sdb1
sudo mkdir -p /mnt/myvol && sudo mount /dev/sdb1 /mnt/myvol
```

After creating more volumes, rescan the initiator so the host sees the
new LUN (`iscsiadm -m session --rescan` or `nvme ns-rescan /dev/nvme0`).
Volume operations — snapshots, clones, online resize, encryption — are
in [`admin/VOLUME.md`](admin/VOLUME.md); host connection
detail in [`admin/CONNECTING.md`](admin/CONNECTING.md).

---

## Next steps

- Use a real backend (S3 / GCS / Azure / S3-compatible):
  [`admin/CONFIGURATION.md`](admin/CONFIGURATION.md),
  [`admin/AUTH.md`](admin/AUTH.md),
  [`admin/S3_BACKENDS.md`](admin/S3_BACKENDS.md).
- Secure it: CHAP / TLS-PSK, admin password, TLS —
  [`admin/NETWORK_SECURITY.md`](admin/NETWORK_SECURITY.md).
- Before going live: [`admin/PRODUCTION_READINESS.md`](admin/PRODUCTION_READINESS.md).
- The whole Admin Guide: [`admin/README.md`](admin/README.md).
