// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// READ ATTRIBUTE / WRITE ATTRIBUTE implementation for SSC-4 tape drives.
// Reference: SCSI Stream Commands (SSC-4) §7.x, MAM attribute model.
//
// The drive synthesizes a fixed set of device/medium-owned attributes
// (capacity, load count, manufacturer, serial) and persists
// host-written attributes (application metadata, incl. the barcode
// 0x0806) per cartridge so they round-trip through UNLOAD/reload — see
// issue #60.

#![allow(dead_code)]

/// Attribute identifiers (SSC-4 MAM attribute registry; a subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum AttributeId {
    RemainingCapacity = 0x0000,
    MaximumCapacity = 0x0001,
    TapeAlertFlags = 0x0002,
    LoadCount = 0x0003,
    MamSpaceRemaining = 0x0004,
    AssigningOrganization = 0x0005,
    FormattedDensityCode = 0x0006,
    /// MEDIUM MANUFACTURER (medium-type, read-only).
    Manufacturer = 0x0400,
    /// MEDIUM SERIAL NUMBER (medium-type, read-only).
    SerialNumber = 0x0401,
    ManufacturingDate = 0x0406,
    /// BARCODE (host-type, read/write). Not synthesized — absent from
    /// MAM until a host writes it, mirroring real LTO-CM. The
    /// authoritative cartridge barcode is the changer's volume tag
    /// (READ ELEMENT STATUS), set out-of-band at `cartridge create`.
    Barcode = 0x0806,
}

/// Attribute format codes — the low 2 bits of the descriptor control
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AttributeFormat {
    Binary = 0x00,
    Ascii = 0x01,
    Text = 0x02,
}

/// Read-only flag in the attribute descriptor control byte (bit 7).
const READONLY_BIT: u8 = 0x80;

/// Device/medium-owned MAM attribute ids: synthesized by the daemon
/// and read-only to the host. WRITE ATTRIBUTE targeting any of these
/// is rejected with INVALID FIELD IN PARAMETER LIST. These are the
/// device-statistics ids (capacity, load count) and the medium-type
/// manufacturer/serial that a real CM carries factory-written. The
/// host barcode 0x0806 is deliberately *not* here — it is an ordinary
/// host-writable attribute (see `is_host_writable_mam`).
pub fn is_device_owned_mam(id: u16) -> bool {
    matches!(id, 0x0000 | 0x0001 | 0x0003 | 0x0400 | 0x0401)
}

/// Whether `id` is a host-writable MAM attribute: in an SSC-4 host
/// range (0x0800-0x0BFF standardized host, 0x1400-0x17FF vendor host)
/// and not one of the device-owned ids the VTL reserves.
pub fn is_host_writable_mam(id: u16) -> bool {
    !is_device_owned_mam(id) && ((0x0800..=0x0BFF).contains(&id) || (0x1400..=0x17FF).contains(&id))
}

/// MAM info for a loaded cartridge — passed by callers to populate
/// capacity-bearing attributes from the actual manifest instead of
/// hardcoded values.
#[derive(Debug, Clone, Copy)]
pub struct CartridgeMamInfo<'a> {
    pub label: &'a str,
    /// Maximum capacity in bytes (decimal). 0 = unknown/unlimited.
    pub max_capacity_bytes: u64,
    /// Remaining capacity in bytes (decimal).
    pub remaining_capacity_bytes: u64,
}

/// Handle READ ATTRIBUTE command (0x8C).
///
/// `persisted` carries the host-written MAM attributes for the loaded
/// cartridge as `(id, format, value)` tuples in ascending-id order;
/// they are merged with the synthesized device/medium attributes.
pub fn handle_read_attribute(
    service_action: u8,
    element_address: u16,
    first_attribute: u16,
    cartridge_info: Option<CartridgeMamInfo<'_>>,
    persisted: &[(u16, u8, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    tracing::debug!(
        "READ ATTRIBUTE: SA={}, element={}, first_attr=0x{:04x}, cartridge={:?}, persisted={}",
        service_action,
        element_address,
        first_attribute,
        cartridge_info.as_ref().map(|c| c.label),
        persisted.len()
    );

    // Service action codes:
    // 0x00 = Attribute values
    // 0x05 = Supported attributes
    match service_action {
        0x00 => read_attribute_values(first_attribute, cartridge_info, persisted),
        0x05 => read_supported_attributes(persisted),
        _ => Err(format!(
            "Unsupported service action: 0x{:02x}",
            service_action
        )),
    }
}

/// Synthesized device/medium-owned attributes for a loaded cartridge,
/// as `(id, format, value)` tuples. All ids are in
/// [`is_device_owned_mam`] and so can never collide with a persisted
/// host attribute.
fn synthesized_attributes(info: &CartridgeMamInfo<'_>) -> Vec<(u16, u8, Vec<u8>)> {
    vec![
        // 0x0000 Remaining capacity in partition (decimal MB, 8-byte binary).
        (
            0x0000,
            AttributeFormat::Binary as u8,
            (info.remaining_capacity_bytes / 1_000_000)
                .to_be_bytes()
                .to_vec(),
        ),
        // 0x0001 Maximum capacity in partition (decimal MB, 8-byte binary).
        (
            0x0001,
            AttributeFormat::Binary as u8,
            (info.max_capacity_bytes / 1_000_000).to_be_bytes().to_vec(),
        ),
        // 0x0003 Load count (4-byte binary).
        (
            0x0003,
            AttributeFormat::Binary as u8,
            1u32.to_be_bytes().to_vec(),
        ),
        // 0x0400 Medium manufacturer (ASCII).
        (
            0x0400,
            AttributeFormat::Ascii as u8,
            shared_naming::VENDOR_INQUIRY.as_bytes().to_vec(),
        ),
        // 0x0401 Medium serial number (ASCII).
        (
            0x0401,
            AttributeFormat::Ascii as u8,
            format!("NVT-{}", info.label).into_bytes(),
        ),
        // 0x0806 BARCODE is deliberately not synthesized — it is a
        // host-writable attribute that stays absent from MAM until a
        // host writes it (mirroring a real LTO-CM). The cartridge
        // barcode identity is the changer's volume tag.
    ]
}

/// Service Action 0x00: Read Attribute Values.
///
/// Merges the synthesized device/medium attributes (when a cartridge
/// is loaded) with the persisted host attributes, emits them in
/// ascending-id order, honoring the `first_attribute` lower bound.
fn read_attribute_values(
    first_attribute: u16,
    cartridge_info: Option<CartridgeMamInfo<'_>>,
    persisted: &[(u16, u8, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    // 4-byte AVAILABLE DATA header (big-endian length of everything
    // that follows; filled in below).
    let mut response = vec![0u8; 4];

    let mut attrs: Vec<(u16, u8, Vec<u8>)> = Vec::new();
    if let Some(info) = &cartridge_info {
        attrs.extend(synthesized_attributes(info));
    }
    attrs.extend(persisted.iter().cloned());
    // SSC-4 requires ascending attribute-id order in the response.
    // Synthesized + persisted ids are disjoint (synthesized ids are
    // all device-owned, which WRITE rejects), so a stable sort by id
    // is unambiguous.
    attrs.sort_by_key(|(id, _, _)| *id);

    for (id, format, value) in &attrs {
        if first_attribute != 0x0000 && *id < first_attribute {
            continue;
        }
        let ctrl = if is_device_owned_mam(*id) {
            READONLY_BIT | *format
        } else {
            *format
        };
        emit_descriptor(&mut response, *id, ctrl, value);
    }

    set_available_data(&mut response);
    tracing::debug!("READ ATTRIBUTE VALUES response: {} bytes", response.len());
    Ok(response)
}

/// Service Action 0x05: Read Supported Attributes — a packed,
/// ascending list of 2-byte attribute ids (no per-entry length). The
/// device-owned ids are always advertised; persisted host ids are
/// added so a host can discover what it has written.
fn read_supported_attributes(persisted: &[(u16, u8, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut response = vec![0u8; 4]; // 4-byte AVAILABLE DATA header

    let mut ids: Vec<u16> = vec![0x0000, 0x0001, 0x0003, 0x0400, 0x0401];
    ids.extend(persisted.iter().map(|(id, _, _)| *id));
    ids.sort_unstable();
    ids.dedup();

    for id in ids {
        response.extend_from_slice(&id.to_be_bytes());
    }

    set_available_data(&mut response);
    tracing::info!(
        "READ SUPPORTED ATTRIBUTES response: {} bytes",
        response.len()
    );
    Ok(response)
}

/// Handle WRITE ATTRIBUTE command (0x8D).
///
/// Parses the parameter list into `(id, format, value)` records and
/// returns them to the caller (the SCSI dispatch handler), which
/// rejects device/medium read-only ids and persists the rest. We do
/// not persist here — the cartridge handle lives behind the drive
/// manager.
///
/// Parameter list layout (SSC-4 §7.x):
///   bytes 0..3 = parameter list length (excluding this 4-byte
///                header), big-endian
///   bytes 4..  = stream of attribute records, each:
///                  bytes 0..1 = attribute id
///                  byte  2    = control (bit 7 = read-only,
///                               bits 1..0 = format)
///                  bytes 3..4 = attribute length N
///                  bytes 5..5+N = attribute value
pub fn handle_write_attribute(data: &[u8]) -> Result<Vec<(u16, u8, Vec<u8>)>, String> {
    if data.len() < 4 {
        return Err(format!(
            "WRITE ATTRIBUTE: parameter list too short ({} bytes, expected >= 4)",
            data.len()
        ));
    }
    let payload_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    // Compare as u64: `payload_len` is host-controlled and `+ 4` would
    // wrap on a 32-bit `usize`, silently passing this bounds check.
    if payload_len as u64 + 4 > data.len() as u64 {
        return Err(format!(
            "WRITE ATTRIBUTE: parameter list claims {} bytes but only {} available",
            payload_len as u64 + 4,
            data.len()
        ));
    }
    let mut offset = 4usize;
    let end = 4 + payload_len;
    let mut records = Vec::new();
    // Each descriptor is a 5-byte header (id + control + length)
    // followed by the value; the shortest is a 5-byte, zero-length
    // record, so `<= end` admits a zero-length record at the tail.
    while offset + 5 <= end {
        let attr_id = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let format = data[offset + 2] & 0x03;
        let attr_len = u16::from_be_bytes([data[offset + 3], data[offset + 4]]) as usize;
        let value_start = offset + 5;
        if value_start as u64 + attr_len as u64 > end as u64 {
            return Err(format!(
                "WRITE ATTRIBUTE: attribute 0x{:04x} declared length {} overflows parameter list",
                attr_id, attr_len
            ));
        }
        let value = data[value_start..value_start + attr_len].to_vec();
        tracing::debug!(
            "WRITE ATTRIBUTE: parsed attribute 0x{:04x} ({} bytes)",
            attr_id,
            attr_len
        );
        records.push((attr_id, format, value));
        offset = value_start + attr_len;
    }
    tracing::debug!("WRITE ATTRIBUTE: parsed {} record(s)", records.len());
    Ok(records)
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Append one attribute descriptor: id(2) + control(1) + length(2) +
/// value (the SSC-4 5-byte descriptor header).
fn emit_descriptor(response: &mut Vec<u8>, id: u16, ctrl: u8, value: &[u8]) {
    response.extend_from_slice(&id.to_be_bytes());
    response.push(ctrl);
    response.extend_from_slice(&(value.len() as u16).to_be_bytes());
    response.extend_from_slice(value);
}

/// Stamp the 4-byte big-endian AVAILABLE DATA field (bytes 0..3) =
/// length of everything after it.
fn set_available_data(response: &mut [u8]) {
    let len = (response.len() - 4) as u32;
    response[0..4].copy_from_slice(&len.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> CartridgeMamInfo<'static> {
        CartridgeMamInfo {
            label: "TAPE001",
            max_capacity_bytes: 12_000_000_000_000, // 12 TB
            remaining_capacity_bytes: 11_500_000_000_000,
        }
    }

    /// Scan a READ ATTRIBUTE VALUES response and return `(control,
    /// value)` for `wanted_id` if present. Skips the 4-byte AVAILABLE
    /// DATA header; each descriptor is id(2) + control(1) + length(2)
    /// + value(length).
    fn find_attribute(data: &[u8], wanted_id: u16) -> Option<(u8, &[u8])> {
        let mut pos = 4;
        while pos + 5 <= data.len() {
            let id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let ctrl = data[pos + 2];
            let len = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as usize;
            let value_start = pos + 5;
            if id == wanted_id {
                return data.get(value_start..value_start + len).map(|v| (ctrl, v));
            }
            pos = value_start + len;
        }
        None
    }

    fn available_data(data: &[u8]) -> usize {
        u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize
    }

    #[test]
    fn test_read_attribute_values() {
        let data = read_attribute_values(0x0000, Some(sample_info()), &[]).unwrap();
        assert!(data.len() > 4);
        // AVAILABLE DATA is a 4-byte big-endian field = body length.
        assert_eq!(available_data(&data), data.len() - 4);
    }

    #[test]
    fn test_read_attribute_no_cartridge() {
        let data = read_attribute_values(0x0000, None, &[]).unwrap();
        // Header only, AVAILABLE DATA = 0.
        assert_eq!(data.len(), 4);
        assert_eq!(available_data(&data), 0);
        assert_eq!(data[0], 0x00);
        assert_eq!(data[1], 0x00);
    }

    #[test]
    fn test_capacity_attributes_use_real_values() {
        // 12 TB max, 0.5 TB used -> 11.5 TB remaining.
        let info = CartridgeMamInfo {
            label: "TAPE001",
            max_capacity_bytes: 12_000_000_000_000,
            remaining_capacity_bytes: 11_500_000_000_000,
        };
        let data = read_attribute_values(0x0000, Some(info), &[]).unwrap();

        let (max_ctrl, max_val) = find_attribute(&data, 0x0001).expect("MaximumCapacity present");
        assert_eq!(u64::from_be_bytes(max_val.try_into().unwrap()), 12_000_000); // MB
        // Capacity is device-owned -> read-only bit set, binary format.
        assert_eq!(max_ctrl, READONLY_BIT | AttributeFormat::Binary as u8);

        let (_, rem_val) = find_attribute(&data, 0x0000).expect("RemainingCapacity present");
        assert_eq!(u64::from_be_bytes(rem_val.try_into().unwrap()), 11_500_000); // MB
    }

    #[test]
    fn test_manufacturer_serial_realigned_to_medium_ids() {
        let data = read_attribute_values(0x0000, Some(sample_info()), &[]).unwrap();
        // Manufacturer/serial live at the medium ids 0x0400/0x0401,
        // not the application ids 0x0800/0x0801.
        assert!(find_attribute(&data, 0x0400).is_some());
        assert!(find_attribute(&data, 0x0401).is_some());
        assert!(find_attribute(&data, 0x0800).is_none());
        assert!(find_attribute(&data, 0x0801).is_none());
    }

    #[test]
    fn test_barcode_not_synthesized_but_writable() {
        // No host write -> 0x0806 absent from the response (mirrors a
        // blank LTO-CM; the barcode lives in READ ELEMENT STATUS).
        let data = read_attribute_values(0x0000, Some(sample_info()), &[]).unwrap();
        assert!(find_attribute(&data, 0x0806).is_none());

        // Once a host writes it, it appears with the read-only bit
        // clear (writable).
        let persisted = vec![(0x0806u16, AttributeFormat::Ascii as u8, b"TAPE001".to_vec())];
        let data = read_attribute_values(0x0000, Some(sample_info()), &persisted).unwrap();
        let (ctrl, val) = find_attribute(&data, 0x0806).expect("written barcode present");
        assert_eq!(val, b"TAPE001");
        assert_eq!(ctrl & READONLY_BIT, 0);
    }

    #[test]
    fn test_read_attribute_merges_persisted_ascending() {
        // A host application attribute (0x0801 application name).
        let persisted = vec![(0x0801u16, AttributeFormat::Ascii as u8, b"Bareos".to_vec())];
        let data = read_attribute_values(0x0000, Some(sample_info()), &persisted).unwrap();

        let (host_ctrl, host_val) =
            find_attribute(&data, 0x0801).expect("persisted host attr present");
        assert_eq!(host_val, b"Bareos");
        // Host attribute: read-only bit clear.
        assert_eq!(host_ctrl & READONLY_BIT, 0);
        // Synthesized medium manufacturer: read-only bit set.
        let (mfr_ctrl, _) = find_attribute(&data, 0x0400).unwrap();
        assert_eq!(mfr_ctrl & READONLY_BIT, READONLY_BIT);

        // Verify the whole descriptor stream is ascending by id.
        let mut pos = 4;
        let mut last = 0u16;
        while pos + 5 <= data.len() {
            let id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            assert!(id >= last, "ids not ascending: {id:#06x} after {last:#06x}");
            last = id;
            let len = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as usize;
            pos += 5 + len;
        }
    }

    #[test]
    fn test_first_attribute_lower_bound() {
        let data = read_attribute_values(0x0400, Some(sample_info()), &[]).unwrap();
        // Everything below 0x0400 is filtered out.
        assert!(find_attribute(&data, 0x0000).is_none());
        assert!(find_attribute(&data, 0x0003).is_none());
        assert!(find_attribute(&data, 0x0400).is_some());
    }

    #[test]
    fn test_read_supported_attributes() {
        let data = read_supported_attributes(&[]).unwrap();
        assert!(data.len() > 4);
        // Header + a packed array of 2-byte ids.
        assert_eq!(available_data(&data), data.len() - 4);
        assert_eq!((data.len() - 4) % 2, 0);
    }

    #[test]
    fn test_supported_list_includes_persisted() {
        let persisted = vec![(0x080Cu16, AttributeFormat::Text as u8, b"pool1".to_vec())];
        let data = read_supported_attributes(&persisted).unwrap();
        let mut pos = 4;
        let mut found = false;
        while pos + 2 <= data.len() {
            if u16::from_be_bytes([data[pos], data[pos + 1]]) == 0x080C {
                found = true;
            }
            pos += 2;
        }
        assert!(found, "persisted id 0x080C missing from supported list");
    }

    #[test]
    fn test_write_attribute_parse_5byte_descriptor() {
        // One descriptor: id 0x0801, format ASCII (control 0x01),
        // length 6, value "Bareos".
        let mut data = Vec::new();
        let body: Vec<u8> = {
            let mut b = Vec::new();
            b.extend_from_slice(&0x0801u16.to_be_bytes());
            b.push(0x01); // control: format = ASCII, read-only clear
            b.extend_from_slice(&6u16.to_be_bytes());
            b.extend_from_slice(b"Bareos");
            b
        };
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(&body);

        let recs = handle_write_attribute(&data).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0],
            (0x0801u16, AttributeFormat::Ascii as u8, b"Bareos".to_vec())
        );
    }

    #[test]
    fn test_write_attribute_zero_length_value() {
        // A zero-length value (delete request) at the tail must parse.
        let mut data = Vec::new();
        let body: Vec<u8> = {
            let mut b = Vec::new();
            b.extend_from_slice(&0x0801u16.to_be_bytes());
            b.push(0x01);
            b.extend_from_slice(&0u16.to_be_bytes());
            b
        };
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(&body);

        let recs = handle_write_attribute(&data).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].2.is_empty());
    }

    #[test]
    fn test_write_attribute_empty_list_is_noop() {
        let data = [0u8, 0, 0, 0]; // declared length 0
        let recs = handle_write_attribute(&data).unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn test_write_attribute_overlong_length_rejected() {
        // Declared attribute length runs past the parameter list.
        let mut data = Vec::new();
        let body: Vec<u8> = {
            let mut b = Vec::new();
            b.extend_from_slice(&0x0801u16.to_be_bytes());
            b.push(0x01);
            b.extend_from_slice(&99u16.to_be_bytes()); // lies
            b.extend_from_slice(b"xx");
            b
        };
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(&body);
        assert!(handle_write_attribute(&data).is_err());
    }

    #[test]
    fn test_ownership_predicates() {
        // Device + medium ids are read-only.
        for id in [0x0000, 0x0001, 0x0003, 0x0400, 0x0401] {
            assert!(is_device_owned_mam(id), "{id:#06x} should be device-owned");
            assert!(
                !is_host_writable_mam(id),
                "{id:#06x} should not be host-writable"
            );
        }
        // Application + vendor-host ids are host-writable. 0x0806
        // (BARCODE) is a host id too — writable, not synthesized.
        for id in [
            0x0800, 0x0801, 0x0802, 0x0806, 0x080C, 0x0BFF, 0x1400, 0x17FF,
        ] {
            assert!(
                is_host_writable_mam(id),
                "{id:#06x} should be host-writable"
            );
            assert!(
                !is_device_owned_mam(id),
                "{id:#06x} should not be device-owned"
            );
        }
        // Out-of-range ids are neither.
        for id in [0x0002u16, 0x0405, 0x0C00, 0x1800] {
            assert!(
                !is_host_writable_mam(id),
                "{id:#06x} should not be host-writable"
            );
        }
    }

    #[test]
    fn test_handle_read_attribute_unsupported_sa() {
        let result = handle_read_attribute(0xFF, 0, 0, Some(sample_info()), &[]);
        assert!(result.is_err());
    }
}
