# Reference

How Thur works beneath the device surface — wire contracts, conformance
tables, and storage/transport internals. Not needed to operate the
products (that's the [Admin Guide](../admin/)); needed to understand,
debug, or extend them.

- [SPEC.md](SPEC.md) — the wire specification: SCSI opcodes, VPD / mode /
  log pages, manifest + library/inventory schemas, on-disk and
  storage-backend layout, iSCSI / LTO emulation IDs, telemetry inventory.
- [CONFORMANCE_SCSI.md](CONFORMANCE_SCSI.md) — SPC-4 / SAM-5 / iSCSI /
  CHAP (shared), the SSC-4 / SMC-3 tape surface and its deliberate
  divergences from physical LTO, and the SBC-3 block surface.
- [CONFORMANCE_NVME.md](CONFORMANCE_NVME.md) — NVMe Base / NVM Command
  Set / NVMe-oF / NVMe-TCP, including TLS-PSK.
- [STORAGE.md](STORAGE.md) — on-disk layout: the chunk pool, cartridge
  and volume directories, indexes, snapshots.
- [DEDUP.md](DEDUP.md) — the content-addressed deduplication mechanism.
- [BACKPRESSURE.md](BACKPRESSURE.md) — the upload-backpressure design.
- [NVMETCP.md](NVMETCP.md) — the NVMe/TCP transport walkthrough.
- [WEBUI.md](WEBUI.md) — the embedded read-only web console.
- [openapi.yaml](openapi.yaml) / [openapi-admin.yaml](openapi-admin.yaml)
  — the read-only HTTP `/api/v1` and admin-socket API contracts.
