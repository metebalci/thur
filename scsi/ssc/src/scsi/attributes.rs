// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// READ ATTRIBUTE implementation for SSC-2 tape drives
// Reference: SCSI Stream Commands (SSC-2) specification
//
// Complete MAM attribute implementation per spec.

#![allow(dead_code)]

/// Attribute identifiers (SSC-2 Table 164)
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
    Barcode = 0x0806, // Media barcode
    ManufacturingDate = 0x0406,
    Manufacturer = 0x0800,
    SerialNumber = 0x0801,
}

/// Attribute format codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AttributeFormat {
    Binary = 0x00,
    Ascii = 0x01,
    Text = 0x02,
}

/// MAM info for a loaded cartridge — passed by callers to populate
/// capacity-bearing attributes from the actual manifest instead of hardcoded values.
#[derive(Debug, Clone, Copy)]
pub struct CartridgeMamInfo<'a> {
    pub label: &'a str,
    /// Maximum capacity in bytes (decimal). 0 = unknown/unlimited.
    pub max_capacity_bytes: u64,
    /// Remaining capacity in bytes (decimal).
    pub remaining_capacity_bytes: u64,
}

/// Handle READ ATTRIBUTE command (0x8C)
/// Returns attribute data for the loaded cartridge
pub fn handle_read_attribute(
    service_action: u8,
    element_address: u16,
    first_attribute: u16,
    cartridge_info: Option<CartridgeMamInfo<'_>>,
) -> Result<Vec<u8>, String> {
    tracing::debug!(
        "READ ATTRIBUTE: SA={}, element={}, first_attr=0x{:04x}, cartridge={:?}",
        service_action,
        element_address,
        first_attribute,
        cartridge_info.as_ref().map(|c| c.label)
    );

    // Service action codes:
    // 0x00 = Attribute values
    // 0x01 = Attribute list
    // 0x02 = Volume list
    // 0x05 = Supported attributes

    match service_action {
        0x00 => read_attribute_values(first_attribute, cartridge_info),
        0x05 => read_supported_attributes(),
        _ => Err(format!(
            "Unsupported service action: 0x{:02x}",
            service_action
        )),
    }
}

/// Service Action 0x00: Read Attribute Values
fn read_attribute_values(
    first_attribute: u16,
    cartridge_info: Option<CartridgeMamInfo<'_>>,
) -> Result<Vec<u8>, String> {
    let mut response = Vec::new();

    // Attribute list header (4 bytes)
    response.extend_from_slice(&[0x00, 0x00]); // Available data length (filled later)
    response.extend_from_slice(&[0x00, 0x00]); // Reserved

    // If no cartridge loaded, return empty list
    let info = match cartridge_info {
        Some(i) => i,
        None => {
            // Update available data length
            let len = (response.len() - 4) as u16;
            response[0] = (len >> 8) as u8;
            response[1] = (len & 0xFF) as u8;
            return Ok(response);
        }
    };

    // Determine which attributes to return
    // If first_attribute is 0x0000, return all attributes
    // Otherwise, return attributes >= first_attribute

    let attributes_to_return = vec![
        (AttributeId::Barcode as u16, AttributeFormat::Ascii),
        (AttributeId::Manufacturer as u16, AttributeFormat::Ascii),
        (AttributeId::SerialNumber as u16, AttributeFormat::Ascii),
        (AttributeId::LoadCount as u16, AttributeFormat::Binary),
        (AttributeId::MaximumCapacity as u16, AttributeFormat::Binary),
        (
            AttributeId::RemainingCapacity as u16,
            AttributeFormat::Binary,
        ),
    ];

    for (attr_id, format) in attributes_to_return {
        if first_attribute != 0x0000 && attr_id < first_attribute {
            continue;
        }

        add_attribute(&mut response, attr_id, format, &info)?;
    }

    // Update available data length
    let len = (response.len() - 4) as u16;
    response[0] = (len >> 8) as u8;
    response[1] = (len & 0xFF) as u8;

    tracing::debug!("READ ATTRIBUTE VALUES response: {} bytes", response.len());
    Ok(response)
}

/// Service Action 0x05: Read Supported Attributes
fn read_supported_attributes() -> Result<Vec<u8>, String> {
    let mut response = Vec::new();

    // Attribute list header (4 bytes)
    response.extend_from_slice(&[0x00, 0x00]); // Available data length (filled later)
    response.extend_from_slice(&[0x00, 0x00]); // Reserved

    // List of supported attribute IDs
    let supported = vec![
        AttributeId::RemainingCapacity as u16,
        AttributeId::MaximumCapacity as u16,
        AttributeId::LoadCount as u16,
        AttributeId::Barcode as u16,
        AttributeId::Manufacturer as u16,
        AttributeId::SerialNumber as u16,
    ];

    for attr_id in supported {
        // Each supported attribute: 2 bytes attribute ID + 2 bytes length (0x0000)
        response.extend_from_slice(&attr_id.to_be_bytes());
        response.extend_from_slice(&[0x00, 0x00]); // Length = 0 for supported list
    }

    // Update available data length
    let len = (response.len() - 4) as u16;
    response[0] = (len >> 8) as u8;
    response[1] = (len & 0xFF) as u8;

    tracing::info!(
        "READ SUPPORTED ATTRIBUTES response: {} bytes",
        response.len()
    );
    Ok(response)
}

/// Handle WRITE ATTRIBUTE command (0x8D)
///
/// Parses the parameter list sent by the initiator and ack's the write. We
/// don't persist attribute values into the cartridge's manifest yet — backup
/// software primarily uses WRITE ATTRIBUTE to set host-specific metadata that
/// doesn't need to round-trip through thurvtl — but we validate the
/// parameter list shape and reject obviously malformed input.
///
/// Parameter list layout (SSC-4 §7.10):
///   bytes 0..3 = parameter list length (excluding this header), big-endian
///   bytes 4..  = stream of attribute records, each:
///                  bytes 0..1 = attribute id
///                  byte  2    = format (b/t/a)
///                  byte  3    = reserved
///                  bytes 4..5 = attribute length N
///                  bytes 6..6+N = attribute value
pub fn handle_write_attribute(data: &[u8]) -> Result<(), String> {
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
    let mut count = 0u32;
    while offset + 5 < end {
        let attr_id = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let _format = data[offset + 2];
        let attr_len = u16::from_be_bytes([data[offset + 4], data[offset + 5]]) as usize;
        if offset as u64 + 6 + attr_len as u64 > end as u64 {
            return Err(format!(
                "WRITE ATTRIBUTE: attribute 0x{:04x} declared length {} overflows parameter list",
                attr_id, attr_len
            ));
        }
        tracing::debug!(
            "WRITE ATTRIBUTE: accepting attribute 0x{:04x} ({} bytes, transient)",
            attr_id,
            attr_len
        );
        offset += 6 + attr_len;
        count += 1;
    }
    tracing::info!("WRITE ATTRIBUTE: accepted {} attribute(s)", count);
    Ok(())
}

// ============================================================================
// Helper Functions
// ============================================================================

fn add_attribute(
    response: &mut Vec<u8>,
    attr_id: u16,
    format: AttributeFormat,
    info: &CartridgeMamInfo<'_>,
) -> Result<(), String> {
    // Attribute descriptor header
    response.extend_from_slice(&attr_id.to_be_bytes()); // Attribute identifier
    response.push(format as u8); // Format
    response.push(0x00); // Reserved
    response.extend_from_slice(&[0x00, 0x00]); // Attribute length (filled later)

    let start_len = response.len();

    // Generate attribute value based on ID
    match attr_id {
        0x0806 => {
            // Barcode (up to 32 bytes ASCII)
            let barcode = format!("{:32}", info.label); // Pad to 32 bytes
            response.extend_from_slice(barcode.as_bytes());
        }
        0x0800 => {
            // Manufacturer (MAM attribute 0x0800)
            let manufacturer = shared_naming::VENDOR_INQUIRY;
            response.extend_from_slice(manufacturer.as_bytes());
        }
        0x0801 => {
            // Serial number
            let serial = format!("NVT-{}", info.label);
            response.extend_from_slice(serial.as_bytes());
        }
        0x0003 => {
            // Load count (4-byte binary)
            response.extend_from_slice(&1u32.to_be_bytes());
        }
        0x0001 => {
            // Maximum capacity (in megabytes, 8 bytes) — SSC-2 uses decimal MB
            let capacity_mb = info.max_capacity_bytes / 1_000_000;
            response.extend_from_slice(&capacity_mb.to_be_bytes());
        }
        0x0000 => {
            // Remaining capacity (in megabytes, 8 bytes) — SSC-2 uses decimal MB
            let remaining_mb = info.remaining_capacity_bytes / 1_000_000;
            response.extend_from_slice(&remaining_mb.to_be_bytes());
        }
        _ => {
            return Err(format!("Unsupported attribute: 0x{:04x}", attr_id));
        }
    }

    // Update attribute length
    let attr_len = (response.len() - start_len) as u16;
    let len_offset = start_len - 2;
    response[len_offset] = (attr_len >> 8) as u8;
    response[len_offset + 1] = (attr_len & 0xFF) as u8;

    Ok(())
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

    #[test]
    fn test_read_attribute_values() {
        let result = read_attribute_values(0x0000, Some(sample_info()));
        assert!(result.is_ok());
        let data = result.unwrap();

        // Should have header + multiple attributes
        assert!(data.len() > 4);
        let len = u16::from_be_bytes([data[0], data[1]]);
        assert_eq!(len as usize, data.len() - 4);
    }

    #[test]
    fn test_read_attribute_no_cartridge() {
        let result = read_attribute_values(0x0000, None);
        assert!(result.is_ok());
        let data = result.unwrap();

        // Should have only header
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], 0x00);
        assert_eq!(data[1], 0x00);
    }

    /// Scan a READ ATTRIBUTE response and return the value for `wanted_id` if present.
    /// Each descriptor: id(2) + format(1) + reserved(1) + length(2) + value(length).
    fn find_attribute(data: &[u8], wanted_id: u16) -> Option<&[u8]> {
        let mut pos = 4; // skip 4-byte response header
        while pos + 6 <= data.len() {
            let id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let len = u16::from_be_bytes([data[pos + 4], data[pos + 5]]) as usize;
            let value_start = pos + 6;
            if id == wanted_id {
                return data.get(value_start..value_start + len);
            }
            pos = value_start + len;
        }
        None
    }

    #[test]
    fn test_capacity_attributes_use_real_values() {
        // 12 TB max, 0.5 TB used → 11.5 TB remaining
        let info = CartridgeMamInfo {
            label: "TAPE001",
            max_capacity_bytes: 12_000_000_000_000,
            remaining_capacity_bytes: 11_500_000_000_000,
        };
        let data = read_attribute_values(0x0000, Some(info)).unwrap();

        let max_val = find_attribute(&data, 0x0001).expect("MaximumCapacity present");
        assert_eq!(u64::from_be_bytes(max_val.try_into().unwrap()), 12_000_000); // MB

        let rem_val = find_attribute(&data, 0x0000).expect("RemainingCapacity present");
        assert_eq!(u64::from_be_bytes(rem_val.try_into().unwrap()), 11_500_000); // MB
    }

    #[test]
    fn test_read_supported_attributes() {
        let result = read_supported_attributes();
        assert!(result.is_ok());
        let data = result.unwrap();

        // Header + (attribute count * 4 bytes per attr)
        assert!(data.len() > 4);
        assert_eq!(data.len() % 4, 0); // Should be multiple of 4
    }

    #[test]
    fn test_handle_read_attribute_unsupported_sa() {
        let result = handle_read_attribute(0xFF, 0, 0, Some(sample_info()));
        assert!(result.is_err());
    }
}
