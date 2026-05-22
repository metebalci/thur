# core/

Device-type cores — the per-device-class storage logic each product
builds on. `core-stream` (SSC-4 tape cartridges), `core-mediachanger`
(SMC-3 medium changer + library), `core-block` (SBC-3 direct-access
volumes). All sit on the shared content-addressed chunk pool.

Per-crate breakdown: [`docs/WORKSPACE.md`](../docs/WORKSPACE.md).
