// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Log Page builders (NVMe Base §5.16).
//!
//! Hosts use Get Log Page (Admin opcode 0x02) to read controller and
//! namespace health / telemetry / firmware data. The structures
//! returned are fixed-layout binary blobs; this module emits the
//! ones a fabrics target actually has to populate to keep Linux
//! happy:
//!
//! - LID 0x01 — Error Information (64 bytes per entry × N entries)
//! - LID 0x02 — SMART / Health Information (512 bytes)
//! - LID 0x03 — Firmware Slot Information (512 bytes)
//! - LID 0x04 — Changed Namespace List (4096 bytes) — the namespaces
//!   that changed since the host last read it, paired with the
//!   Namespace Attribute Changed AER
//!
//! Everything else (Commands Supported and Effects, ANA, Sanitize
//! Status, Endurance Group, ...) is optional per spec and currently
//! returned as Invalid Field by the dispatcher.

/// SMART / Health Information log (NVMe Base §5.16.1.3). 512 bytes.
pub const SMART_HEALTH_LEN: usize = 512;
/// Error Information log entry (NVMe Base §5.16.1.2). 64 bytes each.
pub const ERROR_INFO_ENTRY_LEN: usize = 64;
/// Firmware Slot Information log (NVMe Base §5.16.1.4). 512 bytes.
pub const FIRMWARE_SLOT_INFO_LEN: usize = 512;
/// Changed Namespace List log (NVMe Base §5.16.1.5). 4096 bytes =
/// 1024 × u32 NSIDs.
pub const CHANGED_NAMESPACE_LIST_LEN: usize = 4096;
/// Maximum NSIDs the Changed Namespace List can carry before it
/// overflows into the "too many to enumerate" marker.
pub const CHANGED_NAMESPACE_LIST_MAX: usize = 1024;
/// Reservation Notification log page (NVMe NVM Command Set). One
/// 64-byte entry per Get Log Page.
pub const RESERVATION_NOTIFICATION_LEN: usize = 64;

/// Log Page IDs hosts query against an NVMe-oF controller.
pub mod lid {
    pub const ERROR_INFO: u8 = 0x01;
    pub const SMART_HEALTH: u8 = 0x02;
    pub const FIRMWARE_SLOT: u8 = 0x03;
    /// Changed Namespace List (NVMe Base §5.16.1.5). Lists the NSIDs
    /// whose attributes changed since the host last read it; paired
    /// with the Namespace Attribute Changed asynchronous event.
    pub const CHANGED_NAMESPACE_LIST: u8 = 0x04;
    /// Discovery Log Page (NVMe-oF §5.16.1.23). A Discovery
    /// controller answers this with the list of subsystems a host can
    /// reach; `nvme discover` / `nvme connect-all` read it.
    pub const DISCOVERY: u8 = 0x70;
    /// Reservation Notification (NVMe NVM Command Set). Carries the
    /// most-recent reservation event for the host to consume.
    pub const RESERVATION_NOTIFICATION: u8 = 0x80;
}

/// Reservation Notification Log Page Type (byte 8 of the LID 0x80
/// entry). 0 = no notification available; the other three name the
/// reservation event class.
pub mod resv_notif_type {
    pub const EMPTY: u8 = 0;
    pub const REGISTRATION_PREEMPTED: u8 = 1;
    pub const RESERVATION_RELEASED: u8 = 2;
    pub const RESERVATION_PREEMPTED: u8 = 3;
}

/// Build a SMART / Health Information page.
///
/// A software target has no real telemetry; the only field hosts
/// reliably inspect is Composite Temperature. We return a constant
/// 300 K (27 °C) so monitoring dashboards don't show "unknown" or
/// trigger thermal alarms. Critical Warning and Available Spare
/// stay zero — there's nothing wrong and no spare blocks model
/// applies to a software backend.
pub fn smart_health() -> [u8; SMART_HEALTH_LEN] {
    let mut buf = [0u8; SMART_HEALTH_LEN];
    // Composite Temperature in Kelvin at bytes 1..3.
    let temp_k: u16 = 300;
    buf[1..3].copy_from_slice(&temp_k.to_le_bytes());
    // Available Spare = 100 (percent), threshold = 10. Lets hosts'
    // monitoring scripts compute a safe-headroom value.
    buf[3] = 100;
    buf[4] = 10;
    buf
}

/// Build a single Error Information log entry — all zeros (no error
/// to report). ELPE in Identify Controller is 0 by default, so the
/// host only ever asks for one entry.
pub fn error_info_zero_entry() -> [u8; ERROR_INFO_ENTRY_LEN] {
    [0u8; ERROR_INFO_ENTRY_LEN]
}

/// Build a Firmware Slot Information log.
///
/// - AFI (byte 0): bits 2:0 = active slot, bits 6:4 = next active
///   slot. We populate slot 1 only, with current = next = 1.
/// - FRS1 (bytes 8..16): 8 ASCII bytes, space-padded, of the active
///   firmware revision. Caller supplies the revision string;
///   truncated to 8 bytes if longer.
pub fn firmware_slot_info(active_revision: &str) -> [u8; FIRMWARE_SLOT_INFO_LEN] {
    let mut buf = [0u8; FIRMWARE_SLOT_INFO_LEN];
    // AFI: active=1 (bits 2:0), next active=1 (bits 6:4).
    buf[0] = 0b0001_0001;
    // FRS1 at bytes 8..16, ASCII, space-padded.
    let revision = active_revision.as_bytes();
    let n = revision.len().min(8);
    buf[8..8 + n].copy_from_slice(&revision[..n]);
    for b in buf[8 + n..16].iter_mut() {
        *b = b' ';
    }
    buf
}

/// Build a Reservation Notification log page (LID 0x80, 64 bytes).
///
/// Get Log Page LID 0x80 returns the single oldest unconsumed
/// notification for the host:
///
/// - bytes 0..8  — Log Page Count (u64 LE). A controller-global,
///   monotonically increasing identifier; 0 means "no notification"
///   (the host treats type 0 / count 0 as an empty page).
/// - byte  8     — Reservation Notification Log Page Type (see
///   [`resv_notif_type`]).
/// - byte  9     — Number of Available Log Pages: how many *more*
///   notifications remain queued for the host *after* this one.
/// - bytes 12..16 — Namespace ID (u32 LE) the event applies to.
///
/// All other bytes are reserved / zero. An empty page (no event
/// queued) is the all-zero buffer: build it with
/// `reservation_notification(0, resv_notif_type::EMPTY, 0, 0)`.
pub fn reservation_notification(
    log_page_count: u64,
    notification_type: u8,
    num_available: u8,
    nsid: u32,
) -> [u8; RESERVATION_NOTIFICATION_LEN] {
    let mut buf = [0u8; RESERVATION_NOTIFICATION_LEN];
    buf[0..8].copy_from_slice(&log_page_count.to_le_bytes());
    buf[8] = notification_type;
    buf[9] = num_available;
    buf[12..16].copy_from_slice(&nsid.to_le_bytes());
    buf
}

/// Build a Changed Namespace List log page (LID 0x04, 4096 bytes).
///
/// The page is a list of up to [`CHANGED_NAMESPACE_LIST_MAX`] u32
/// NSIDs (little-endian), zero-padded. NSID 0 is reserved, so a zero
/// entry terminates the list — an empty list is the all-zero buffer.
///
/// If more than [`CHANGED_NAMESPACE_LIST_MAX`] namespaces changed the
/// list overflows: the first entry is set to `0xFFFFFFFF` and the rest
/// are zero, telling the host "too many to enumerate, re-scan all
/// namespaces" (NVMe Base §5.16.1.5). `nsids` should be sorted and
/// de-duplicated by the caller; this builder copies them verbatim.
pub fn changed_namespace_list(nsids: &[u32]) -> [u8; CHANGED_NAMESPACE_LIST_LEN] {
    let mut buf = [0u8; CHANGED_NAMESPACE_LIST_LEN];
    if nsids.len() > CHANGED_NAMESPACE_LIST_MAX {
        buf[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        return buf;
    }
    for (i, nsid) in nsids.iter().enumerate() {
        let off = i * 4;
        buf[off..off + 4].copy_from_slice(&nsid.to_le_bytes());
    }
    buf
}

// ===================== Discovery Log Page (LID 0x70) =================

/// Discovery Log Page header (NVMe-oF §5.16.1.23). Fixed 1024 bytes,
/// followed by `numrec` × [`DISCOVERY_LOG_ENTRY_LEN`]-byte entries.
pub const DISCOVERY_LOG_HEADER_LEN: usize = 1024;
/// One Discovery Log Page entry (NVMe-oF §5.16.1.23 Figure). 1024 bytes.
pub const DISCOVERY_LOG_ENTRY_LEN: usize = 1024;

/// Discovery Log entry `TRTYPE` (transport type, byte 0).
pub mod disc_trtype {
    /// TCP transport (NVMe/TCP).
    pub const TCP: u8 = 3;
}

/// Discovery Log entry `ADRFAM` (address family, byte 1).
pub mod disc_adrfam {
    pub const IPV4: u8 = 1;
    pub const IPV6: u8 = 2;
}

/// Discovery Log entry `SUBTYPE` (subsystem type, byte 2).
pub mod disc_subtype {
    /// Referral to another Discovery service.
    pub const DISCOVERY: u8 = 1;
    /// An NVMe subsystem (one that exposes namespaces).
    pub const NVME: u8 = 2;
}

/// Discovery Log entry `TREQ` (transport requirements, byte 3, bits
/// 1:0). Tells the host whether the referenced subsystem requires a
/// secure (TLS) channel.
pub mod disc_treq {
    pub const NOT_SPECIFIED: u8 = 0;
    pub const REQUIRED: u8 = 1;
    pub const NOT_REQUIRED: u8 = 2;
}

/// Discovery Log entry `TSAS.SECTYPE` for the TCP transport (byte 768).
pub mod disc_sectype {
    /// No security (plain TCP).
    pub const NONE: u8 = 0;
    /// TLS 1.3.
    pub const TLS13: u8 = 2;
}

/// One subsystem the Discovery controller refers a host to. Resolved
/// fields only; [`DiscoveryLogEntry::to_bytes`] lays them out at the
/// NVMe-oF wire offsets.
#[derive(Debug, Clone)]
pub struct DiscoveryLogEntry {
    /// Address family of `traddr` ([`disc_adrfam`]).
    pub adrfam: u8,
    /// Subsystem type ([`disc_subtype`]) — `NVME` for a namespace-
    /// bearing subsystem.
    pub subtype: u8,
    /// Transport requirements ([`disc_treq`]).
    pub treq: u8,
    /// Port identifier (cosmetic; the host keys on traddr/trsvcid).
    pub port_id: u16,
    /// Controller ID the host should request at Connect. 0xFFFF =
    /// dynamic ("any").
    pub cntlid: u16,
    /// Minimum admin submission queue size the host must allocate.
    pub asqsz: u16,
    /// Transport service identifier — the TCP port, as an ASCII string
    /// (e.g. "4420").
    pub trsvcid: String,
    /// The referenced subsystem's NQN.
    pub subnqn: String,
    /// Transport address — the IP, as an ASCII string (e.g.
    /// "192.168.1.10").
    pub traddr: String,
    /// TCP TSAS security type ([`disc_sectype`]).
    pub sectype: u8,
}

impl DiscoveryLogEntry {
    /// Lay out the entry at the NVMe-oF §5.16.1.23 wire offsets
    /// (matches Linux `struct nvmf_disc_rsp_page_entry`). TRTYPE is
    /// always TCP — the TSAS union is laid out for the TCP transport.
    pub fn to_bytes(&self) -> [u8; DISCOVERY_LOG_ENTRY_LEN] {
        let mut buf = [0u8; DISCOVERY_LOG_ENTRY_LEN];
        buf[0] = disc_trtype::TCP;
        buf[1] = self.adrfam;
        buf[2] = self.subtype;
        buf[3] = self.treq;
        buf[4..6].copy_from_slice(&self.port_id.to_le_bytes());
        buf[6..8].copy_from_slice(&self.cntlid.to_le_bytes());
        buf[8..10].copy_from_slice(&self.asqsz.to_le_bytes());
        // TRSVCID (transport service id) — ASCII, 32-byte field at 32.
        write_ascii(&mut buf[32..64], &self.trsvcid);
        // SUBNQN — ASCII, 256-byte field at 256.
        write_ascii(&mut buf[256..512], &self.subnqn);
        // TRADDR — ASCII, 256-byte field at 512.
        write_ascii(&mut buf[512..768], &self.traddr);
        // TSAS (256 bytes at 768). For TCP only byte 0 (SECTYPE) is
        // defined.
        buf[768] = self.sectype;
        buf
    }
}

/// Build a Discovery Log Page: a [`DISCOVERY_LOG_HEADER_LEN`]-byte
/// header (GENCTR @0, NUMREC @8, RECFMT @16) followed by each entry's
/// 1024-byte image.
///
/// `genctr` is the Generation Counter the host echoes between its
/// two-phase read (header to learn NUMREC, then the full page). It
/// MUST stay stable across those reads or the host retries — callers
/// pass a constant (the content is fixed at boot).
pub fn discovery_log_page(genctr: u64, entries: &[DiscoveryLogEntry]) -> Vec<u8> {
    let mut buf = vec![0u8; DISCOVERY_LOG_HEADER_LEN + entries.len() * DISCOVERY_LOG_ENTRY_LEN];
    buf[0..8].copy_from_slice(&genctr.to_le_bytes());
    buf[8..16].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    // RECFMT at 16..18 stays 0 (the only defined record format).
    for (i, entry) in entries.iter().enumerate() {
        let off = DISCOVERY_LOG_HEADER_LEN + i * DISCOVERY_LOG_ENTRY_LEN;
        buf[off..off + DISCOVERY_LOG_ENTRY_LEN].copy_from_slice(&entry.to_bytes());
    }
    buf
}

/// Copy `s` into `dst` as ASCII, truncating to fit, NUL-padding the
/// rest. Discovery Log string fields (TRSVCID / SUBNQN / TRADDR) are
/// NUL-padded ASCII.
fn write_ascii(dst: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    for b in dst[n..].iter_mut() {
        *b = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_health_carries_temperature() {
        let log = smart_health();
        assert_eq!(log.len(), SMART_HEALTH_LEN);
        let temp = u16::from_le_bytes([log[1], log[2]]);
        assert_eq!(temp, 300);
        assert_eq!(log[3], 100);
        assert_eq!(log[4], 10);
    }

    #[test]
    fn firmware_slot_info_pads_revision_to_8_bytes() {
        let log = firmware_slot_info("0.1.0");
        assert_eq!(log.len(), FIRMWARE_SLOT_INFO_LEN);
        assert_eq!(log[0], 0b0001_0001);
        assert_eq!(&log[8..16], b"0.1.0   ");
    }

    #[test]
    fn firmware_slot_info_truncates_long_revision() {
        let log = firmware_slot_info("0.1.0-alpha.1+x");
        // Truncated to 8 bytes.
        assert_eq!(&log[8..16], b"0.1.0-al");
    }

    #[test]
    fn reservation_notification_layout() {
        let log = reservation_notification(
            0x0102_0304_0506_0708,
            resv_notif_type::RESERVATION_PREEMPTED,
            2,
            0x0000_002A,
        );
        assert_eq!(log.len(), RESERVATION_NOTIFICATION_LEN);
        // Log Page Count, u64 LE at 0..8.
        assert_eq!(
            u64::from_le_bytes(log[0..8].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        // Type at byte 8, available count at byte 9.
        assert_eq!(log[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(log[9], 2);
        // bytes 10..12 reserved / zero.
        assert_eq!(&log[10..12], &[0, 0]);
        // NSID, u32 LE at 12..16.
        assert_eq!(u32::from_le_bytes(log[12..16].try_into().unwrap()), 0x2A);
        // Tail reserved / zero.
        assert!(log[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn reservation_notification_empty_page_is_all_zero() {
        let log = reservation_notification(0, resv_notif_type::EMPTY, 0, 0);
        assert!(log.iter().all(|&b| b == 0));
    }

    #[test]
    fn changed_namespace_list_layout() {
        let log = changed_namespace_list(&[1, 7, 0x0000_002A]);
        assert_eq!(log.len(), CHANGED_NAMESPACE_LIST_LEN);
        assert_eq!(u32::from_le_bytes(log[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(log[4..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(log[8..12].try_into().unwrap()), 0x2A);
        // Remaining entries are zero (list terminator).
        assert!(log[12..].iter().all(|&b| b == 0));
    }

    #[test]
    fn changed_namespace_list_empty_is_all_zero() {
        let log = changed_namespace_list(&[]);
        assert!(log.iter().all(|&b| b == 0));
    }

    #[test]
    fn changed_namespace_list_overflow_marks_first_dword() {
        let many: Vec<u32> = (1..=(CHANGED_NAMESPACE_LIST_MAX as u32 + 1)).collect();
        let log = changed_namespace_list(&many);
        // First dword = 0xFFFFFFFF ("too many, re-scan all").
        assert_eq!(
            u32::from_le_bytes(log[0..4].try_into().unwrap()),
            0xFFFF_FFFF
        );
        // Everything after the marker is zero.
        assert!(log[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn changed_namespace_list_exactly_max_enumerates() {
        let exact: Vec<u32> = (1..=(CHANGED_NAMESPACE_LIST_MAX as u32)).collect();
        let log = changed_namespace_list(&exact);
        // Not an overflow — last entry is enumerated, not the marker.
        assert_eq!(u32::from_le_bytes(log[0..4].try_into().unwrap()), 1);
        let last = (CHANGED_NAMESPACE_LIST_MAX - 1) * 4;
        assert_eq!(
            u32::from_le_bytes(log[last..last + 4].try_into().unwrap()),
            CHANGED_NAMESPACE_LIST_MAX as u32
        );
    }

    fn sample_entry() -> DiscoveryLogEntry {
        DiscoveryLogEntry {
            adrfam: disc_adrfam::IPV4,
            subtype: disc_subtype::NVME,
            treq: disc_treq::NOT_REQUIRED,
            port_id: 1,
            cntlid: 0xFFFF,
            asqsz: 32,
            trsvcid: "4420".into(),
            subnqn: "nqn.2025-10.com.metebalci:thurvsa".into(),
            traddr: "192.168.1.10".into(),
            sectype: disc_sectype::NONE,
        }
    }

    #[test]
    fn discovery_log_entry_layout() {
        let e = sample_entry();
        let b = e.to_bytes();
        assert_eq!(b.len(), DISCOVERY_LOG_ENTRY_LEN);
        assert_eq!(b[0], disc_trtype::TCP);
        assert_eq!(b[1], disc_adrfam::IPV4);
        assert_eq!(b[2], disc_subtype::NVME);
        assert_eq!(b[3], disc_treq::NOT_REQUIRED);
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 0xFFFF);
        assert_eq!(u16::from_le_bytes([b[8], b[9]]), 32);
        // TRSVCID at 32..64, NUL-padded.
        assert_eq!(&b[32..36], b"4420");
        assert_eq!(b[36], 0);
        // SUBNQN at 256..512.
        assert_eq!(&b[256..256 + 33], b"nqn.2025-10.com.metebalci:thurvsa");
        // TRADDR at 512..768.
        assert_eq!(&b[512..512 + 12], b"192.168.1.10");
        // SECTYPE at 768.
        assert_eq!(b[768], disc_sectype::NONE);
    }

    #[test]
    fn discovery_log_page_header_and_record_count() {
        let page = discovery_log_page(0, &[sample_entry()]);
        assert_eq!(
            page.len(),
            DISCOVERY_LOG_HEADER_LEN + DISCOVERY_LOG_ENTRY_LEN
        );
        // GENCTR @0.
        assert_eq!(u64::from_le_bytes(page[0..8].try_into().unwrap()), 0);
        // NUMREC @8 = 1.
        assert_eq!(u64::from_le_bytes(page[8..16].try_into().unwrap()), 1);
        // RECFMT @16 = 0.
        assert_eq!(u16::from_le_bytes([page[16], page[17]]), 0);
        // First entry begins right after the header.
        assert_eq!(page[DISCOVERY_LOG_HEADER_LEN], disc_trtype::TCP);
    }

    #[test]
    fn discovery_log_page_empty_is_header_only() {
        let page = discovery_log_page(0, &[]);
        assert_eq!(page.len(), DISCOVERY_LOG_HEADER_LEN);
        assert_eq!(u64::from_le_bytes(page[8..16].try_into().unwrap()), 0);
    }

    #[test]
    fn discovery_log_entry_sectype_tls13() {
        let mut e = sample_entry();
        e.sectype = disc_sectype::TLS13;
        e.treq = disc_treq::REQUIRED;
        let b = e.to_bytes();
        assert_eq!(b[3], disc_treq::REQUIRED);
        assert_eq!(b[768], 2);
    }
}
