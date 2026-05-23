# Thur VTL — Static Binary Tarball

This tarball contains the Thur VTL daemon and CLI plus reference
configuration. Use the `.deb` or `.rpm` from the same release if your
distribution supports them — they handle user creation, conffile
upgrades, and systemd registration automatically. This tarball exists
for distributions outside that matrix (Arch, Alpine, Slackware, custom
images, air-gapped hosts) where you'd rather lay things out by hand.

## What's inside

```
thurvtld                   Thur VTL Daemon (iSCSI on :3260, HTTP on :9090)
thurvtl                      Management CLI
thurvtld.service           systemd unit reference
thurvtl.yaml                     Minimal starter config — install to /etc/thurvtl/, edit
thurvtl.env                        Daemon env file: cloud creds, ${ENV_VAR} secrets, feature flags (vars commented; uncomment what you need)
thurvtl.defaults.yaml            Full reference: every key documented (read-only docs)
thurvtl-completion.bash      Bash completion for the CLI
thurvtl-completion.zsh       Zsh completion for the CLI
LICENSE                            Apache License 2.0 — full text
README.md                          This file
```

Thur VTL is licensed under the **Apache License, Version 2.0**
(see `LICENSE`).

The binaries are statically linked against OpenSSL and zstd, dynamically
against glibc only. A single binary runs on any glibc 2.31+ host.

## Install

Run as root.

```sh
# 1. Binaries
install -m 755 thurvtld thurvtl /usr/bin/

# 2. Systemd unit
install -m 644 thurvtld.service /etc/systemd/system/

# 3. System user (the unit runs as thurvtl:thurvtl)
groupadd --system thurvtl
useradd --system --gid thurvtl \
        --home-dir /var/lib/thurvtl \
        --shell /usr/sbin/nologin \
        --comment "Thur VTL daemon" \
        thurvtl

# 4. Config + data directories
install -d -m 755 /etc/thurvtl
install -m 640 thurvtl.yaml /etc/thurvtl/thurvtl.yaml
# Daemon env file. Loaded by the systemd unit; ships with all vars
# commented out. Holds cloud creds, ${ENV_VAR} secrets referenced
# from thurvtl.yaml (alerting tokens, SMTP passwords), and any
# future env-var feature flags. root:thurvtl 0640 so the daemon
# group can read once you uncomment something.
install -m 640 -o root -g thurvtl thurvtl.env /etc/thurvtl/thurvtl.env
# Full reference (every key, with comments) — keep alongside the live
# config for quick lookup. Or read it via `thurvtl config defaults`.
install -d -m 755 /usr/share/doc/thurvtl
install -m 644 thurvtl.defaults.yaml /usr/share/doc/thurvtl/thurvtl.defaults.yaml
install -d -m 750 -o thurvtl -g thurvtl /var/lib/thurvtl

# 5. Shell completions (optional but useful)
install -m 644 thurvtl-completion.bash \
        /usr/share/bash-completion/completions/thurvtl
install -m 644 thurvtl-completion.zsh \
        /usr/share/zsh/site-functions/_thurvtl

# 6. Register the unit (does NOT enable / start)
systemctl daemon-reload
```

## Configure and start

```sh
# Edit the config: at minimum uncomment one cloud.backends entry.
# data_dir defaults to /var/lib/thurvtl (created with the right
# ownership in step 4 above) — change it only if you want tape data
# elsewhere. The starter only ships with required keys; refer to
# /usr/share/doc/thurvtl/thurvtl.defaults.yaml (or
# `thurvtl config defaults`) for every key the daemon understands.
${EDITOR:-vi} /etc/thurvtl/thurvtl.yaml

# Initialize the library topology (one-time)
sudo -u thurvtl thurvtl library init \
    --slots 40 --drives 3 --lto-generation 8

# Enable + start
systemctl enable --now thurvtld

# Tail logs
journalctl -u thurvtld -f
```

The daemon is **not** started automatically — it needs an edited
`/etc/thurvtl/thurvtl.yaml` and an initialized library before it
can run.

### Operator access to the CLI

Once the daemon is running, the admin Unix socket is at
`/run/thurvtl/admin.sock` (mode `0660`, owned by
`thurvtl:thurvtl`). To drive `thurvtl` against it, pick
one:

```sh
# Add operator users to the thurvtl group (log out / log back in
# to apply). Recommended for interactive use.
sudo usermod -aG thurvtl <user>

# Or run the CLI as the daemon user each time. Recommended where
# policy forbids extra service-group memberships on operator
# accounts.
sudo -u thurvtl thurvtl ...
```

## Uninstall

```sh
systemctl disable --now thurvtld
rm -f /usr/bin/thurvtld /usr/bin/thurvtl
rm -f /etc/systemd/system/thurvtld.service
rm -f /usr/share/bash-completion/completions/thurvtl
rm -f /usr/share/zsh/site-functions/_thurvtl
systemctl daemon-reload

# Operator decides whether to keep these — they hold tape data
# and operator edits respectively:
#   /var/lib/thurvtl/        (cartridge data, indexes, audit log)
#   /etc/thurvtl/            (operator-edited config)

userdel thurvtl
groupdel thurvtl
```

## Verifying the artifact

The release publishes a detached GPG signature alongside the tarball
(`*.tar.gz.asc`). Verify against the release public key (fingerprint
published alongside the artifact) before running anything:

```sh
gpg --verify thurvtl-<version>-x86_64.tar.gz.asc thurvtl-<version>-x86_64.tar.gz
```
