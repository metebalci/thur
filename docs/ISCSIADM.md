# iscsiadm cheatsheet

The mental model: `iscsiadm` operates in *modes* (`-m`), and each mode is a
different namespace of objects.

## The four modes you'll actually use

| Mode          | What it manages                                                                                       |
| ------------- | ----------------------------------------------------------------------------------------------------- |
| `-m discovery`| Discovery records — "I ran SendTargets against portal X, here's what it returned"                     |
| `-m node`     | **Node records** — persistent config files under `/etc/iscsi/nodes/<iqn>/<portal>/` describing a target you *could* log into |
| `-m session`  | **Live sessions** — sessions currently up in the kernel, identified by an SID                         |
| `-m iface`    | Initiator-side network interface bindings                                                             |

`node` = config on disk. `session` = live connection in the kernel. They're
separate.

## Typical lifecycle

```bash
# 1. Discover — writes node records to /etc/iscsi/nodes/
iscsiadm -m discovery -t sendtargets -p 10.0.0.5

# 2. Login — reads the node record, opens a session
iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvsa -p 10.0.0.5 -l

# 3. See what's actually connected
iscsiadm -m session
# tcp: [3] 10.0.0.5:3260,1 iqn.2025-10.com.metebalci:thurvsa (non-flash)
#       ^sid

# 4. Logout
iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvsa -p 10.0.0.5 -u
#   …or…
iscsiadm -m session -r 3 --logout

# 5. Forget the target entirely (delete the node record on disk)
iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvsa -p 10.0.0.5 -o delete
```

## `-m node -u` vs `-m session --logout`

Both close a session. The difference is *how you name the thing you want
closed*:

- `-m node -T <iqn> -p <portal> -u` — "find any session matching this node
  record and log it out." Addresses by configuration. Works even if no
  session is up (no-op then). This is the usual one.
- `-m session -r <sid> --logout` — "log out session #3, whatever it is."
  Addresses by live SID. Only works when the session exists. Useful when
  you have multiple sessions to the same target (multipath) and want to
  drop a specific one.

The node record stays on disk in both cases. If iscsid has
`node.startup = automatic`, it'll log back in on next boot — that's why
step 5 (`-o delete`) exists.

## The four states a target can be in

```
                  discover           login
   (nothing) ─────────────────► node ──────────► node + session
                  delete              logout
             ◄─────────────── ◄──────────────
```

`node` = "I know about this target." `session` = "I'm connected to this
target right now." Most confusion comes from conflating those two.

## Other commands worth knowing

```bash
# Detailed session info (LUNs, CHAP state, negotiated params)
iscsiadm -m session -P 3

# Show / edit a node record without logging in
iscsiadm -m node -T <iqn> -p <portal>
iscsiadm -m node -T <iqn> -p <portal> -o update -n node.startup -v manual

# Rescan a live session for newly-added LUNs (no relogin)
iscsiadm -m session -r <sid> --rescan

# Log into / out of everything at once
iscsiadm -m node -L all
iscsiadm -m node -U all
```

Rule of thumb: `-m node` for "what I'm configured to talk to," `-m session`
for "what I'm actually talking to right now."

## Logging out a session with a mounted filesystem

Short answer: you can try, but the initiator will refuse (or the logout
will succeed and then everything mounted on top of it will throw I/O
errors).

- `iscsiadm -m node ... -u` checks whether the session's block devices are
  in use. If a filesystem is mounted on `/dev/sdX` from that session,
  logout returns busy and the session stays up.
- If you force the underlying tear-down (e.g. `iscsiadm -m session -r
  <sid> --logout` while in-flight I/O exists, or yanking the network),
  the SCSI device disappears from under the mounted filesystem. You then
  get EIO on every read/write, dirty pages can't flush, and you
  typically need `umount -l` plus a reboot to get clean again.

Correct order is always: `sync` → `umount` (and `vgchange -an` / close
LUKS / stop multipath, if applicable) → `iscsiadm ... -u`.
