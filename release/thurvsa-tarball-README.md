# Thur VSA — Static Binary Tarball

This tarball contains the Thur VSA daemon and CLI plus reference
configuration. Use the `.deb` or `.rpm` from the same release if your
distribution supports them — they handle user creation, conffile
upgrades, and systemd registration automatically. This tarball exists
for distributions outside that matrix (Arch, Alpine, Slackware, custom
images, air-gapped hosts) where you'd rather lay things out by hand.

## What's inside

```
thurvsad                   Block-storage daemon (iSCSI on :3260, HTTP on :9090)
thurvsa                      Management CLI
thurvsad.service           systemd unit reference
thurvsa.yaml                     Minimal starter config — install to /etc/thurvsa/, edit
thurvsa.env                     Daemon env file: cloud creds, ${ENV_VAR} secrets, feature flags (vars commented; uncomment what you need)
thurvsa.defaults.yaml            Full reference: every key documented (read-only docs)
thurvsa-completion.bash      Bash completion for the CLI
thurvsa-completion.zsh       Zsh completion for the CLI
LICENSE                         Apache License 2.0 — full text
README.md                       This file
```

Thur VSA is licensed under the **Apache License, Version 2.0**
(see `LICENSE`).

The binaries are statically linked against OpenSSL and zstd, dynamically
against glibc only. A single binary runs on any glibc 2.31+ host.

## Co-existence with Thur VTL

Thur VSA and Thur VTL are sibling products that can run on the same host.
The system users (`thurvsa` vs `thurvtl`), data directories
(`/var/lib/thurvsa` vs `/var/lib/thurvtl`), conffile paths
(`/etc/thurvsa/` vs `/etc/thurvtl/`), unit names (`thurvsad` vs
`thurvtld`), and admin sockets
(`/run/thurvsa/admin.sock` vs `/run/thurvtl/admin.sock`) are disjoint.
iSCSI / HTTP ports default to the same number on both (3260 / 9090);
override one in YAML when co-residing. Install whichever halves you need.

## Install

Run as root.

```sh
# 1. Binaries
install -m 755 thurvsad thurvsa /usr/bin/

# 2. Systemd unit
install -m 644 thurvsad.service /etc/systemd/system/

# 3. System user (the unit runs as thurvsa:thurvsa)
groupadd --system thurvsa
useradd --system --gid thurvsa \
        --home-dir /var/lib/thurvsa \
        --shell /usr/sbin/nologin \
        --comment "Thur VSA daemon" \
        thurvsa

# 4. Config + data directories
install -d -m 755 /etc/thurvsa
install -m 640 thurvsa.yaml /etc/thurvsa/thurvsa.yaml
# Daemon env file. Loaded by the systemd unit; ships with all vars
# commented out. Holds cloud creds, ${ENV_VAR} secrets referenced
# from thurvsa.yaml (alerting tokens, SMTP passwords), and any
# future env-var feature flags. root:thurvsa 0640 so the daemon
# group can read once you uncomment something.
install -m 640 -o root -g thurvsa thurvsa.env /etc/thurvsa/thurvsa.env
# Full reference (every key, with comments) — keep alongside the live
# config for quick lookup. Or read it via `thurvsa config defaults`.
install -d -m 755 /usr/share/doc/thurvsa
install -m 644 thurvsa.defaults.yaml /usr/share/doc/thurvsa/thurvsa.defaults.yaml
install -d -m 750 -o thurvsa -g thurvsa /var/lib/thurvsa

# 5. Shell completions (optional but useful)
install -m 644 thurvsa-completion.bash \
        /usr/share/bash-completion/completions/thurvsa
install -m 644 thurvsa-completion.zsh \
        /usr/share/zsh/site-functions/_thurvsa

# 6. Register the unit (does NOT enable / start)
systemctl daemon-reload
```

## Configure and start

```sh
# Edit the config: at minimum uncomment one cloud.backends entry.
# data_dir defaults to /var/lib/thurvsa (created with the right
# ownership in step 4 above) — change it only if you want volume
# data elsewhere. The starter only ships with required keys; refer
# to /usr/share/doc/thurvsa/thurvsa.defaults.yaml (or
# `thurvsa config defaults`) for every key the daemon understands.
${EDITOR:-vi} /etc/thurvsa/thurvsa.yaml

# Enable + start
systemctl enable --now thurvsad

# Create your first volume (admin socket route — daemon must be running)
thurvsa volume create myvol --size 100G

# Tail logs
journalctl -u thurvsad -f
```

The daemon is **not** started automatically — it needs an edited
`/etc/thurvsa/thurvsa.yaml` (at least one `cloud.backends` entry
uncommented) before it can usefully run.

The target IQN is `iqn.2025-10.com.metebalci:thurvsa` on port 3260;
point your iSCSI initiator at that and a volume's assigned LUN.

### Operator access to the CLI

Once the daemon is running, the admin Unix socket is at
`/run/thurvsa/admin.sock` (mode `0660`, owned by
`thurvsa:thurvsa`). To drive `thurvsa` against it, pick
one:

```sh
# Add operator users to the thurvsa group (log out / log back in
# to apply). Recommended for interactive use.
sudo usermod -aG thurvsa <user>

# Or run the CLI as the daemon user each time. Recommended where
# policy forbids extra service-group memberships on operator
# accounts.
sudo -u thurvsa thurvsa ...
```

## Uninstall

```sh
systemctl disable --now thurvsad
rm -f /usr/bin/thurvsad /usr/bin/thurvsa
rm -f /etc/systemd/system/thurvsad.service
rm -f /usr/share/bash-completion/completions/thurvsa
rm -f /usr/share/zsh/site-functions/_thurvsa
systemctl daemon-reload

# Operator decides whether to keep these — they hold volume data
# and operator edits respectively:
#   /var/lib/thurvsa/        (volume manifests, page indexes, chunks, audit log)
#   /etc/thurvsa/            (operator-edited config)

userdel thurvsa
groupdel thurvsa
```

## Verifying the artifact

The release publishes a detached GPG signature alongside the tarball
(`*.tar.gz.asc`). Verify against the release public key (fingerprint
published alongside the artifact) before running anything:

```sh
gpg --verify thurvsa-<version>-x86_64.tar.gz.asc thurvsa-<version>-x86_64.tar.gz
```
