# scsi/

SCSI command-set crates — the wire-level opcode dispatch. `scsi-spc`
(SPC-4 baseline: INQUIRY / mode / persistent reservations), `scsi-ssc`
(tape drive LUN), `scsi-smc` (medium-changer LUN), `scsi-sbc`
(block LUN).

The SCSI surface we present is in
[`docs/reference/CONFORMANCE_SCSI.md`](../docs/reference/CONFORMANCE_SCSI.md); opcode-level
detail in [`docs/reference/SPEC.md`](../docs/reference/SPEC.md).
