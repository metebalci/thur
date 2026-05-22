# update-scripts/

Operator scripts for an in-place daemon upgrade from a local
package: `update-{vsa,vtl}-{deb,rpm}.sh`. They quiesce the host
(unmount loopback volumes / LTFS, log iSCSI sessions out), swap the
package, restart the daemon, and remount; `lib.sh` carries the
shared logic. `--dry-run` shows the plan without changing anything.

Loopback only — see the header comments in `lib.sh` for the full
sequence and caveats.
