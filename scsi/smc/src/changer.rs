// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Medium Changer Commands (SMC) - SCSI-2 Medium Changer Command Set
//
// This module implements SCSI Medium Changer (SMC) commands for LUN 0,
// which presents the tape library's changer/robot functionality.
//
// Implemented commands:
// - INITIALIZE ELEMENT STATUS (0x07)
// - READ ELEMENT STATUS (0xB8)
// - MOVE MEDIUM (0xA5)
// - POSITION TO ELEMENT (0x2B)

#![allow(dead_code)]

use core_mediachanger::errors::SmcError;
use core_mediachanger::{DriveInfo, Library, MailSlotInfo, SlotInfo};
use std::sync::{Arc, Mutex};

/// Element types for READ ELEMENT STATUS command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ElementType {
    AllElements = 0x00,
    MediumTransport = 0x01, // Robot arm (not physically emulated)
    Storage = 0x02,         // Cartridge slots
    ImportExport = 0x03,    // Mail slots
    DataTransfer = 0x04,    // Tape drives
}

impl ElementType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x00 => Some(Self::AllElements),
            0x01 => Some(Self::MediumTransport),
            0x02 => Some(Self::Storage),
            0x03 => Some(Self::ImportExport),
            0x04 => Some(Self::DataTransfer),
            _ => None,
        }
    }
}

/// Element address configuration — defines how element addresses
/// map to library components. SMC-3 §3.1 element-type ranges; the
/// specific base addresses below are operator-friendly conventions.
#[derive(Debug, Clone, Copy)]
pub struct ElementAddressConfig {
    pub transport_start: u16,     // Robot arm address (single element)
    pub storage_start: u16,       // First cartridge slot address
    pub storage_count: u16,       // Number of cartridge slots
    pub import_export_start: u16, // First mail slot address
    pub import_export_count: u16, // Number of mail slots
    pub data_transfer_start: u16, // First drive address
    pub data_transfer_count: u16, // Number of drives
}

impl ElementAddressConfig {
    /// Construct from the four element-type bases (set at
    /// `library init`, read out of `library.json`) and the chassis
    /// counts. Caller is responsible for range validation — that
    /// happens at `library init` time, see
    /// `core_mediachanger::validate_element_address_layout`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport_start: u16,
        storage_start: u16,
        storage_count: u16,
        import_export_start: u16,
        import_export_count: u16,
        data_transfer_start: u16,
        data_transfer_count: u16,
    ) -> Self {
        Self {
            transport_start,
            storage_start,
            storage_count,
            import_export_start,
            import_export_count,
            data_transfer_start,
            data_transfer_count,
        }
    }

    /// Determine element type from address
    pub fn element_type_from_address(&self, address: u16) -> Option<ElementType> {
        if address == self.transport_start {
            Some(ElementType::MediumTransport)
        } else if address >= self.storage_start && address < self.storage_start + self.storage_count
        {
            Some(ElementType::Storage)
        } else if address >= self.import_export_start
            && address < self.import_export_start + self.import_export_count
        {
            Some(ElementType::ImportExport)
        } else if address >= self.data_transfer_start
            && address < self.data_transfer_start + self.data_transfer_count
        {
            Some(ElementType::DataTransfer)
        } else {
            None
        }
    }

    /// Convert storage slot ID to element address
    pub fn storage_id_to_address(&self, id: u32) -> u16 {
        self.storage_start + id as u16
    }

    /// Convert element address to storage slot ID
    pub fn address_to_storage_id(&self, address: u16) -> Option<u32> {
        if address >= self.storage_start && address < self.storage_start + self.storage_count {
            Some((address - self.storage_start) as u32)
        } else {
            None
        }
    }

    /// Convert mail slot ID to element address
    pub fn mail_id_to_address(&self, id: u32) -> u16 {
        self.import_export_start + id as u16
    }

    /// Convert element address to mail slot ID
    pub fn address_to_mail_id(&self, address: u16) -> Option<u32> {
        if address >= self.import_export_start
            && address < self.import_export_start + self.import_export_count
        {
            Some((address - self.import_export_start) as u32)
        } else {
            None
        }
    }

    /// Convert drive ID to element address
    pub fn drive_id_to_address(&self, id: u32) -> u16 {
        self.data_transfer_start + id as u16
    }

    /// Convert element address to drive ID
    pub fn address_to_drive_id(&self, address: u16) -> Option<u32> {
        if address >= self.data_transfer_start
            && address < self.data_transfer_start + self.data_transfer_count
        {
            Some((address - self.data_transfer_start) as u32)
        } else {
            None
        }
    }
}

/// INITIALIZE ELEMENT STATUS (0x07)
/// In physical changers, this triggers a barcode scan.
/// In virtual library, this reloads inventory.json from disk to pick up CLI changes.
pub fn handle_initialize_element_status(
    library: &Arc<Mutex<Library>>,
    data_dir: &std::path::Path,
) -> Result<(), SmcError> {
    tracing::debug!("INITIALIZE ELEMENT STATUS - reloading inventory from disk");

    let mut lib = library
        .lock()
        .map_err(|_| SmcError::InvalidOp("library mutex poisoned"))?;
    let (occupied_slots, loaded_drives) = lib.reload_inventory(data_dir)?;

    tracing::info!(
        "Inventory reloaded: {} occupied slots, {} loaded drives",
        occupied_slots,
        loaded_drives
    );

    // Emit event for monitoring/metrics
    tracing::event!(
        tracing::Level::INFO,
        occupied_slots = occupied_slots,
        loaded_drives = loaded_drives,
        "inventory_reloaded"
    );

    Ok(())
}

/// POSITION TO ELEMENT (0x2B)
/// In physical changers, this moves the robot arm. For virtual library, it's a no-op.
pub fn handle_position_to_element(element_address: u16) -> Result<(), SmcError> {
    tracing::info!(
        "POSITION TO ELEMENT address={} (no-op for virtual library)",
        element_address
    );
    Ok(())
}

/// Per-call options for READ ELEMENT STATUS — bit fields parsed from the
/// CDB plus the bits of host-side identity we need to render DVCID and
/// Mixed extensions.
#[derive(Debug, Clone, Copy)]
pub struct ReadElementStatusOpts {
    pub voltag: bool,
    pub dvcid: bool,
    pub mixed: bool,
    pub lto_generation: u8,
}

/// Medium Type code (low nibble of byte 9 in storage / I/E / DT
/// descriptors per SMC-3). 0x00 unspecified, 0x01 data, 0x02
/// cleaning, 0x03 diagnostic, 0x04 WORM.
fn derive_medium_type(barcode: Option<&str>) -> u8 {
    let Some(bc) = barcode else { return 0x00 };
    let bytes = bc.as_bytes();
    if bytes.len() < 2 {
        return 0x01;
    }
    let suffix = &bytes[bytes.len() - 2..];
    match suffix {
        // Barcode suffix: L<digit> = LTO data tape, L<letter> = LTO WORM
        // (U=4, V=5, W=6, X=7, Y=8, Z=9). C-prefixed = cleaning.
        [b'L', b'1'..=b'9'] => 0x01, // data
        [b'L', b'U'..=b'Z'] => 0x04, // WORM
        [b'C', _] => 0x02,           // cleaning cartridge
        _ => 0x01,                   // default to data
    }
}

/// Two-byte Mixed-Media descriptor extension (vendor-specific): Media
/// Domain + Media Type. Domain 0x4C = LTO, 0x57 = LTO-WORM, 0x7F =
/// unknown. Media Type is ASCII generation digit ('7'-'9') for data or
/// letter ('X'-'Z') for WORM. The remaining 6 bytes of the 8-byte
/// extension are reserved.
fn append_mixed_extension(out: &mut Vec<u8>, barcode: Option<&str>, lto_generation: u8) {
    let mut ext = [0u8; 8];
    if let Some(bc) = barcode {
        let bytes = bc.as_bytes();
        if bytes.len() >= 2 {
            let suffix = bytes[bytes.len() - 2..].to_vec();
            match suffix[..] {
                [b'L', d] if d.is_ascii_digit() => {
                    ext[0] = 0x4C; // LTO
                    ext[1] = d; // '7' / '8' / '9'
                }
                [b'L', l] if (b'U'..=b'Z').contains(&l) => {
                    ext[0] = 0x57; // LTO WORM
                    ext[1] = l; // 'X' / 'Y' / 'Z'
                }
                [b'C', _] => {
                    ext[0] = 0x43; // LTO cleaning
                    ext[1] = b'C';
                }
                _ => {
                    ext[0] = 0x4C;
                    ext[1] = b'0' + lto_generation;
                }
            }
        }
    } else {
        ext[0] = 0x7F;
        ext[1] = 0x7F;
    }
    out.extend_from_slice(&ext);
}

const MIXED_EXT_LEN: u16 = 8;
const VOLTAG_LEN: u16 = 36;

/// Compose the DVCID extension for a data-transfer element. Returns a
/// 38-byte buffer (4-byte SMC-3 descriptor header + 34-byte ASCII
/// identifier: 8-byte vendor + 16-byte product + 10-byte serial).
fn build_dvcid(drive_id: u32, lto_generation: u8) -> [u8; 38] {
    let mut buf = [0u8; 38];
    buf[0] = 0x02; // Code Set = ASCII
    buf[1] = 0x01; // PIV=0 | ASSOC=00 (LU) | IDTYPE=1 (T10 vendor-based)
    buf[2] = 0x00;
    buf[3] = 34; // identifier length
    // 8-byte vendor — must match the drive LUN's own INQUIRY vendor
    // so an initiator cross-checking the changer's DVCID against the
    // drive sees a single, consistent identity.
    let mut vendor = [b' '; 8];
    let vbytes = shared_naming::VENDOR_INQUIRY.as_bytes();
    let vlen = vbytes.len().min(8);
    vendor[..vlen].copy_from_slice(&vbytes[..vlen]);
    buf[4..4 + 8].copy_from_slice(&vendor);
    let product = format!("Ultrium {}-SCSI", lto_generation);
    let mut prod_padded = [b' '; 16];
    let plen = product.len().min(16);
    prod_padded[..plen].copy_from_slice(&product.as_bytes()[..plen]);
    buf[12..12 + 16].copy_from_slice(&prod_padded);
    // Serial pinned to 10 bytes (the 34-byte identifier reserves
    // 8 vendor + 16 product + 10 serial). Keep the format compact so
    // the drive id stays visible.
    let serial = format!("TVL-DRV{:03}", drive_id);
    let mut ser_padded = [b' '; 10];
    let slen = serial.len().min(10);
    ser_padded[..slen].copy_from_slice(&serial.as_bytes()[..slen]);
    buf[28..28 + 10].copy_from_slice(&ser_padded);
    buf
}

const DVCID_LEN: u16 = 38;

/// Per-element-type descriptor length (excluding any common prefix).
/// Base = 12 bytes (SMC-3); voltag adds 36; DVCID adds 38 on data
/// transfer elements only; Mixed adds 8 (vendor-specific) on every type.
fn descriptor_length(et: ElementType, opts: &ReadElementStatusOpts) -> u16 {
    let mut len: u16 = 12;
    if opts.voltag {
        len += VOLTAG_LEN;
    }
    if opts.dvcid && et == ElementType::DataTransfer {
        len += DVCID_LEN;
    }
    if opts.mixed {
        len += MIXED_EXT_LEN;
    }
    len
}

/// READ ELEMENT STATUS (0xB8)
/// Returns the status of library elements (slots, mail slots, drives).
/// When `partition_filter` is `Some(name)` only elements that belong
/// to the named logical partition are emitted; everything else is
/// silently dropped from the response. Robot-arm element is always
/// emitted (it's shared infrastructure, not partitioned).
pub fn handle_read_element_status(
    library: &Library,
    config: &ElementAddressConfig,
    element_type: ElementType,
    start_address: u16,
    count: u16,
    opts: &ReadElementStatusOpts,
    partition_filter: Option<&str>,
) -> Result<Vec<u8>, SmcError> {
    tracing::info!(
        "READ ELEMENT STATUS: type={:?} start={} count={} voltag={} dvcid={} mixed={} partition={:?}",
        element_type,
        start_address,
        count,
        opts.voltag,
        opts.dvcid,
        opts.mixed,
        partition_filter
    );

    let mut response = Vec::new();

    // Element Status Data header (8 bytes)
    response.extend_from_slice(&start_address.to_be_bytes()); // First element address
    response.extend_from_slice(&count.to_be_bytes()); // Number of elements
    response.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // Reserved + byte count placeholder

    match element_type {
        ElementType::AllElements => {
            append_storage_elements(
                &mut response,
                library,
                config,
                0,
                config.storage_count,
                opts,
                partition_filter,
            );
            append_import_export_elements(
                &mut response,
                library,
                config,
                0,
                config.import_export_count,
                opts,
                partition_filter,
            );
            append_data_transfer_elements(
                &mut response,
                library,
                config,
                0,
                config.data_transfer_count,
                opts,
                partition_filter,
            );
        }
        ElementType::MediumTransport => {
            append_transport_element(&mut response, config, opts);
        }
        ElementType::Storage => {
            let start_id = config.address_to_storage_id(start_address).unwrap_or(0);
            append_storage_elements(
                &mut response,
                library,
                config,
                start_id as u16,
                count,
                opts,
                partition_filter,
            );
        }
        ElementType::ImportExport => {
            let start_id = config.address_to_mail_id(start_address).unwrap_or(0);
            append_import_export_elements(
                &mut response,
                library,
                config,
                start_id as u16,
                count,
                opts,
                partition_filter,
            );
        }
        ElementType::DataTransfer => {
            let start_id = config.address_to_drive_id(start_address).unwrap_or(0);
            append_data_transfer_elements(
                &mut response,
                library,
                config,
                start_id as u16,
                count,
                opts,
                partition_filter,
            );
        }
    }

    let byte_count = (response.len() - 8) as u32;
    response[4..8].copy_from_slice(&byte_count.to_be_bytes());

    tracing::debug!("READ ELEMENT STATUS response: {} bytes", response.len());
    Ok(response)
}

/// Append transport element descriptor (robot arm)
fn append_transport_element(
    response: &mut Vec<u8>,
    config: &ElementAddressConfig,
    opts: &ReadElementStatusOpts,
) {
    let descriptor_len = descriptor_length(ElementType::MediumTransport, opts);

    // Element type page header (8 bytes)
    response.push(0x01); // Element type code: Medium Transport
    response.push(0x00);
    response.extend_from_slice(&descriptor_len.to_be_bytes());
    response.push(0x00);
    response.push(0x00);
    response.extend_from_slice(&descriptor_len.to_be_bytes()); // byte count = one descriptor

    response.extend_from_slice(&config.transport_start.to_be_bytes()); // 0-1
    response.push(0x00); // 2: flags (Full=0, Access=0 — robot doesn't store cartridges)
    response.push(0x00); // 3 reserved
    response.extend_from_slice(&[0x00, 0x00]); // 4-5 ASC/ASCQ
    response.extend_from_slice(&[0x00, 0x00, 0x00]); // 6-8 reserved
    response.push(0x00); // 9 SVALID=0
    response.extend_from_slice(&[0x00, 0x00]); // 10-11 source address (none)

    if opts.voltag {
        append_volume_tag(response, None);
    }
    if opts.mixed {
        append_mixed_extension(response, None, opts.lto_generation);
    }
}

/// Append storage element descriptors (cartridge slots)
fn append_storage_elements(
    response: &mut Vec<u8>,
    library: &Library,
    config: &ElementAddressConfig,
    start_id: u16,
    count: u16,
    opts: &ReadElementStatusOpts,
    partition_filter: Option<&str>,
) {
    let descriptor_len = descriptor_length(ElementType::Storage, opts);

    response.push(0x02); // Element type code: Storage
    response.push(0x00);
    response.extend_from_slice(&descriptor_len.to_be_bytes());
    response.push(0x00);

    let byte_count_offset = response.len();
    response.extend_from_slice(&[0x00, 0x00, 0x00]);

    let descriptors_start = response.len();
    let slots = library.storage_slots();
    for i in start_id..(start_id + count).min(slots.len() as u16) {
        if let Some(slot) = slots.get(i as usize) {
            if let Some(part) = partition_filter
                && library.partition_for_storage_slot(slot.id) != Some(part)
            {
                continue;
            }
            append_storage_descriptor(response, config, slot, opts);
        }
    }

    let byte_count = (response.len() - descriptors_start) as u32;
    response[byte_count_offset..byte_count_offset + 3]
        .copy_from_slice(&byte_count.to_be_bytes()[1..4]);
}

/// Append single storage element descriptor
fn append_storage_descriptor(
    response: &mut Vec<u8>,
    config: &ElementAddressConfig,
    slot: &SlotInfo,
    opts: &ReadElementStatusOpts,
) {
    let address = config.storage_id_to_address(slot.id);
    response.extend_from_slice(&address.to_be_bytes()); // 0-1

    // 2: flags. Bit 0=Full, bit 2=Except, bit 3=Access. VTL slots are
    // always accessible to the robot.
    let mut flags = 0x08; // Access=1
    if slot.occupied {
        flags |= 0x01; // Full
    }
    response.push(flags);
    response.push(0x00); // 3 reserved

    response.extend_from_slice(&[0x00, 0x00]); // 4-5 ASC/ASCQ
    response.extend_from_slice(&[0x00, 0x00, 0x00]); // 6-8 reserved

    // 9: SVALID(7) | INVERT(6) | ED(4) | low nibble = Medium Type
    let medium_type = if slot.occupied {
        derive_medium_type(slot.barcode.as_deref())
    } else {
        0x00
    };
    response.push(medium_type & 0x0F);
    response.extend_from_slice(&[0x00, 0x00]); // 10-11 source storage element

    if opts.voltag {
        append_volume_tag(response, slot.barcode.as_deref());
    }
    if opts.mixed {
        append_mixed_extension(response, slot.barcode.as_deref(), opts.lto_generation);
    }
}

/// Append import/export element descriptors (mail slots)
fn append_import_export_elements(
    response: &mut Vec<u8>,
    library: &Library,
    config: &ElementAddressConfig,
    start_id: u16,
    count: u16,
    opts: &ReadElementStatusOpts,
    partition_filter: Option<&str>,
) {
    let descriptor_len = descriptor_length(ElementType::ImportExport, opts);

    response.push(0x03); // Element type code: Import/Export
    response.push(0x00);
    response.extend_from_slice(&descriptor_len.to_be_bytes());
    response.push(0x00);

    let byte_count_offset = response.len();
    response.extend_from_slice(&[0x00, 0x00, 0x00]);

    let descriptors_start = response.len();
    let mail_slots = library.mail_slots();
    for i in start_id..(start_id + count).min(mail_slots.len() as u16) {
        if let Some(slot) = mail_slots.get(i as usize) {
            if let Some(part) = partition_filter
                && library.partition_for_mail_slot(slot.id) != Some(part)
            {
                continue;
            }
            append_import_export_descriptor(response, config, slot, opts);
        }
    }

    let byte_count = (response.len() - descriptors_start) as u32;
    response[byte_count_offset..byte_count_offset + 3]
        .copy_from_slice(&byte_count.to_be_bytes()[1..4]);
}

/// Append single import/export element descriptor
fn append_import_export_descriptor(
    response: &mut Vec<u8>,
    config: &ElementAddressConfig,
    slot: &MailSlotInfo,
    opts: &ReadElementStatusOpts,
) {
    let address = config.mail_id_to_address(slot.id);
    response.extend_from_slice(&address.to_be_bytes()); // 0-1

    // 2: flags. Bit 0=Full, 1=ImpExp(always 1), 3=Access, 4=ExEnab,
    // 5=InEnab, 6=CMC, 7=OIR. thurvtl is bidirectional so ExEnab and
    // InEnab are both 1.
    let mut flags = 0x02 | 0x10 | 0x20; // ImpExp + ExEnab + InEnab
    if slot.occupied {
        flags |= 0x01; // Full
    }
    if slot.accessible {
        flags |= 0x08; // Access
    }
    response.push(flags);
    response.push(0x00); // 3 reserved

    response.extend_from_slice(&[0x00, 0x00]); // 4-5 ASC/ASCQ
    response.extend_from_slice(&[0x00, 0x00, 0x00]); // 6-8 reserved

    let medium_type = if slot.occupied {
        derive_medium_type(slot.barcode.as_deref())
    } else {
        0x00
    };
    response.push(medium_type & 0x0F);
    response.extend_from_slice(&[0x00, 0x00]); // 10-11 source address

    if opts.voltag {
        append_volume_tag(response, slot.barcode.as_deref());
    }
    if opts.mixed {
        append_mixed_extension(response, slot.barcode.as_deref(), opts.lto_generation);
    }
}

/// Append data transfer element descriptors (tape drives)
fn append_data_transfer_elements(
    response: &mut Vec<u8>,
    library: &Library,
    config: &ElementAddressConfig,
    start_id: u16,
    count: u16,
    opts: &ReadElementStatusOpts,
    partition_filter: Option<&str>,
) {
    let descriptor_len = descriptor_length(ElementType::DataTransfer, opts);

    response.push(0x04); // Element type code: Data Transfer
    response.push(0x00);
    response.extend_from_slice(&descriptor_len.to_be_bytes());
    response.push(0x00);

    let byte_count_offset = response.len();
    response.extend_from_slice(&[0x00, 0x00, 0x00]);

    let descriptors_start = response.len();
    let drives = library.drives();
    for i in start_id..(start_id + count).min(drives.len() as u16) {
        if let Some(drive) = drives.get(i as usize) {
            if let Some(part) = partition_filter
                && library.partition_for_drive(drive.id) != Some(part)
            {
                continue;
            }
            append_data_transfer_descriptor(response, config, drive, opts);
        }
    }

    let byte_count = (response.len() - descriptors_start) as u32;
    response[byte_count_offset..byte_count_offset + 3]
        .copy_from_slice(&byte_count.to_be_bytes()[1..4]);
}

/// Append single data transfer element descriptor
fn append_data_transfer_descriptor(
    response: &mut Vec<u8>,
    config: &ElementAddressConfig,
    drive: &DriveInfo,
    opts: &ReadElementStatusOpts,
) {
    let address = config.drive_id_to_address(drive.id);
    response.extend_from_slice(&address.to_be_bytes()); // 0-1

    // 2: flags. Bit 0=Full, 2=Except, 3=Access. VTL drives are always
    // accessible.
    let mut flags = 0x08; // Access=1
    if drive.occupied {
        flags |= 0x01; // Full
    }
    response.push(flags);
    response.push(0x00); // 3 reserved

    response.extend_from_slice(&[0x00, 0x00]); // 4-5 ASC/ASCQ
    response.extend_from_slice(&[0x00, 0x00, 0x00]); // 6-8 reserved (IDValid=0, so SCSI bus address byte is reserved)

    // 9: SVALID(7) | INVERT(6) | ED(4) | low nibble = Medium Type
    let svalid: u8 = if drive.home_slot.is_some() {
        0x80
    } else {
        0x00
    };
    let medium_type = if drive.occupied {
        derive_medium_type(drive.barcode.as_deref())
    } else {
        0x00
    };
    response.push(svalid | (medium_type & 0x0F));

    // 10-11: source storage element address
    if let Some(src) = drive.home_slot {
        response.extend_from_slice(&src.to_be_bytes());
    } else {
        response.extend_from_slice(&[0x00, 0x00]);
    }

    if opts.voltag {
        append_volume_tag(response, drive.barcode.as_deref());
    }
    if opts.dvcid {
        let dvcid = build_dvcid(drive.id, opts.lto_generation);
        response.extend_from_slice(&dvcid);
    }
    if opts.mixed {
        append_mixed_extension(response, drive.barcode.as_deref(), opts.lto_generation);
    }
}

/// Append volume tag (barcode) to element descriptor
fn append_volume_tag(response: &mut Vec<u8>, barcode: Option<&str>) {
    // Volume tag: 32 bytes barcode + 4 bytes reserved = 36 bytes total
    let mut tag = [0x20u8; 36]; // Space-padded

    if let Some(bc) = barcode {
        let bytes = bc.as_bytes();
        let len = bytes.len().min(32);
        tag[..len].copy_from_slice(&bytes[..len]);
    }

    response.extend_from_slice(&tag);
}

/// MOVE MEDIUM (0xA5)
/// Move a cartridge between elements (slot to drive, drive to slot, etc.)
pub fn handle_move_medium(
    library: &mut Library,
    config: &ElementAddressConfig,
    _transport_address: u16, // Robot arm (ignored in virtual library)
    source_address: u16,
    destination_address: u16,
    _invert: bool, // Cartridge orientation (not applicable)
) -> Result<(), SmcError> {
    tracing::info!(
        "MOVE MEDIUM: source={} destination={}",
        source_address,
        destination_address
    );

    let source_type = config
        .element_type_from_address(source_address)
        .ok_or(SmcError::InvalidOp("invalid source address"))?;
    let dest_type = config
        .element_type_from_address(destination_address)
        .ok_or(SmcError::InvalidOp("invalid destination address"))?;

    match (source_type, dest_type) {
        (ElementType::Storage, ElementType::DataTransfer) => {
            // Load cartridge from storage slot to drive
            let storage_id = config
                .address_to_storage_id(source_address)
                .ok_or(SmcError::InvalidOp("invalid storage slot"))?;
            let drive_id = config
                .address_to_drive_id(destination_address)
                .ok_or(SmcError::InvalidOp("invalid drive"))?;
            library.load_to_drive(storage_id, drive_id)?;
        }
        (ElementType::DataTransfer, ElementType::Storage) => {
            // Unload cartridge from drive to storage slot
            let drive_id = config
                .address_to_drive_id(source_address)
                .ok_or(SmcError::InvalidOp("invalid drive"))?;
            let storage_id = config
                .address_to_storage_id(destination_address)
                .ok_or(SmcError::InvalidOp("invalid storage slot"))?;
            library.unload_from_drive(drive_id, storage_id)?;
        }
        (ElementType::Storage, ElementType::ImportExport) => {
            // Export cartridge to mail slot
            let storage_id = config
                .address_to_storage_id(source_address)
                .ok_or(SmcError::InvalidOp("invalid storage slot"))?;
            let mail_id = config
                .address_to_mail_id(destination_address)
                .ok_or(SmcError::InvalidOp("invalid mail slot"))?;
            library.export_to_mail(storage_id, mail_id)?;
        }
        (ElementType::ImportExport, ElementType::Storage) => {
            // Import cartridge from mail slot
            let mail_id = config
                .address_to_mail_id(source_address)
                .ok_or(SmcError::InvalidOp("invalid mail slot"))?;
            let storage_id = config
                .address_to_storage_id(destination_address)
                .ok_or(SmcError::InvalidOp("invalid storage slot"))?;
            library.import_from_mail(mail_id, storage_id)?;
        }
        (ElementType::Storage, ElementType::Storage) => {
            // Slot-to-slot relocation. Real changers expose this and backup
            // software occasionally uses it (e.g. mtx transfer).
            let src_id = config
                .address_to_storage_id(source_address)
                .ok_or(SmcError::InvalidOp("invalid source storage slot"))?;
            let dst_id = config
                .address_to_storage_id(destination_address)
                .ok_or(SmcError::InvalidOp("invalid destination storage slot"))?;
            library.move_cartridge(src_id, dst_id)?;
        }
        _ => {
            return Err(SmcError::InvalidOp(
                "unsupported element type combination for MOVE MEDIUM",
            ));
        }
    }

    tracing::info!("MOVE MEDIUM completed successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_address_config() {
        let config = ElementAddressConfig::new(0, 1001, 40, 101, 5, 1, 3);

        // Test transport
        assert_eq!(config.transport_start, 0);

        // Test storage addressing
        assert_eq!(config.storage_start, 1001);
        assert_eq!(config.storage_id_to_address(0), 1001);
        assert_eq!(config.storage_id_to_address(39), 1040);
        assert_eq!(config.address_to_storage_id(1001), Some(0));
        assert_eq!(config.address_to_storage_id(1040), Some(39));

        // Test mail slot addressing
        assert_eq!(config.import_export_start, 101);
        assert_eq!(config.mail_id_to_address(0), 101);
        assert_eq!(config.mail_id_to_address(4), 105);
        assert_eq!(config.address_to_mail_id(101), Some(0));
        assert_eq!(config.address_to_mail_id(105), Some(4));

        // Test drive addressing
        assert_eq!(config.data_transfer_start, 1);
        assert_eq!(config.drive_id_to_address(0), 1);
        assert_eq!(config.drive_id_to_address(2), 3);
        assert_eq!(config.address_to_drive_id(1), Some(0));
        assert_eq!(config.address_to_drive_id(3), Some(2));
    }

    #[test]
    fn test_element_type_detection() {
        let config = ElementAddressConfig::new(0, 1001, 40, 101, 5, 1, 3);

        assert_eq!(
            config.element_type_from_address(0),
            Some(ElementType::MediumTransport)
        );
        assert_eq!(
            config.element_type_from_address(1001),
            Some(ElementType::Storage)
        );
        assert_eq!(
            config.element_type_from_address(1040),
            Some(ElementType::Storage)
        );
        assert_eq!(
            config.element_type_from_address(101),
            Some(ElementType::ImportExport)
        );
        assert_eq!(
            config.element_type_from_address(105),
            Some(ElementType::ImportExport)
        );
        assert_eq!(
            config.element_type_from_address(1),
            Some(ElementType::DataTransfer)
        );
        assert_eq!(
            config.element_type_from_address(3),
            Some(ElementType::DataTransfer)
        );
        assert_eq!(config.element_type_from_address(9999), None);
    }

    #[test]
    fn medium_type_data_vs_worm_vs_cleaning() {
        assert_eq!(derive_medium_type(Some("TAPE001L8")), 0x01); // data
        assert_eq!(derive_medium_type(Some("TAPE001L7")), 0x01); // data
        assert_eq!(derive_medium_type(Some("TAPE001LY")), 0x04); // LTO-8 WORM
        assert_eq!(derive_medium_type(Some("TAPE001LX")), 0x04); // LTO-7 WORM
        assert_eq!(derive_medium_type(Some("CLN001CU")), 0x02); // cleaning
        assert_eq!(derive_medium_type(None), 0x00); // unspecified
    }

    #[test]
    fn descriptor_length_with_optional_extensions() {
        let mut opts = ReadElementStatusOpts {
            voltag: false,
            dvcid: false,
            mixed: false,
            lto_generation: 8,
        };
        assert_eq!(descriptor_length(ElementType::Storage, &opts), 12);
        opts.voltag = true;
        assert_eq!(descriptor_length(ElementType::Storage, &opts), 12 + 36);
        opts.mixed = true;
        assert_eq!(descriptor_length(ElementType::Storage, &opts), 12 + 36 + 8);
        opts.dvcid = true;
        // DVCID applies only to DataTransfer descriptors.
        assert_eq!(descriptor_length(ElementType::Storage, &opts), 12 + 36 + 8);
        assert_eq!(
            descriptor_length(ElementType::DataTransfer, &opts),
            12 + 36 + 38 + 8
        );
    }

    #[test]
    fn dvcid_carries_vendor_product_serial() {
        let dvcid = build_dvcid(2, 8);
        // SMC-3 descriptor header: 0x02 ASCII | 0x01 T10 | 0x00 | 34 length
        assert_eq!(&dvcid[..4], &[0x02, 0x01, 0x00, 34]);
        // 8-byte vendor immediately follows
        assert_eq!(&dvcid[4..4 + 8], b"MB      ");
        // 16-byte product
        assert!(dvcid[12..12 + 16].starts_with(b"Ultrium 8-SCSI"));
        // 10-byte serial includes the drive id
        assert!(dvcid[28..].starts_with(b"TVL-DRV002"));
    }

    #[test]
    fn mixed_extension_marks_lto_data_vs_worm() {
        let mut buf = Vec::new();
        append_mixed_extension(&mut buf, Some("TAPE001L8"), 8);
        assert_eq!(buf[0], 0x4C); // LTO domain
        assert_eq!(buf[1], b'8');
        let mut buf2 = Vec::new();
        append_mixed_extension(&mut buf2, Some("TAPE001LY"), 8);
        assert_eq!(buf2[0], 0x57); // LTO WORM domain
        assert_eq!(buf2[1], b'Y');
        let mut buf3 = Vec::new();
        append_mixed_extension(&mut buf3, None, 8);
        assert_eq!(buf3[0], 0x7F);
    }

    #[test]
    fn position_to_element_is_noop_success() {
        // Robot-arm "go here" — no inventory side effect on a virtual
        // library, but the SCSI surface must still return success so
        // initiators that pre-position before MOVE MEDIUM don't bail.
        assert!(handle_position_to_element(1001).is_ok());
        assert!(handle_position_to_element(0).is_ok());
        assert!(handle_position_to_element(u16::MAX).is_ok());
    }

    #[test]
    fn read_element_status_header_carries_start_and_count() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            8,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 8, 101, 0, 1, 2);
        let opts = ReadElementStatusOpts {
            voltag: false,
            dvcid: false,
            mixed: false,
            lto_generation: 8,
        };
        let resp = handle_read_element_status(
            &library,
            &cfg,
            ElementType::Storage,
            cfg.storage_start,
            8,
            &opts,
            None,
        )
        .unwrap();
        // 8-byte Element Status Data header: u16 first_addr, u16 count.
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), cfg.storage_start);
        assert_eq!(u16::from_be_bytes([resp[2], resp[3]]), 8);
    }

    #[test]
    fn read_element_status_data_transfer_with_dvcid() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            4,
            0,
            3,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 4, 101, 0, 1, 3);
        let opts = ReadElementStatusOpts {
            voltag: false,
            dvcid: true,
            mixed: false,
            lto_generation: 8,
        };
        let resp = handle_read_element_status(
            &library,
            &cfg,
            ElementType::DataTransfer,
            cfg.data_transfer_start,
            3,
            &opts,
            None,
        )
        .unwrap();
        // Per-descriptor length on DataTransfer with DVCID is 12 + 38 = 50.
        let per_page_bytes = u32::from_be_bytes([0, resp[8 + 5], resp[8 + 6], resp[8 + 7]]);
        assert_eq!(per_page_bytes, 50 * 3);
        // The vendor string should appear somewhere in the body.
        let body = &resp[8..];
        assert!(
            body.windows(8).any(|w| w == b"MB      "),
            "DVCID vendor not found in DataTransfer response"
        );
    }

    #[test]
    fn move_medium_storage_to_drive_and_back() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            8,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        // Seed slot 0 with a tape so the load has something to move.
        library.add_or_create_tape("TAPE001", "primary").unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 8, 101, 0, 1, 2);

        // Storage slot 0 (addr 1001) -> Drive 0 (addr 1).
        handle_move_medium(&mut library, &cfg, 0, 1001, 1, false).unwrap();
        assert!(
            library.drives()[0].occupied,
            "drive 0 should be loaded after slot->drive move"
        );
        assert!(
            !library.storage_slots()[0].occupied,
            "slot 0 should be empty after slot->drive move"
        );

        // Drive 0 (addr 1) -> Storage slot 0 (addr 1001).
        handle_move_medium(&mut library, &cfg, 0, 1, 1001, false).unwrap();
        assert!(!library.drives()[0].occupied);
        assert!(library.storage_slots()[0].occupied);
    }

    #[test]
    fn move_medium_storage_to_storage_relocates_barcode() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            8,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        library.add_or_create_tape("TAPE001", "primary").unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 8, 101, 0, 1, 2);

        // Slot 0 -> Slot 5 (mtx transfer pattern).
        handle_move_medium(&mut library, &cfg, 0, 1001, 1006, false).unwrap();
        assert!(!library.storage_slots()[0].occupied);
        assert!(library.storage_slots()[5].occupied);
        assert_eq!(
            library.storage_slots()[5].barcode.as_deref(),
            Some("TAPE001")
        );
    }

    #[test]
    fn move_medium_rejects_drive_to_drive() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            4,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 4, 101, 0, 1, 2);
        // Drive (addr 1) -> Drive (addr 2): not a real workflow, must
        // refuse with InvalidOp rather than corrupting state.
        let err = handle_move_medium(&mut library, &cfg, 0, 1, 2, false).unwrap_err();
        assert!(
            matches!(err, SmcError::InvalidOp(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn move_medium_rejects_invalid_address() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            4,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 4, 101, 0, 1, 2);
        // 0xFFFF lives outside every range — element_type_from_address
        // returns None and the handler must propagate InvalidOp.
        let err = handle_move_medium(&mut library, &cfg, 0, 0xFFFF, 1001, false).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn read_element_status_filters_out_of_partition_slots() {
        use core_mediachanger::{LibraryPartition, SlotRange};
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        let mut library =
            Library::initialize(&lib_root, &tapes_root, 40, 0, 3, 8, None, 0, 1001, 101, 1)
                .unwrap();
        library
            .set_partitions(vec![
                LibraryPartition {
                    name: "alpha".into(),
                    storage_slots: SlotRange { start: 0, end: 20 },
                    mail_slots: SlotRange::default(),
                    drives: vec![0, 1],
                },
                LibraryPartition {
                    name: "bravo".into(),
                    storage_slots: SlotRange { start: 20, end: 40 },
                    mail_slots: SlotRange::default(),
                    drives: vec![2],
                },
            ])
            .unwrap();
        let cfg = ElementAddressConfig::new(0, 1001, 40, 101, 0, 1, 3);
        let opts = ReadElementStatusOpts {
            voltag: false,
            dvcid: false,
            mixed: false,
            lto_generation: 8,
        };

        // Without partition filter: every storage slot descriptor
        // appears (12 bytes × 40 slots = 480 + 8 outer + 8 page hdr).
        let unfiltered = handle_read_element_status(
            &library,
            &cfg,
            ElementType::Storage,
            cfg.storage_start,
            40,
            &opts,
            None,
        )
        .unwrap();
        // Per-page byte count lives at offsets [13..16] of the type
        // page header (which sits 8 bytes into the response).
        let per_page_bytes_unfiltered =
            u32::from_be_bytes([0, unfiltered[8 + 5], unfiltered[8 + 6], unfiltered[8 + 7]]);
        assert_eq!(per_page_bytes_unfiltered, 12 * 40);

        // Bound to bravo: only the second half (slot ids 20..40)
        // should appear.
        let filtered = handle_read_element_status(
            &library,
            &cfg,
            ElementType::Storage,
            cfg.storage_start,
            40,
            &opts,
            Some("bravo"),
        )
        .unwrap();
        let per_page_bytes_filtered =
            u32::from_be_bytes([0, filtered[8 + 5], filtered[8 + 6], filtered[8 + 7]]);
        assert_eq!(per_page_bytes_filtered, 12 * 20);
    }
}
