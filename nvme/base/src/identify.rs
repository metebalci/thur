// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Identify data structures (NVMe Base §5.17).
//!
//! Two 4 KiB structures the controller emits in response to the
//! Identify admin command:
//!
//! - [`IdentifyController`] — CNS = 0x01. One per subsystem.
//! - [`IdentifyNamespace`] — CNS = 0x00. One per attached NSID.
//!
//! A third common Identify return is the **Active Namespace ID List**
//! (CNS = 0x02): 1024 little-endian u32 NSIDs starting after the one
//! passed in CDW1.NSID, zero-padded. That's a builder helper rather
//! than a struct ([`active_namespace_list`]).
//!
//! Only the fields the target actually populates are exposed here.
//! Every other byte of the 4 KiB structure stays zero — NVMe's
//! convention is "if the field isn't supported, leave it zero", and
//! initiators tolerate that for everything outside the few required
//! lanes (Subsystem NQN on a fabrics controller, NN, NSZE/NCAP/NUSE).

use crate::error::NvmeError;

/// CNS — Controller-or-Namespace-Structure selector (NVMe Base
/// §5.17.1 Figure 248). The Identify admin command's CDW10[7:0].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CNS {
    /// Identify Namespace — returns 4 KiB describing the NSID in
    /// SQE.NSID.
    Namespace = 0x00,
    /// Identify Controller — returns 4 KiB describing the controller
    /// (vendor / model / NN / OACS / ...).
    Controller = 0x01,
    /// Active Namespace ID List — returns up to 1024 u32 NSIDs
    /// greater than SQE.NSID, zero-padded.
    ActiveNamespaceList = 0x02,
    /// Namespace Identification Descriptor list — returns one or
    /// more descriptors (EUI64 / NGUID / UUID / CSI) for the NSID
    /// in SQE.NSID, zero-terminated. Linux nvme-tcp issues this
    /// right after Identify Namespace; returning anything other
    /// than success kills the namespace attach silently.
    NamespaceIdDescList = 0x03,
    /// I/O Command Set specific Identify Controller (NVMe Base 2.0
    /// §5.17.2.16). Linux nvme-tcp's `nvme_init_identify` calls this
    /// for the NVM Command Set (CSI=0x00, NSID=0) during bring-up
    /// even against a 1.4-versioned controller; refusing it (Invalid
    /// Field) propagates into the namespace-attach path and `/dev/
    /// nvmeXn1` never appears. The wire layout is a 4 KiB structure
    /// whose every field is a "no specific limit" hint when zero,
    /// so all-zeros is the right minimal response.
    IoCommandSetIdentifyController = 0x06,
}

impl CNS {
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Namespace,
            0x01 => Self::Controller,
            0x02 => Self::ActiveNamespaceList,
            0x03 => Self::NamespaceIdDescList,
            0x06 => Self::IoCommandSetIdentifyController,
            _ => return None,
        })
    }
}

/// Identify Controller (NVMe Base §5.17.2.1, 4 KiB).
///
/// Only the operator-visible identity fields plus the absolutely
/// required lanes are exposed. Defaults render zero — which is what
/// every other field should be on a software target that doesn't
/// implement the optional feature.
#[derive(Debug, Clone)]
pub struct IdentifyController {
    /// VID — PCI Vendor ID. Zero for fabrics-only targets; physical
    /// NVMe SSDs carry the assigned PCI-SIG value.
    pub vid: u16,
    /// SSVID — PCI Subsystem Vendor ID. Zero for fabrics-only.
    pub ssvid: u16,
    /// SN — Serial Number (20 bytes, ASCII, space-padded).
    pub sn: String,
    /// MN — Model Number (40 bytes, ASCII, space-padded).
    pub mn: String,
    /// FR — Firmware Revision (8 bytes, ASCII, space-padded).
    pub fr: String,
    /// CNTLID — Controller ID (16-bit).
    pub cntlid: u16,
    /// VER — NVMe-spec version this target implements. Bit layout
    /// is `MJR(16) | MNR(8) | TER(8)`. We claim 1.4.0 = 0x00010400.
    pub ver: u32,
    /// NN — Number of Namespaces. Maximum NSID the host may use.
    /// VSA today supports one namespace per volume; the daemon fills
    /// this in at boot from the registry's len().
    pub nn: u32,
    /// SUBNQN — NVM Subsystem NQN, 256-byte ASCII, NUL-padded
    /// (NVMe-oF §5.3). Required for fabrics.
    pub subnqn: String,
    /// IOCCSZ — I/O Queue Command Capsule Size (NVMe-oF §5.2.1) in
    /// units of 16 bytes. Minimum 4 (= 64 byte SQE only, no inline
    /// data). The dispatcher reports the largest inline data the
    /// target will accept in one capsule.
    pub ioccsz: u32,
    /// IORCSZ — I/O Queue Response Capsule Size, in units of 16 bytes.
    /// Always 1 for us (= 16 byte CQE only).
    pub iorcsz: u32,
}

/// Keep Alive Support advertised in Identify Controller (NVMe Base
/// §5.17.2.1 byte 320, units of 100 ms). NVMe-oF makes Keep Alive
/// mandatory — Linux nvme-tcp explicitly checks for `kas != 0` and
/// refuses the controller with "keep-alive support is mandatory for
/// fabrics" otherwise. 12 s is a conservative default that hosts
/// freely scale via Set Features 0x0F (KATO).
const KAS_GRANULARITY_100MS: u16 = 120;

impl IdentifyController {
    /// SN / MN / FR are fixed-width ASCII fields on the wire (20 / 40
    /// / 8 bytes); `to_bytes` truncates anything longer via
    /// `write_ascii_padded`. We don't pre-validate their lengths
    /// because returning an error here would surface to the host as
    /// SC=0x06 (Internal Error) on Identify Controller, which Linux
    /// nvme-cli renders as the very opaque "Identify Controller
    /// failed (6)" before tearing the session down — much worse than
    /// silently truncating an identity string the host displays only
    /// in `nvme id-ctrl` output. SUBNQN stays validated because a
    /// mis-shaped subsystem NQN is a semantic mismatch, not a cosmetic
    /// truncation.
    pub fn new(
        sn: String,
        mn: String,
        fr: String,
        cntlid: u16,
        nn: u32,
        subnqn: String,
    ) -> Result<Self, NvmeError> {
        if subnqn.len() > 256 {
            return Err(NvmeError::FieldTooLong {
                field: "SUBNQN",
                got: subnqn.len(),
                max: 256,
            });
        }
        Ok(Self {
            vid: 0,
            ssvid: 0,
            sn,
            mn,
            fr,
            cntlid,
            ver: 0x0001_0400, // NVMe 1.4.0
            nn,
            subnqn,
            // Match the largest practical capsule we plan to accept
            // (16 KiB inline data + 64 byte SQE = 16448 bytes;
            // 16448 / 16 = 1028). Final value tunes when nvme-tcp
            // wires its inline-data handler.
            ioccsz: 1028,
            iorcsz: 1,
        })
    }

    /// Encode to the 4 KiB wire image (NVMe Base §5.17.2.1 layout).
    /// Only the fields above are populated; everything else stays
    /// zero. Initiators are explicit about treating unknown / zero
    /// optional fields as "feature not supported" so this is safe.
    pub fn to_bytes(&self) -> [u8; crate::IDENTIFY_DATA_SIZE] {
        let mut out = [0u8; crate::IDENTIFY_DATA_SIZE];
        out[0..2].copy_from_slice(&self.vid.to_le_bytes());
        out[2..4].copy_from_slice(&self.ssvid.to_le_bytes());
        write_ascii_padded(&mut out[4..24], &self.sn, b' ');
        write_ascii_padded(&mut out[24..64], &self.mn, b' ');
        write_ascii_padded(&mut out[64..72], &self.fr, b' ');
        // RAB at 73 = 0 (no recommended arbitration burst hint)
        // IEEE OUI at 73..76 zero
        // CMIC at 76 zero (no multi-path)
        // MDTS at 77 zero (no transfer-size limit beyond what the
        //   transport enforces)
        out[78..80].copy_from_slice(&self.cntlid.to_le_bytes());
        out[80..84].copy_from_slice(&self.ver.to_le_bytes());
        // OAES, CTRATT etc all zero
        // KAS at 320..322 — mandatory non-zero for NVMe-oF
        // controllers (Linux nvme-tcp logs "keep-alive support is
        // mandatory for fabrics" and aborts otherwise).
        out[320..322].copy_from_slice(&KAS_GRANULARITY_100MS.to_le_bytes());
        out[516..520].copy_from_slice(&self.nn.to_le_bytes());
        // VWC at byte 525 (NVMe Base §5.17.2.1) — bit 0 = "controller
        // has a volatile write cache". Without this, Linux's nvme
        // driver treats the controller as having no cache, never
        // issues NVMe Flush, and the filesystem layer's sync/umount
        // path silently bypasses the daemon's PageCache → pool
        // sealing fence. Set bit 0 so ext4 + sync issues Flush,
        // which we map to `PageCache::synchronize_bytes` and which
        // is the only path that drains dirty pages to the chunk pool
        // during normal operation.
        out[525] = 0b0000_0001;
        // SGLS at 536..540 — bit 0 = 1 (SGLs supported), bits 17..16
        // = 0b01 (keyed SGLs not required). NVMe-oF mandates SGLs.
        out[536..540].copy_from_slice(&0x0000_0001u32.to_le_bytes());
        write_ascii_padded(&mut out[768..1024], &self.subnqn, 0);
        // Fabrics-specific: IOCCSZ at 1792..1796, IORCSZ at 1796..1800
        out[1792..1796].copy_from_slice(&self.ioccsz.to_le_bytes());
        out[1796..1800].copy_from_slice(&self.iorcsz.to_le_bytes());
        out
    }
}

/// Identify Namespace (NVMe Base §5.17.2.2, 4 KiB).
///
/// Per-NSID structure. The dispatcher builds one of these for every
/// attached volume on demand at Identify time.
#[derive(Debug, Clone)]
pub struct IdentifyNamespace {
    /// NSZE — Namespace Size, in LBAs. (size_bytes / lba_bytes)
    pub nsze: u64,
    /// NCAP — Namespace Capacity, in LBAs. For thin-provisioned
    /// namespaces equal to NSZE; for thick equal to NSZE. VSA is
    /// thin so NCAP = NSZE.
    pub ncap: u64,
    /// NUSE — Namespace Utilization, in LBAs. Bytes actually in
    /// use. Filled by the daemon from VolumeWriter's allocated-page
    /// counter at Identify time.
    pub nuse: u64,
    /// NSFEAT — Namespace Features bitmap. Bit 0 = thin
    /// provisioning supported.
    pub nsfeat: u8,
    /// NLBAF — Number of LBA Formats. Zero-based, so 0 means
    /// "1 format defined", which is all we emit.
    pub nlbaf: u8,
    /// FLBAS — Formatted LBA Size. Selects which of the LBA
    /// formats below is active. Zero = format index 0.
    pub flbas: u8,
    /// LBADS — log2 of the LBA size for the only format we report.
    /// Default 9 (= 512 byte LBAs); VSA's default volume is 4096
    /// byte sectors so the daemon supplies 12.
    pub lbads: u8,
    /// NGUID — Namespace Globally Unique Identifier (NVMe Base
    /// §5.17.2.2 bytes 104..120). Derived from the per-volume UUID
    /// so Linux generates a stable `/dev/disk/by-id/nvme-<wwid>`
    /// path that survives NSID renumber when sibling volumes are
    /// added or removed. NSID alone is the device-name backing
    /// (`/dev/nvmeXn<NSID>`), so without NGUID the host's only
    /// stable reference would be the filesystem UUID.
    pub nguid: [u8; 16],
}

impl IdentifyNamespace {
    /// Build from a volume size + LBA size + per-volume UUID.
    /// Validates that `lba_size_bytes` is a power of two ≥ 512.
    pub fn from_volume(
        size_bytes: u64,
        lba_size_bytes: u32,
        nguid: [u8; 16],
    ) -> Result<Self, NvmeError> {
        if !lba_size_bytes.is_power_of_two() || lba_size_bytes < 512 {
            return Err(NvmeError::FieldTooLong {
                field: "lba_size_bytes",
                got: lba_size_bytes as usize,
                max: 0,
            });
        }
        let lba_bytes = u64::from(lba_size_bytes);
        let nsze = size_bytes / lba_bytes;
        let lbads = lba_size_bytes.trailing_zeros() as u8;
        Ok(Self {
            nsze,
            ncap: nsze,
            nuse: 0,
            nsfeat: 0b0000_0001, // thin provisioning supported
            nlbaf: 0,
            flbas: 0,
            lbads,
            nguid,
        })
    }

    pub fn to_bytes(&self) -> [u8; crate::IDENTIFY_DATA_SIZE] {
        let mut out = [0u8; crate::IDENTIFY_DATA_SIZE];
        out[0..8].copy_from_slice(&self.nsze.to_le_bytes());
        out[8..16].copy_from_slice(&self.ncap.to_le_bytes());
        out[16..24].copy_from_slice(&self.nuse.to_le_bytes());
        out[24] = self.nsfeat;
        out[25] = self.nlbaf;
        out[26] = self.flbas;
        // MC, DPC, DPS, NMIC, RESCAP, FPI, DLFEAT — all zero
        // NGUID at 104..120 (16 bytes). Linux's nvme-tcp generates
        // `/dev/disk/by-id/nvme-<wwid>` from the first non-zero
        // entry in (NGUID, EUI-64); without it, the host falls back
        // to NSID-based naming that moves when sibling namespaces
        // are added or removed.
        out[104..120].copy_from_slice(&self.nguid);
        // LBAF0 at byte 128: MS(16) | LBADS(8) | RP(2 bits) | rsvd
        out[128..130].copy_from_slice(&0u16.to_le_bytes()); // MS = 0
        out[130] = self.lbads;
        out
    }
}

/// Build a CNS=0x03 Namespace Identification Descriptor list
/// (NVMe Base §5.17.2.7) carrying two descriptors: NGUID (NIDT=0x02)
/// from the per-volume UUID, then CSI (NIDT=0x04) for the NVM
/// Command Set. CSI is the minimum descriptor Linux nvme-tcp needs
/// to attach a namespace; NGUID gives the kernel a stable `wwid` so
/// `/dev/disk/by-id/nvme-<wwid>` becomes the host's stable
/// reference (otherwise the kernel falls back to NSID-derived names
/// that move when sibling namespaces are added or removed).
///
/// Descriptor layout (each entry): NIDT (1) + NIDL (1) + resv (2)
/// + NID (NIDL bytes). List is terminated by an all-zero NIDT.
pub fn namespace_id_descriptor_list(nguid: [u8; 16]) -> [u8; crate::IDENTIFY_DATA_SIZE] {
    let mut out = [0u8; crate::IDENTIFY_DATA_SIZE];
    // Descriptor 1 — NGUID at offset 0, payload at offset 4..20.
    out[0] = 0x02; // NIDT = NGUID
    out[1] = 0x10; // NIDL = 16
    out[4..20].copy_from_slice(&nguid);
    // Descriptor 2 — CSI at offset 20, payload at offset 24..25.
    out[20] = 0x04; // NIDT = CSI
    out[21] = 0x01; // NIDL = 1
    out[24] = 0x00; // NID = NVM Command Set
    // bytes 25..end remain zero — first zero NIDT terminates the list.
    out
}

/// Build the Active Namespace ID List (CNS = 0x02) payload from a
/// sorted list of attached NSIDs. The host passes a starting NSID in
/// SQE.NSID; the response is up to 1024 u32 NSIDs *greater than*
/// that one, in ascending order, zero-padded.
pub fn active_namespace_list(
    attached_sorted: &[u32],
    starting_nsid: u32,
) -> [u8; crate::IDENTIFY_DATA_SIZE] {
    let mut out = [0u8; crate::IDENTIFY_DATA_SIZE];
    let mut written = 0usize;
    for &nsid in attached_sorted.iter() {
        if nsid <= starting_nsid {
            continue;
        }
        if written >= 1024 {
            break;
        }
        let off = written * 4;
        out[off..off + 4].copy_from_slice(&nsid.to_le_bytes());
        written += 1;
    }
    out
}

/// Copy `s` into `dst`, truncating to fit, then pad the remainder
/// with `pad`. NVMe ASCII fields are space-padded; SUBNQN is
/// NUL-padded.
fn write_ascii_padded(dst: &mut [u8], s: &str, pad: u8) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    for b in dst[n..].iter_mut() {
        *b = pad;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_controller_builds_with_sn_mn() {
        let ic = IdentifyController::new(
            "0000000000000000VSA1".into(),
            "ThurVSA Volume".into(),
            "0.1.0".into(),
            1,
            3,
            "nqn.2025-10.com.metebalci:thurvsa".into(),
        )
        .expect("build");
        let bytes = ic.to_bytes();
        assert_eq!(bytes.len(), crate::IDENTIFY_DATA_SIZE);
        // SN is at offset 4..24, ASCII space-padded.
        assert_eq!(&bytes[4..24], b"0000000000000000VSA1");
        // NN at 516..520
        assert_eq!(&bytes[516..520], &3u32.to_le_bytes());
        // SUBNQN starts at 768
        assert_eq!(&bytes[768..801], b"nqn.2025-10.com.metebalci:thurvsa");
        assert_eq!(bytes[801], 0);
    }

    #[test]
    fn identify_controller_truncates_oversized_fields() {
        // FR is 8 ASCII chars on the wire. Pass a longer string and
        // confirm `to_bytes` truncates instead of erroring (the wire
        // format leaves no honest alternative; prior validation
        // surfaced as SC=0x06 on the wire — see new() rationale).
        let ic = IdentifyController::new(
            "x".repeat(40),         // SN longer than 20
            "m".repeat(60),         // MN longer than 40
            "0.1.0-alpha.1".into(), // FR = 13 bytes, exceeds 8
            1,
            1,
            "nqn.example".into(),
        )
        .expect("oversized SN/MN/FR are silently truncated, not rejected");
        let bytes = ic.to_bytes();
        assert_eq!(&bytes[4..24], &[b'x'; 20]); // SN truncated to 20
        assert_eq!(&bytes[24..64], &[b'm'; 40]); // MN truncated to 40
        assert_eq!(&bytes[64..72], b"0.1.0-al"); // FR truncated to 8
    }

    #[test]
    fn identify_controller_rejects_oversized_subnqn() {
        // SUBNQN remains validated — it's a semantic mismatch, not a
        // cosmetic truncation.
        let res =
            IdentifyController::new("sn".into(), "mn".into(), "fr".into(), 1, 1, "n".repeat(257));
        assert!(matches!(
            res,
            Err(NvmeError::FieldTooLong {
                field: "SUBNQN",
                ..
            })
        ));
    }

    #[test]
    fn identify_namespace_4k_sectors() {
        let nguid = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ];
        let id = IdentifyNamespace::from_volume(4 * 1024 * 1024, 4096, nguid).expect("build");
        // NSZE = 4 MiB / 4 KiB = 1024
        assert_eq!(id.nsze, 1024);
        assert_eq!(id.lbads, 12);
        assert_eq!(id.nguid, nguid);
        let bytes = id.to_bytes();
        assert_eq!(&bytes[0..8], &1024u64.to_le_bytes());
        assert_eq!(bytes[130], 12);
        assert_eq!(&bytes[104..120], &nguid);
    }

    #[test]
    fn ns_id_descriptor_list_carries_nguid_and_csi() {
        let nguid = [0xAB; 16];
        let list = namespace_id_descriptor_list(nguid);
        // Descriptor 1: NGUID at offset 0..20.
        assert_eq!(list[0], 0x02, "first descriptor must be NIDT=NGUID");
        assert_eq!(list[1], 0x10, "NGUID NIDL must be 16");
        assert_eq!(&list[4..20], &nguid);
        // Descriptor 2: CSI at offset 20..25.
        assert_eq!(list[20], 0x04, "second descriptor must be NIDT=CSI");
        assert_eq!(list[21], 0x01, "CSI NIDL must be 1");
        assert_eq!(list[24], 0x00, "NVM Command Set");
        // List terminates with a zero NIDT in the next slot.
        assert_eq!(list[25], 0x00, "list terminator");
    }

    #[test]
    fn active_namespace_list_skips_le_start() {
        let list = active_namespace_list(&[1, 2, 5, 7], 2);
        // First entry should be 5 (greater than 2).
        assert_eq!(&list[0..4], &5u32.to_le_bytes());
        assert_eq!(&list[4..8], &7u32.to_le_bytes());
        assert_eq!(&list[8..12], &0u32.to_le_bytes());
    }
}
