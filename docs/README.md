# Thur documentation

Documentation for **Thur VTL** (virtual tape library) and **Thur VSA**
(virtual storage appliance), organized into four sets by audience. Start
with whichever matches what you're doing.

## [Quick Start](QUICKSTART.md)

Install to a working virtual device in a few minutes, on one machine, with
the `local` backend. The fastest loop for both products.

## [Admin Guide](admin/) — operating it

The operator's handbook: installation, configuration, storage backends,
connecting hosts, day-to-day VTL and VSA operations, security, monitoring,
disaster recovery, troubleshooting, and a production-readiness checklist.
Index and reading order in [`admin/README.md`](admin/README.md).

Highlights: [`admin/CONCEPTS.md`](admin/CONCEPTS.md) ·
[`admin/CONFIGURATION.md`](admin/CONFIGURATION.md) ·
[`admin/CONNECTING.md`](admin/CONNECTING.md) ·
[`admin/VSA_OPERATIONS.md`](admin/VSA_OPERATIONS.md) ·
[`admin/CARTRIDGE.md`](admin/CARTRIDGE.md) ·
[`admin/NETWORK_SECURITY.md`](admin/NETWORK_SECURITY.md) ·
[`admin/DISASTER_RECOVERY.md`](admin/DISASTER_RECOVERY.md) ·
[`admin/TROUBLESHOOTING.md`](admin/TROUBLESHOOTING.md).

## [Reference](reference/) — how it works

The deep technical layer — not needed to use the products, needed to
understand or extend them:

- Wire spec: [`reference/SPEC.md`](reference/SPEC.md) (SCSI opcodes, VPD /
  mode / log pages, schemas, on-disk + storage-backend layout).
- Conformance: [`reference/CONFORMANCE_SCSI.md`](reference/CONFORMANCE_SCSI.md),
  [`reference/CONFORMANCE_NVME.md`](reference/CONFORMANCE_NVME.md).
- Internals: [`reference/STORAGE.md`](reference/STORAGE.md),
  [`reference/DEDUP.md`](reference/DEDUP.md),
  [`reference/BACKPRESSURE.md`](reference/BACKPRESSURE.md),
  [`reference/NVMETCP.md`](reference/NVMETCP.md),
  [`reference/WEBUI.md`](reference/WEBUI.md).
- API contracts: [`reference/openapi.yaml`](reference/openapi.yaml),
  [`reference/openapi-admin.yaml`](reference/openapi-admin.yaml).

## [Developer](dev/) — building & contributing

[`dev/DEVELOPMENT.md`](dev/DEVELOPMENT.md) (build from source, run the
suite) · [`dev/WORKSPACE.md`](dev/WORKSPACE.md) (crate map) ·
[`dev/TESTCOVERAGE.md`](dev/TESTCOVERAGE.md) ·
[`dev/RELEASING.md`](dev/RELEASING.md) ·
[`dev/LTO-9.md`](dev/LTO-9.md).

---

Repo orientation for contributors and tooling is in
[`../CLAUDE.md`](../CLAUDE.md). Project overview and install one-liner are
in the top-level [`../README.md`](../README.md).
