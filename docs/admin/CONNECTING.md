# Connecting Hosts

How an initiator reaches the devices a daemon serves. Thur VTL serves
iSCSI only; Thur VSA serves iSCSI and/or NVMe/TCP depending on
`transports:` in its conffile. The on-the-wire identities are fixed:

| | iSCSI IQN / NVMe NQN | Default port |
|---|---|---|
| Thur VTL | `iqn.2025-10.com.metebalci:thurvtl` | 3260 |
| Thur VSA (iSCSI) | `iqn.2025-10.com.metebalci:thurvsa` | 3260 |
| Thur VSA (NVMe/TCP) | `nqn.2025-10.com.metebalci:thurvsa` | 4420 |

Co-resident installs share port 3260 by default — override one product's
port in YAML so they don't clash. The target advertises the connection's
local IP in discovery, which matters for containers and NAT (see
[Advertised address](#advertised-address)).

For the day-to-day `iscsiadm` verbs — discover, login, logout, rescan,
forget — keep [`ISCSIADM.md`](ISCSIADM.md) handy; it is the initiator-side
cheatsheet. Authentication setup (CHAP, TLS-PSK, per-host/per-volume
admission) is in [`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).

## iSCSI (both products)

```bash
# 1. Discover (writes node records under /etc/iscsi/nodes/)
sudo iscsiadm -m discovery -t sendtargets -p <target_ip>:3260

# 2. Log in
sudo iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvtl \
     -p <target_ip>:3260 --login

# 3. See the devices
lsscsi -g
```

For Thur VTL the changer and drive LUNs appear as kernel devices:

```
[7:0:0:0]  mediumx MB      THUR VTL       NVL8  /dev/sch0
[7:0:0:1]  tape    MB      Ultrium 8-SCSI NVL8  /dev/st0
```

For Thur VSA each volume is a LUN — an ordinary block device (`sdb`,
`sdc`, …) you partition, format, and mount like any disk.

On **Windows**, use the built-in iSCSI Initiator (Control Panel): add the
portal, connect, and the devices appear under Tape drives and Medium
Changers (VTL) or Disk Management (VSA).

## NVMe/TCP (Thur VSA)

Enabled by listing `nvmetcp` in `transports:` (`[iscsi, nvmetcp]` serves
both at once; a volume is then reachable over either transport):

```bash
sudo nvme discover -t tcp -a <target_ip> -s 4420
sudo nvme connect  -t tcp -a <target_ip> -s 4420 \
     -n nqn.2025-10.com.metebalci:thurvsa
sudo nvme list
```

The transport design (handshake, R2T flow, auth) is in
[`../reference/NVMETCP.md`](../reference/NVMETCP.md).

## After creating or cloning a volume — rescan

A new or cloned VSA LUN is not visible until the host rescans the live
session — no relogin needed:

```bash
sudo iscsiadm -m session --rescan        # iSCSI
sudo nvme ns-rescan /dev/nvme0           # NVMe/TCP
```

If you destroy and recreate a volume, the kernel keeps the old partition
table cached and the new contents won't show in `lsblk` / `/proc/partitions`.
Force a re-read on the affected device:

```bash
sudo blockdev --rereadpt /dev/sdb        # or /dev/nvme0n1
```

When CHAP (iSCSI) or TLS-PSK (NVMe/TCP) is on, a freshly created or cloned
volume also needs an **admission grant** before the host can see it —
`iscsi users grant` / `nvmetcp psks grant`. See
[`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).

## Advertised address

The target hands initiators an IP to connect back on. With host
networking (or bare metal) the bound address works as-is. Behind a Docker
bridge with published ports, or any NAT, set the advertised address so the
initiator gets a reachable IP instead of the container-internal one:

```yaml
iscsi:
  listen: [{ bind: "0.0.0.0:3260", advertise: "<host-ip>:3260" }]
nvmetcp:
  advertise: "<host-ip>:4420"
```
