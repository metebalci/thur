// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// SCSI Tape Data Encryption pages (SPC-4 §7.6, SSC-4 §8.5).
//
// These pages are exchanged via SECURITY PROTOCOL IN (0xA2) and
// SECURITY PROTOCOL OUT (0xB5) with SECURITY PROTOCOL = 0x20.
// SECURITY_PROTOCOL_SPECIFIC selects the page within the protocol.

use core_mediachanger::encryption::{
    ALGORITHM_CODE_AES_256_GCM, ALGORITHM_INDEX_AES_256_GCM, DecryptionMode, DriveEncryptionState,
    EncryptionMode, KEY_LEN, KeyScope,
};

/// Tape Data Encryption protocol identifier (SPC-4 Table 263).
pub const SECURITY_PROTOCOL_TAPE_DATA_ENC: u8 = 0x20;

/// Page codes inside protocol 0x20 (SPC-4 Table 264 / SSC-5 §4.2.20).
/// SP-IN and SP-OUT use the same page-code namespace but are
/// disambiguated by direction; e.g. SPSP=0x0010 means "Data Encryption
/// Capabilities" on SP IN and "Set Data Encryption" on SP OUT.
pub const PAGE_TAPE_DATA_ENC_IN_SUPPORT: u16 = 0x0000;
pub const PAGE_TAPE_DATA_ENC_OUT_SUPPORT: u16 = 0x0001;
pub const PAGE_DATA_ENCRYPTION_CAPABILITIES: u16 = 0x0010;
pub const PAGE_SUPPORTED_KEY_FORMATS: u16 = 0x0011;
pub const PAGE_DATA_ENCRYPTION_MGMT_CAPS: u16 = 0x0012;
pub const PAGE_DATA_ENCRYPTION_STATUS: u16 = 0x0020;
pub const PAGE_NEXT_BLOCK_ENCRYPTION_STATUS: u16 = 0x0021;

/// SP OUT Set Data Encryption page code (SSC-4 §8.5.3.2).
pub const PAGE_SET_DATA_ENCRYPTION: u16 = 0x0010;

/// Build the supported security protocols list (SP IN, protocol 0x00).
/// Header (8 bytes) + N protocol bytes. We support 0x00 (info) and 0x20
/// (Tape Data Encryption).
pub fn build_supported_protocols() -> Vec<u8> {
    let protocols: [u8; 2] = [0x00, SECURITY_PROTOCOL_TAPE_DATA_ENC];
    let mut out = Vec::with_capacity(8 + protocols.len());
    out.extend_from_slice(&[0u8; 6]); // reserved
    out.extend_from_slice(&(protocols.len() as u16).to_be_bytes()); // list length
    out.extend_from_slice(&protocols);
    out
}

/// SP IN protocol 0x20 / SPSP 0x0000: Tape Data Encryption In Support.
/// Lists every SP-IN page we answer, including the OUT-support page
/// (which is itself an SP-IN-direction page that enumerates SP-OUT
/// support).
pub fn build_in_support_page() -> Vec<u8> {
    let pages: [u16; 6] = [
        PAGE_TAPE_DATA_ENC_IN_SUPPORT,
        PAGE_TAPE_DATA_ENC_OUT_SUPPORT,
        PAGE_DATA_ENCRYPTION_CAPABILITIES,
        PAGE_SUPPORTED_KEY_FORMATS,
        PAGE_DATA_ENCRYPTION_STATUS,
        PAGE_NEXT_BLOCK_ENCRYPTION_STATUS,
    ];
    let body_len = pages.len() * 2;
    let mut out = Vec::with_capacity(4 + body_len);
    out.extend_from_slice(&PAGE_TAPE_DATA_ENC_IN_SUPPORT.to_be_bytes());
    out.extend_from_slice(&(body_len as u16).to_be_bytes());
    for p in pages {
        out.extend_from_slice(&p.to_be_bytes());
    }
    out
}

/// SP IN protocol 0x20 / SPSP 0x0001: Tape Data Encryption Out Support.
/// Lists the SP-OUT pages we accept.
pub fn build_out_support_page() -> Vec<u8> {
    let pages: [u16; 1] = [PAGE_SET_DATA_ENCRYPTION];
    let body_len = pages.len() * 2;
    let mut out = Vec::with_capacity(4 + body_len);
    out.extend_from_slice(&PAGE_TAPE_DATA_ENC_OUT_SUPPORT.to_be_bytes());
    out.extend_from_slice(&(body_len as u16).to_be_bytes());
    for p in pages {
        out.extend_from_slice(&p.to_be_bytes());
    }
    out
}

/// SP IN protocol 0x20 / SPSP 0x0020: Data Encryption Capabilities.
/// Reports a single algorithm descriptor for AES-256-GCM.
pub fn build_capabilities_page() -> Vec<u8> {
    // Page header (16 bytes): page code, length, control flags, reserved
    let mut out = Vec::with_capacity(16 + 24);
    out.extend_from_slice(&PAGE_DATA_ENCRYPTION_CAPABILITIES.to_be_bytes());
    // page length placeholder (filled at end)
    out.extend_from_slice(&[0u8; 2]);
    // byte 4: EXTDECC=00, CFG_P=01 (configurable encryption parameters)
    out.push(0x10);
    // byte 5: reserved
    out.push(0x00);
    // bytes 6-15: reserved
    out.extend_from_slice(&[0u8; 10]);

    // Algorithm Descriptor: 24 bytes for AES-256-GCM at index 1
    out.push(ALGORITHM_INDEX_AES_256_GCM); // byte 0: algorithm index
    out.push(0x00); // byte 1: reserved
    out.extend_from_slice(&0x0014u16.to_be_bytes()); // bytes 2-3: descriptor length = 20
    // byte 4: AVFMV=0 SDK_C=0 MAC_C=0 DELB_C=0 CAP_C=00 (no EXTERNAL
    // mode for either direction) DECRYPT_C=1 ENCRYPT_C=1. EXTERNAL
    // is for inline encryption appliances and has no analogue here;
    // SP OUT Set Data Encryption with ENCRYPTION_MODE=0x01 is also
    // rejected at the parser.
    out.push(0x03);
    // byte 5: NONCE_C=11 KADF_C=0 VCELB_C=0 AVFCLP=0
    out.push(0x0C);
    // bytes 6-7: max unauthenticated KAD bytes = 0
    out.extend_from_slice(&[0u8; 2]);
    // bytes 8-9: max authenticated KAD bytes = 0
    out.extend_from_slice(&[0u8; 2]);
    // bytes 10-11: KEY_SIZE = 32
    out.extend_from_slice(&(KEY_LEN as u16).to_be_bytes());
    // byte 12: DKAD_C=0
    out.push(0x00);
    // byte 13: reserved
    out.push(0x00);
    // bytes 14-15: max encryption KAD = 0
    out.extend_from_slice(&[0u8; 2]);
    // bytes 16-17: max authenticated KAD = 0
    out.extend_from_slice(&[0u8; 2]);
    // bytes 18-19: reserved
    out.extend_from_slice(&[0u8; 2]);
    // bytes 20-23: ALGORITHM CODE = 0x00010014 (AES-256-GCM)
    out.extend_from_slice(&ALGORITHM_CODE_AES_256_GCM.to_be_bytes());

    let body_len = (out.len() - 4) as u16;
    out[2..4].copy_from_slice(&body_len.to_be_bytes());
    out
}

/// SP IN protocol 0x20 / SPSP 0x0021: Supported Key Formats.
/// We accept plaintext keys only (KEY_FORMAT = 0x00).
pub fn build_supported_key_formats_page() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&PAGE_SUPPORTED_KEY_FORMATS.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // body length = 1
    out.push(0x00); // KEY_FORMAT = plaintext
    out
}

/// SP IN protocol 0x20 / SPSP 0x0100: Data Encryption Status.
/// Reports the drive's current encryption parameters.
pub fn build_encryption_status_page(state: Option<&DriveEncryptionState>) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&PAGE_DATA_ENCRYPTION_STATUS.to_be_bytes());
    out.extend_from_slice(&[0u8; 2]); // length placeholder

    let (scope, enc_mode, dec_mode, algo_idx) = match state {
        Some(s) => (
            s.scope as u8,
            s.mode as u8,
            s.decryption_mode as u8,
            s.algorithm_index,
        ),
        None => (
            KeyScope::Public as u8,
            EncryptionMode::Disable as u8,
            DecryptionMode::Disable as u8,
            0u8,
        ),
    };

    // byte 4: IT_NEXUS_SCOPE (bits 7:5), reserved
    out.push((scope & 0x07) << 5);
    // byte 5: ENCRYPTION_MODE
    out.push(enc_mode);
    // byte 6: DECRYPTION_MODE
    out.push(dec_mode);
    // byte 7: ALGORITHM_INDEX
    out.push(algo_idx);
    // bytes 8-11: KEY_INSTANCE_COUNTER (BE) — we don't track, send 0
    out.extend_from_slice(&[0u8; 4]);
    // byte 12: PARAMETERS_CONTROL (top bits) | VCELB | CEEMS | RDMD
    out.push(0x00);
    // byte 13: KAD_FORMAT (00 = unspecified)
    out.push(0x00);
    // bytes 14-15: ASDK_COUNT (no extra descriptors)
    out.extend_from_slice(&[0u8; 2]);
    // bytes 16-23: reserved
    out.extend_from_slice(&[0u8; 8]);

    let body_len = (out.len() - 4) as u16;
    out[2..4].copy_from_slice(&body_len.to_be_bytes());
    out
}

/// SP IN protocol 0x20 / SPSP 0x0021: Next Block Encryption Status.
/// Tells the host whether the next block to be read is encrypted, so it
/// can decide whether it needs a key. Only called with a decodable
/// head record (or at EOD): an unreadable record fails the command
/// with CHECK CONDITION instead of fabricating a plaintext status
/// (issue #110).
pub fn build_next_block_status_page(
    next_lba: u64,
    encrypted: bool,
    algorithm_index: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&PAGE_NEXT_BLOCK_ENCRYPTION_STATUS.to_be_bytes());
    out.extend_from_slice(&[0u8; 2]); // length placeholder
    // bytes 4-11: LOGICAL OBJECT NUMBER (BE) — block address
    out.extend_from_slice(&next_lba.to_be_bytes());
    // byte 12: COMPRESSION_STATUS (bits 7:4) | ENCRYPTION_STATUS (bits 3:0)
    //   ENCRYPTION_STATUS values (SSC-4 Table 41 §8.5.4):
    //     0x0 = unable to determine
    //     0x1 = logical block is encrypted, no key info known
    //     0x2 = logical block is encrypted, decryption disabled
    //     0x3 = logical block is encrypted, decryption supported
    //     0x4 = logical block is encrypted, decryption matches drive key
    //     0x5 = logical block is not encrypted
    //     0x6 = logical block is encrypted, decryption mismatch
    let encryption_status = if encrypted { 0x4 } else { 0x5 };
    out.push(encryption_status);
    // byte 13: ALGORITHM_INDEX
    out.push(algorithm_index);
    // bytes 14-15: KEY_LENGTH (BE) — 0 since we don't echo keys back
    out.extend_from_slice(&[0u8; 2]);

    let body_len = (out.len() - 4) as u16;
    out[2..4].copy_from_slice(&body_len.to_be_bytes());
    out
}

/// Result of parsing an SP OUT Set Data Encryption page.
pub enum SetDataEncryption {
    /// Set or replace the drive's encryption state.
    SetKey(DriveEncryptionState),
    /// Clear the drive's encryption state (mode=Disable, no key).
    Clear,
}

/// Parse SP OUT protocol 0x20 / SPSP 0x0010 Set Data Encryption page.
/// Returns Err(()) on malformed input — caller should reply with
/// CHECK CONDITION + INVALID FIELD IN CDB.
pub fn parse_set_data_encryption(
    data: &[u8],
) -> std::result::Result<SetDataEncryption, &'static str> {
    if data.len() < 16 {
        return Err("Set Data Encryption page too short (need at least 16 bytes)");
    }
    let page_code = u16::from_be_bytes([data[0], data[1]]);
    if page_code != PAGE_SET_DATA_ENCRYPTION {
        return Err("unexpected page code (expected 0x0010)");
    }
    let page_length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < 4 + page_length {
        return Err("Set Data Encryption page truncated");
    }

    let scope_bits = (data[4] >> 5) & 0x07;
    let scope = KeyScope::from_u8(scope_bits).map_err(|_| "invalid SCOPE field")?;
    let encryption_mode_byte = data[6];
    let decryption_mode_byte = data[7];
    let encryption_mode = EncryptionMode::from_u8(encryption_mode_byte)
        .map_err(|_| "invalid ENCRYPTION_MODE field")?;
    let decryption_mode = DecryptionMode::from_u8(decryption_mode_byte)
        .map_err(|_| "invalid DECRYPTION_MODE field")?;
    let algorithm_index = data[8];
    // byte 9: KEY_FORMAT (we accept 0x00 plaintext only)
    let key_format = data[9];
    let key_length = u16::from_be_bytes([data[12], data[13]]) as usize;

    // DISABLE with no key clears the drive state.
    if encryption_mode == EncryptionMode::Disable
        && decryption_mode == DecryptionMode::Disable
        && key_length == 0
    {
        return Ok(SetDataEncryption::Clear);
    }

    if key_format != 0x00 {
        return Err("unsupported KEY_FORMAT (only plaintext, 0x00, is supported)");
    }
    if algorithm_index != ALGORITHM_INDEX_AES_256_GCM {
        return Err("unsupported algorithm index (only AES-256-GCM, index 1)");
    }
    if key_length != KEY_LEN {
        return Err("AES-256-GCM key must be 32 bytes");
    }
    let key_start = 14usize;
    let key_end = key_start + key_length;
    if data.len() < key_end {
        return Err("Set Data Encryption page truncated before key");
    }
    let key = data[key_start..key_end].to_vec();

    // KAD descriptors follow; we accept and stash them as opaque bytes.
    // Bound the slice end at `key_end` (.max) so a crafted PAGE LENGTH
    // that declares the page ending *before* the already-validated key
    // can't produce a reversed range (start > end) and panic the
    // spawn_blocking SCSI task — a remote DoS reachable from any
    // logged-in initiator via a single SECURITY PROTOCOL OUT (issue #260).
    let kad = if data.len() > key_end {
        let kad_end = (4 + page_length).max(key_end).min(data.len());
        data[key_end..kad_end].to_vec()
    } else {
        Vec::new()
    };

    Ok(SetDataEncryption::SetKey(DriveEncryptionState {
        mode: encryption_mode,
        decryption_mode,
        scope,
        algorithm_index,
        key,
        kad,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_codes_match_spc4_ssc5_spec() {
        // SPC-4 §7.6.4 / SSC-5 §4.2.20 page-code values for protocol
        // 0x20. Pinned here so a future rename or shuffle that drifts
        // back from spec gets caught at test time. Real backup software
        // and KMIP/SKLM/ESKM key managers all rely on these.
        assert_eq!(PAGE_TAPE_DATA_ENC_IN_SUPPORT, 0x0000);
        assert_eq!(PAGE_TAPE_DATA_ENC_OUT_SUPPORT, 0x0001);
        assert_eq!(PAGE_DATA_ENCRYPTION_CAPABILITIES, 0x0010);
        assert_eq!(PAGE_SUPPORTED_KEY_FORMATS, 0x0011);
        assert_eq!(PAGE_DATA_ENCRYPTION_MGMT_CAPS, 0x0012);
        assert_eq!(PAGE_DATA_ENCRYPTION_STATUS, 0x0020);
        assert_eq!(PAGE_NEXT_BLOCK_ENCRYPTION_STATUS, 0x0021);
        // SP-OUT direction reuses 0x0010 (different namespace).
        assert_eq!(PAGE_SET_DATA_ENCRYPTION, 0x0010);
    }

    #[test]
    fn in_support_page_advertises_every_implemented_in_page() {
        let buf = build_in_support_page();
        assert_eq!(
            u16::from_be_bytes([buf[0], buf[1]]),
            PAGE_TAPE_DATA_ENC_IN_SUPPORT
        );
        let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let body = &buf[4..4 + body_len];
        let mut codes: Vec<u16> = body
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        codes.sort();
        assert_eq!(codes, vec![0x0000, 0x0001, 0x0010, 0x0011, 0x0020, 0x0021]);
    }

    #[test]
    fn supported_protocols_lists_info_and_tape_enc() {
        let buf = build_supported_protocols();
        assert_eq!(buf.len(), 10);
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 2);
        assert_eq!(buf[8], 0x00);
        assert_eq!(buf[9], SECURITY_PROTOCOL_TAPE_DATA_ENC);
    }

    #[test]
    fn capabilities_page_advertises_aes_256_gcm() {
        let buf = build_capabilities_page();
        assert_eq!(
            u16::from_be_bytes([buf[0], buf[1]]),
            PAGE_DATA_ENCRYPTION_CAPABILITIES
        );
        let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        assert_eq!(buf.len(), 4 + body_len);
        // algorithm descriptor at offset 16
        assert_eq!(buf[16], ALGORITHM_INDEX_AES_256_GCM);
        // key size at offset 16+10..12
        let key_size = u16::from_be_bytes([buf[16 + 10], buf[16 + 11]]);
        assert_eq!(key_size as usize, KEY_LEN);
        // algorithm code at offset 16+20..24
        let algo = u32::from_be_bytes([buf[16 + 20], buf[16 + 21], buf[16 + 22], buf[16 + 23]]);
        assert_eq!(algo, ALGORITHM_CODE_AES_256_GCM);
    }

    #[test]
    fn parse_set_key_roundtrip() {
        let mut page = vec![0u8; 14 + 32];
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        let body_len = (page.len() - 4) as u16;
        page[2..4].copy_from_slice(&body_len.to_be_bytes());
        page[4] = (KeyScope::Public as u8) << 5;
        page[6] = EncryptionMode::Encrypt as u8;
        page[7] = DecryptionMode::Decrypt as u8;
        page[8] = ALGORITHM_INDEX_AES_256_GCM;
        page[9] = 0x00; // plaintext key format
        page[12..14].copy_from_slice(&(KEY_LEN as u16).to_be_bytes());
        for (i, b) in page[14..14 + 32].iter_mut().enumerate() {
            *b = i as u8;
        }
        match parse_set_data_encryption(&page).expect("should parse") {
            SetDataEncryption::SetKey(state) => {
                assert_eq!(state.mode, EncryptionMode::Encrypt);
                assert_eq!(state.decryption_mode, DecryptionMode::Decrypt);
                assert_eq!(state.scope, KeyScope::Public);
                assert_eq!(state.algorithm_index, ALGORITHM_INDEX_AES_256_GCM);
                assert_eq!(state.key.len(), KEY_LEN);
                assert_eq!(state.key[0], 0);
                assert_eq!(state.key[31], 31);
            }
            SetDataEncryption::Clear => panic!("expected SetKey"),
        }
    }

    /// Issue #260: a crafted page whose declared PAGE LENGTH ends before
    /// the (validated) 32-byte key must not produce a reversed KAD slice
    /// and panic the SCSI task — a remote DoS. It parses with an empty
    /// KAD instead.
    #[test]
    fn parse_set_key_crafted_short_page_length_does_not_panic() {
        let mut page = vec![0u8; 47]; // key_end = 46, one trailing byte
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        // Crafted: PAGE LENGTH = 0 (the honest body length would be 43).
        page[2..4].copy_from_slice(&0u16.to_be_bytes());
        page[4] = (KeyScope::Public as u8) << 5;
        page[6] = EncryptionMode::Encrypt as u8;
        page[7] = DecryptionMode::Decrypt as u8;
        page[8] = ALGORITHM_INDEX_AES_256_GCM;
        page[9] = 0x00; // plaintext key format
        page[12..14].copy_from_slice(&(KEY_LEN as u16).to_be_bytes());
        for (i, b) in page[14..14 + 32].iter_mut().enumerate() {
            *b = i as u8;
        }
        match parse_set_data_encryption(&page).expect("must parse without panicking") {
            SetDataEncryption::SetKey(state) => {
                assert_eq!(state.key.len(), KEY_LEN);
                assert!(
                    state.kad.is_empty(),
                    "a short crafted page_length yields an empty KAD, not a panic"
                );
            }
            SetDataEncryption::Clear => panic!("expected SetKey"),
        }
    }

    #[test]
    fn parse_disable_clears() {
        // Real backup software sends a 16-byte page for DISABLE (no key
        // body) — page_length = 12 covers bytes 4..16. We require at
        // least 16 bytes so the KEY_LENGTH field is always present.
        let mut page = vec![0u8; 16];
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        page[2..4].copy_from_slice(&12u16.to_be_bytes());
        page[6] = EncryptionMode::Disable as u8;
        page[7] = DecryptionMode::Disable as u8;
        // KEY_LENGTH = 0 (bytes 12-13 already zero)
        match parse_set_data_encryption(&page).expect("should parse") {
            SetDataEncryption::Clear => {}
            SetDataEncryption::SetKey(_) => panic!("expected Clear"),
        }
    }

    #[test]
    fn parse_rejects_wrong_algorithm() {
        let mut page = vec![0u8; 14 + 32];
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        let body_len = (page.len() - 4) as u16;
        page[2..4].copy_from_slice(&body_len.to_be_bytes());
        page[6] = EncryptionMode::Encrypt as u8;
        page[7] = DecryptionMode::Decrypt as u8;
        page[8] = 0x99; // not AES-256-GCM
        page[12..14].copy_from_slice(&(KEY_LEN as u16).to_be_bytes());
        assert!(parse_set_data_encryption(&page).is_err());
    }

    #[test]
    fn parse_rejects_short_key() {
        let mut page = vec![0u8; 14 + 16];
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        let body_len = (page.len() - 4) as u16;
        page[2..4].copy_from_slice(&body_len.to_be_bytes());
        page[6] = EncryptionMode::Encrypt as u8;
        page[7] = DecryptionMode::Decrypt as u8;
        page[8] = ALGORITHM_INDEX_AES_256_GCM;
        page[12..14].copy_from_slice(&16u16.to_be_bytes());
        assert!(parse_set_data_encryption(&page).is_err());
    }

    #[test]
    fn parse_rejects_external_encryption_mode() {
        // EXTERNAL (0x01) is for inline FIPS-bump-in-the-wire
        // appliances; no analogue in a virtual library. Parser must
        // refuse and the dispatcher routes the Err to CHECK CONDITION.
        let mut page = vec![0u8; 14 + 32];
        page[0..2].copy_from_slice(&PAGE_SET_DATA_ENCRYPTION.to_be_bytes());
        let body_len = (page.len() - 4) as u16;
        page[2..4].copy_from_slice(&body_len.to_be_bytes());
        page[6] = 0x01; // EXTERNAL
        page[7] = DecryptionMode::Decrypt as u8;
        page[8] = ALGORITHM_INDEX_AES_256_GCM;
        page[12..14].copy_from_slice(&(KEY_LEN as u16).to_be_bytes());
        assert!(parse_set_data_encryption(&page).is_err());
    }

    #[test]
    fn capabilities_page_advertises_no_external_mode() {
        // CAP_C bits 3:2 of byte 4 of the algorithm descriptor must
        // be 00 (no EXTERNAL encryption / decryption capability),
        // matching the parser's refusal of EncryptionMode=0x01.
        let buf = build_capabilities_page();
        let cap_c = (buf[16 + 4] >> 2) & 0x03;
        assert_eq!(cap_c, 0b00, "CAP_C must be 00 (no EXTERNAL support)");
        // DECRYPT_C / ENCRYPT_C still set — algorithm itself encrypts
        // and decrypts, just not in EXTERNAL mode.
        assert_eq!(buf[16 + 4] & 0x03, 0b11);
    }
}
