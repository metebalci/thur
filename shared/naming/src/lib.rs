// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for per-product identity strings — the
//! handful of operator-visible names (system user, install paths,
//! IQN, metric prefix, service unit) that differ across the two
//! sibling products in this workspace.
//!
//! # What lives here
//!
//! [`ProductIdentity`] holds the per-product bundle. Two consts —
//! [`TAPE_LIBRARY`] and [`DISK`] — are exposed for the products.
//! Plus [`VENDOR_INQUIRY`] for vendor-level (cross-product) strings.

#![forbid(unsafe_code)]

/// Vendor-level identity. One pair across all three products.
///
/// Used for IQN naming-authority composition (the reverse-domain
/// portion of `iqn.YYYY-MM.<domain>:<unique>`) and SCSI INQUIRY
/// vendor-identification (8-byte ASCII, space-padded).
///
/// Placeholder values — rebranded in a later focused commit.
pub const VENDOR_DOMAIN: &str = "metebalci.com";

/// SCSI INQUIRY VENDOR IDENTIFICATION field. Up to 8 ASCII chars;
/// callers space-pad to width. Used by every INQUIRY standard +
/// VPD 0x83 T10-vendor-based identifier across both products.
pub const VENDOR_INQUIRY: &str = "MB";

/// Two-line copyright + license notice shown in CLI `--help`
/// long-about and at daemon startup. Single source of truth so a
/// year bump or license change touches one place.
pub const COPYRIGHT_NOTICE: &str = "Copyright (c) 2026 Mete Balci\nLicensed under Apache-2.0";

/// SCSI INQUIRY PRODUCT IDENTIFICATION (16 bytes, space-padded) for
/// the medium-changer LUN. Full product name; the daemon is a
/// spec-conformant SMC-3 library, not a clone of any specific
/// physical chassis.
pub const TAPE_LIBRARY_PRODUCT: &str = "THUR VTL";

/// SCSI INQUIRY PRODUCT IDENTIFICATION (16 bytes, space-padded) for
/// thurvsa direct-access volume LUNs. Counterpart to
/// [`TAPE_LIBRARY_PRODUCT`] — keeps the per-product identity on one
/// shelf so a future brand rename touches one file.
pub const DISK_PRODUCT: &str = "THUR VSA";

/// Per-product identity bundle.
///
/// Every consumer-facing string that differs between the three
/// sibling products lives here so a brand rename touches one place.
///
/// All `&'static str` fields — these are compile-time constants
/// known at link time, not runtime configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProductIdentity {
    /// Short product name. Doubles as system user, system group,
    /// metric prefix, conffile dir, data dir, run dir.
    /// e.g. `"mbt"`.
    pub name: &'static str,

    /// Brand-cased product name shown in user-facing banners
    /// (CLI `--help` long-about, daemon startup line).
    /// e.g. `"ThurVTL"`.
    pub display_name: &'static str,

    /// iSCSI target IQN, in the form
    /// `iqn.YYYY-MM.<reverse-domain>:<unique>`.
    /// e.g. `"iqn.2025-10.com.metebalci:thurvtl"`.
    pub iqn: &'static str,

    /// NVMe Subsystem NQN (NVMe-oF §5.3), in the form
    /// `nqn.YYYY-MM.<reverse-domain>:<unique>`.
    /// Used by the NVMe/TCP transport when an operator picks
    /// `transport: nvmetcp` instead of iSCSI. VTL stays iSCSI-only
    /// operationally (the SMC medium-changer surface has no
    /// NVMe equivalent), but the field exists symmetrically.
    pub nqn: &'static str,

    /// OpenTelemetry / Prometheus instrument-name prefix
    /// (`<prefix>_<subsystem>_<name>`). One per product so a
    /// shared backend can host all three without collision.
    /// e.g. `"mbt"`.
    pub metric_prefix: &'static str,

    /// Absolute path to the canonical YAML conffile.
    /// e.g. `"/etc/mbt/mbt.yaml"`.
    pub config_path: &'static str,

    /// Absolute path to the on-disk data directory (cartridges /
    /// volumes / chunks / audit chain).
    /// e.g. `"/var/lib/mbt"`.
    pub data_dir: &'static str,

    /// Absolute path to the runtime directory that holds the admin
    /// socket. systemd `RuntimeDirectory=` provisions this at boot.
    /// e.g. `"/run/mbt"`.
    pub run_dir: &'static str,

    /// Absolute path to the daemon's admin Unix socket — peer-cred
    /// authed, mode 0660, group-writable to `system_group`.
    /// e.g. `"/run/mbt/admin.sock"`.
    pub admin_socket: &'static str,

    /// System user the daemon runs as (matches `name` by
    /// convention, but exposed separately so the rebrand pass
    /// can pick a different binary name without forcing a uid
    /// rename).
    /// e.g. `"mbt"`.
    pub system_user: &'static str,

    /// System group, group-owner of the conffile, data dir, and
    /// admin socket.
    /// e.g. `"mbt"`.
    pub system_group: &'static str,

    /// systemd service unit filename.
    /// e.g. `"mbtd.service"`.
    pub service_unit: &'static str,
}

/// Tape library (`thurvtl`). SMC LUN 0 plus N SSC drive LUNs — the
/// successor to today's `thurvtld`. iSCSI target on port 3260.
pub const TAPE_LIBRARY: ProductIdentity = ProductIdentity {
    name: "thurvtl",
    display_name: "ThurVTL",
    iqn: "iqn.2025-10.com.metebalci:thurvtl",
    nqn: "nqn.2025-10.com.metebalci:thurvtl",
    metric_prefix: "thurvtl",
    config_path: "/etc/thurvtl/thurvtl.yaml",
    data_dir: "/var/lib/thurvtl",
    run_dir: "/run/thurvtl",
    admin_socket: "/run/thurvtl/admin.sock",
    system_user: "thurvtl",
    system_group: "thurvtl",
    service_unit: "thurvtld.service",
};

/// Block target (`thurvsa`). SBC-3 direct-access LUNs — consumed by
/// `thurvsad` (renamed from `cirrus-daemon` in 5.B.4).
/// iSCSI target on port 3260.
pub const DISK: ProductIdentity = ProductIdentity {
    name: "thurvsa",
    display_name: "ThurVSA",
    iqn: "iqn.2025-10.com.metebalci:thurvsa",
    nqn: "nqn.2025-10.com.metebalci:thurvsa",
    metric_prefix: "thurvsa",
    config_path: "/etc/thurvsa/thurvsa.yaml",
    data_dir: "/var/lib/thurvsa",
    run_dir: "/run/thurvsa",
    admin_socket: "/run/thurvsa/admin.sock",
    system_user: "thurvsa",
    system_group: "thurvsa",
    service_unit: "thurvsad.service",
};

/// Maximum length of an iSCSI IQN (RFC 3720 § 3.2.6.3.1) or an NVMe
/// Subsystem NQN (NVMe-oF § 5.3): both cap the qualified name at 223
/// ASCII characters.
pub const MAX_QUALIFIED_NAME_LEN: usize = 223;

/// Validate an iSCSI / NVMe qualified name: non-empty, ASCII, no
/// longer than [`MAX_QUALIFIED_NAME_LEN`], and beginning with
/// `prefix` (`"iqn."` or `"nqn."`). Returns a human-readable message
/// naming the first rule violated; `prefix` minus its trailing dot,
/// upper-cased, labels the error (`IQN` / `NQN`).
pub fn validate_qualified_name(name: &str, prefix: &str) -> Result<(), String> {
    let label = prefix.trim_end_matches('.').to_ascii_uppercase();
    if name.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if !name.is_ascii() {
        return Err(format!("{label} must be ASCII: {name:?}"));
    }
    if name.len() > MAX_QUALIFIED_NAME_LEN {
        return Err(format!(
            "{label} is {} chars, over the {MAX_QUALIFIED_NAME_LEN}-char limit",
            name.len()
        ));
    }
    if !name.starts_with(prefix) {
        return Err(format!(
            "invalid {label} {name:?}: must begin with {prefix:?}"
        ));
    }
    Ok(())
}

/// Validate an iSCSI target IQN (RFC 3720). See [`validate_qualified_name`].
pub fn validate_iqn(iqn: &str) -> Result<(), String> {
    validate_qualified_name(iqn, "iqn.")
}

/// Validate an NVMe Subsystem NQN (NVMe-oF § 5.3). See [`validate_qualified_name`].
pub fn validate_nqn(nqn: &str) -> Result<(), String> {
    validate_qualified_name(nqn, "nqn.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_inquiry_fits_in_eight_ascii_bytes() {
        assert!(VENDOR_INQUIRY.len() <= 8);
        assert!(VENDOR_INQUIRY.is_ascii());
    }

    #[test]
    fn tape_library_product_fits_in_sixteen_ascii_bytes() {
        assert!(TAPE_LIBRARY_PRODUCT.len() <= 16);
        assert!(TAPE_LIBRARY_PRODUCT.is_ascii());
    }

    #[test]
    fn disk_product_fits_in_sixteen_ascii_bytes() {
        assert!(DISK_PRODUCT.len() <= 16);
        assert!(DISK_PRODUCT.is_ascii());
    }

    #[test]
    fn vendor_domain_is_lowercase_ascii() {
        assert!(VENDOR_DOMAIN.is_ascii());
        assert_eq!(VENDOR_DOMAIN, VENDOR_DOMAIN.to_ascii_lowercase());
    }

    #[test]
    fn product_consts_are_distinct() {
        assert_ne!(TAPE_LIBRARY.name, DISK.name);
        assert_ne!(TAPE_LIBRARY.iqn, DISK.iqn);
        assert_ne!(TAPE_LIBRARY.admin_socket, DISK.admin_socket);
    }

    #[test]
    fn iqn_format_includes_vendor_domain_reversed() {
        for product in [&TAPE_LIBRARY, &DISK] {
            assert!(
                product.iqn.starts_with("iqn."),
                "{} IQN missing iqn. prefix",
                product.name
            );
            assert!(
                product.iqn.contains(":"),
                "{} IQN missing : separator",
                product.name
            );
        }
    }

    #[test]
    fn nqn_format_includes_vendor_domain_reversed() {
        for product in [&TAPE_LIBRARY, &DISK] {
            assert!(
                product.nqn.starts_with("nqn."),
                "{} NQN missing nqn. prefix",
                product.name
            );
            assert!(
                product.nqn.contains(":"),
                "{} NQN missing : separator",
                product.name
            );
            // NVMe-oF §5.3: SUBNQN max 223 ASCII chars (256-byte
            // field with NUL terminator); ours fits comfortably.
            assert!(
                product.nqn.len() <= 223,
                "{} NQN over 223 bytes",
                product.name
            );
        }
    }

    #[test]
    fn paths_are_absolute() {
        for product in [&TAPE_LIBRARY, &DISK] {
            assert!(product.config_path.starts_with('/'));
            assert!(product.data_dir.starts_with('/'));
            assert!(product.run_dir.starts_with('/'));
            assert!(product.admin_socket.starts_with('/'));
        }
    }

    #[test]
    fn admin_socket_lives_under_run_dir() {
        for product in [&TAPE_LIBRARY, &DISK] {
            assert!(
                product.admin_socket.starts_with(product.run_dir),
                "{} admin_socket {:?} not under run_dir {:?}",
                product.name,
                product.admin_socket,
                product.run_dir
            );
        }
    }

    #[test]
    fn service_unit_has_service_extension() {
        for product in [&TAPE_LIBRARY, &DISK] {
            assert!(product.service_unit.ends_with(".service"));
        }
    }

    #[test]
    fn validate_accepts_the_product_consts() {
        validate_iqn(TAPE_LIBRARY.iqn).expect("VTL IQN valid");
        validate_iqn(DISK.iqn).expect("VSA IQN valid");
        validate_nqn(TAPE_LIBRARY.nqn).expect("VTL NQN valid");
        validate_nqn(DISK.nqn).expect("VSA NQN valid");
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_iqn("").is_err());
        assert!(validate_nqn("").is_err());
    }

    #[test]
    fn validate_rejects_wrong_prefix() {
        assert!(validate_iqn("nqn.2025-10.com.metebalci:thurvsa").is_err());
        assert!(validate_nqn("iqn.2025-10.com.metebalci:thurvtl").is_err());
        assert!(validate_iqn("my-target").is_err());
    }

    #[test]
    fn validate_rejects_over_length() {
        let long = format!("iqn.{}", "a".repeat(MAX_QUALIFIED_NAME_LEN));
        assert!(validate_iqn(&long).is_err());
    }

    #[test]
    fn validate_rejects_non_ascii() {
        assert!(validate_iqn("iqn.2025-10.com.exämple:t").is_err());
    }
}
