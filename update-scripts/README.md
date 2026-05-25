# update-scripts/

Operator scripts for an in-place daemon upgrade: `update-{vsa,vtl}-{deb,rpm}.sh`.
They quiesce the host (unmount loopback volumes / LTFS, log iSCSI
sessions out), swap the package, restart the daemon, and remount;
`lib.sh` carries the shared logic. `--dry-run` shows the plan
without changing anything.

The package source is the current directory by default; pass a
positional `package-dir` to point elsewhere, or `--use-repo` to
upgrade from the host's configured apt / yum / zypper repository
(refreshes repo metadata first so a newly published release is
picked up). `--use-repo` and `package-dir` are mutually exclusive.

Loopback only — see the header comments in `lib.sh` for the full
sequence and caveats.
