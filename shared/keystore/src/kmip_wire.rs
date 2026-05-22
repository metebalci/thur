// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! KMIP 1.4 TTLV codec — the subset Thur VSA's keystore backend needs.
//!
//! We encode and decode `Encrypt` / `Decrypt` / `Query` requests +
//! responses plus the optional `Authentication` request-header field
//! (KMIP `Credential` UsernameAndPassword). Nothing else — no
//! `Create` / `Get` / `RegisterObject`, no attribute-bag semantics, no
//! BigInteger / DateTime / Interval / Boolean (none of them appear on
//! the surface we use). KMIP spec reference: OASIS KMIP v1.4 § 9.1.1
//! (TTLV encoding) and § 9.1.3 (tag / enum registries).
//!
//! TTLV item layout, big-endian throughout:
//! ```text
//!   Tag   : 3 bytes
//!   Type  : 1 byte
//!   Length: 4 bytes (actual value byte count, before padding)
//!   Value : Length bytes, then 0-padding up to next 8-byte boundary
//! ```
//! Every item is padded so its total size is a multiple of 8. A
//! Structure's value is the concatenation of its children — since each
//! child is already padded, the Structure value itself is always a
//! multiple of 8 and needs no extra padding.

// The tag / enum constant tables below are the authoritative subset
// of the KMIP 1.4 wire registry we ship — kept complete (including
// values we don't currently emit, like RR_INVALID_MESSAGE) so they
// double as an in-tree reference when extending the backend later.
// Same for the `as_integer` helper: it's symmetric with the rest of
// the typed-getter set and would be the first thing a future
// `Create` / `Get` implementer reaches for.
#![allow(dead_code)]

use std::convert::TryInto;

// ============================================================
// Tag registry (KMIP 1.4 § 9.1.3.1). Constants kept in numeric
// order so the table is easy to cross-reference against the spec.
// Only the tags this crate actually emits or reads are listed.
// ============================================================

pub const TAG_ATTRIBUTE_NAME: u32 = 0x42_000A;
pub const TAG_ATTRIBUTE_VALUE: u32 = 0x42_000B;
pub const TAG_AUTHENTICATION: u32 = 0x42_000C;
pub const TAG_BATCH_COUNT: u32 = 0x42_000D;
pub const TAG_BATCH_ITEM: u32 = 0x42_000F;
pub const TAG_BLOCK_CIPHER_MODE: u32 = 0x42_0011;
pub const TAG_CREDENTIAL: u32 = 0x42_0023;
pub const TAG_CREDENTIAL_TYPE: u32 = 0x42_0024;
pub const TAG_CREDENTIAL_VALUE: u32 = 0x42_0025;
pub const TAG_CRYPTOGRAPHIC_ALGORITHM: u32 = 0x42_0028;
pub const TAG_CRYPTOGRAPHIC_PARAMETERS: u32 = 0x42_002B;
pub const TAG_OBJECT_TYPE: u32 = 0x42_0057;
pub const TAG_OPERATION: u32 = 0x42_005C;
pub const TAG_PASSWORD: u32 = 0x42_00A1;
pub const TAG_PROTOCOL_VERSION: u32 = 0x42_0069;
pub const TAG_PROTOCOL_VERSION_MAJOR: u32 = 0x42_006A;
pub const TAG_PROTOCOL_VERSION_MINOR: u32 = 0x42_006B;
pub const TAG_QUERY_FUNCTION: u32 = 0x42_0074;
pub const TAG_REQUEST_HEADER: u32 = 0x42_0077;
pub const TAG_REQUEST_MESSAGE: u32 = 0x42_0078;
pub const TAG_REQUEST_PAYLOAD: u32 = 0x42_0079;
pub const TAG_RESPONSE_HEADER: u32 = 0x42_007A;
pub const TAG_RESPONSE_MESSAGE: u32 = 0x42_007B;
pub const TAG_RESPONSE_PAYLOAD: u32 = 0x42_007C;
pub const TAG_RESULT_MESSAGE: u32 = 0x42_007D;
pub const TAG_RESULT_REASON: u32 = 0x42_007E;
pub const TAG_RESULT_STATUS: u32 = 0x42_007F;
pub const TAG_TIME_STAMP: u32 = 0x42_0092;
pub const TAG_UNIQUE_IDENTIFIER: u32 = 0x42_0094;
pub const TAG_USERNAME: u32 = 0x42_0099;
pub const TAG_IV_COUNTER_NONCE: u32 = 0x42_003D;
pub const TAG_DATA: u32 = 0x42_00C2;
pub const TAG_IV_LENGTH: u32 = 0x42_00CD;
pub const TAG_TAG_LENGTH: u32 = 0x42_00CE;
pub const TAG_AUTHENTICATED_ENCRYPTION_ADDITIONAL_DATA: u32 = 0x42_00FE;
pub const TAG_AUTHENTICATED_ENCRYPTION_TAG: u32 = 0x42_00FF;

// ============================================================
// Enum values. The enum's tag determines which numeric space these
// belong to — a value of `0x1F` on TAG_OPERATION is "Encrypt", on
// TAG_RESULT_REASON it would be something different. The constants
// below are prefixed by the enum they belong to.
// ============================================================

// Operation (TAG_OPERATION = 0x42005C). KMIP 1.4 § 9.1.3.2.27.
pub const OP_QUERY: u32 = 0x18;
pub const OP_ENCRYPT: u32 = 0x1F;
pub const OP_DECRYPT: u32 = 0x20;

// QueryFunction (TAG_QUERY_FUNCTION = 0x420074). KMIP 1.4 § 9.1.3.2.24.
pub const QF_QUERY_OPERATIONS: u32 = 0x01;

// CryptographicAlgorithm (TAG_CRYPTOGRAPHIC_ALGORITHM = 0x420028).
// KMIP 1.4 § 9.1.3.2.13.
pub const ALG_AES: u32 = 0x03;

// BlockCipherMode (TAG_BLOCK_CIPHER_MODE = 0x420011). KMIP 1.4 §
// 9.1.3.2.14.
pub const MODE_GCM: u32 = 0x09;

// CredentialType (TAG_CREDENTIAL_TYPE = 0x420024). KMIP 1.4 § 9.1.3.2.1.
pub const CRED_TYPE_USERNAME_AND_PASSWORD: u32 = 0x01;

// ResultStatus (TAG_RESULT_STATUS = 0x42007F). KMIP 1.4 § 9.1.3.2.29.
pub const RS_SUCCESS: u32 = 0x00;
pub const RS_OPERATION_FAILED: u32 = 0x01;
pub const RS_OPERATION_PENDING: u32 = 0x02;
pub const RS_OPERATION_UNDONE: u32 = 0x03;

// ResultReason (TAG_RESULT_REASON = 0x42007E). KMIP 1.4 § 9.1.3.2.28.
// Only the values we map into `KeyStoreError`. The default arm in
// `kmip::classify_result_reason` lumps the rest into `Other`.
pub const RR_ITEM_NOT_FOUND: u32 = 0x01;
pub const RR_RESPONSE_TOO_LARGE: u32 = 0x02;
pub const RR_AUTHENTICATION_NOT_SUCCESSFUL: u32 = 0x03;
pub const RR_INVALID_MESSAGE: u32 = 0x04;
pub const RR_OPERATION_NOT_SUPPORTED: u32 = 0x05;
pub const RR_MISSING_DATA: u32 = 0x06;
pub const RR_INVALID_FIELD: u32 = 0x07;
pub const RR_FEATURE_NOT_SUPPORTED: u32 = 0x08;
pub const RR_CRYPTOGRAPHIC_FAILURE: u32 = 0x0A;
pub const RR_ILLEGAL_OPERATION: u32 = 0x0B;
pub const RR_PERMISSION_DENIED: u32 = 0x0C;

// ============================================================
// TTLV item types (KMIP 1.4 § 9.1.1.3). Subset — we never encode
// BigInteger; everything else we accept on the wire so we can walk
// past server-emitted fields (e.g. Response Header carries a
// `TimeStamp` of type DateTime) without choking.
// ============================================================

const TYPE_STRUCTURE: u8 = 0x01;
const TYPE_INTEGER: u8 = 0x02;
const TYPE_LONG_INTEGER: u8 = 0x03;
const TYPE_ENUMERATION: u8 = 0x05;
const TYPE_BOOLEAN: u8 = 0x06;
const TYPE_TEXT_STRING: u8 = 0x07;
const TYPE_BYTE_STRING: u8 = 0x08;
const TYPE_DATE_TIME: u8 = 0x09;
const TYPE_INTERVAL: u8 = 0x0A;

/// One TTLV value, restricted to the type set this codec supports.
/// `DateTime` carries seconds since the Unix epoch (KMIP § 9.1.1.3
/// signed int64); `Interval` carries a duration in seconds (unsigned
/// int32). We decode-and-preserve both even though we never construct
/// them ourselves — they appear in KMIP response headers.
#[derive(Debug, Clone)]
pub enum Value {
    Structure(Vec<Field>),
    Integer(i32),
    LongInteger(i64),
    Enumeration(u32),
    Boolean(bool),
    TextString(String),
    ByteString(Vec<u8>),
    DateTime(i64),
    Interval(u32),
}

/// One TTLV item — tag + typed value. The wire-format type byte is
/// recovered from the `Value` discriminant on encode.
#[derive(Debug, Clone)]
pub struct Field {
    pub tag: u32,
    pub value: Value,
}

impl Field {
    pub fn structure(tag: u32, children: Vec<Field>) -> Self {
        Self {
            tag,
            value: Value::Structure(children),
        }
    }
    pub fn integer(tag: u32, n: i32) -> Self {
        Self {
            tag,
            value: Value::Integer(n),
        }
    }
    pub fn enumeration(tag: u32, n: u32) -> Self {
        Self {
            tag,
            value: Value::Enumeration(n),
        }
    }
    pub fn text_string(tag: u32, s: impl Into<String>) -> Self {
        Self {
            tag,
            value: Value::TextString(s.into()),
        }
    }
    pub fn byte_string(tag: u32, b: impl Into<Vec<u8>>) -> Self {
        Self {
            tag,
            value: Value::ByteString(b.into()),
        }
    }

    /// First immediate child with `tag` inside a Structure. `None`
    /// for non-Structure values or absent tags.
    pub fn child(&self, tag: u32) -> Option<&Field> {
        if let Value::Structure(children) = &self.value {
            children.iter().find(|c| c.tag == tag)
        } else {
            None
        }
    }

    /// All immediate children with `tag` — used for the repeated
    /// `Operation` entries in a Query response.
    pub fn children(&self, tag: u32) -> Vec<&Field> {
        if let Value::Structure(children) = &self.value {
            children.iter().filter(|c| c.tag == tag).collect()
        } else {
            Vec::new()
        }
    }

    pub fn as_enumeration(&self) -> Option<u32> {
        if let Value::Enumeration(n) = &self.value {
            Some(*n)
        } else {
            None
        }
    }
    pub fn as_text_string(&self) -> Option<&str> {
        if let Value::TextString(s) = &self.value {
            Some(s.as_str())
        } else {
            None
        }
    }
    pub fn as_byte_string(&self) -> Option<&[u8]> {
        if let Value::ByteString(b) = &self.value {
            Some(b.as_slice())
        } else {
            None
        }
    }
    pub fn as_integer(&self) -> Option<i32> {
        if let Value::Integer(n) = &self.value {
            Some(*n)
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TtlvError {
    #[error("ttlv: truncated at offset {0} (need {1} bytes, have {2})")]
    Truncated(usize, usize, usize),
    #[error("ttlv: invalid item type 0x{0:02x} for tag 0x{1:06x}")]
    InvalidType(u8, u32),
    #[error("ttlv: invalid utf-8 in TextString at tag 0x{0:06x}: {1}")]
    InvalidUtf8(u32, std::string::FromUtf8Error),
    #[error("ttlv: invalid length {0} for fixed-width type 0x{1:02x}")]
    InvalidLength(u32, u8),
    #[error("ttlv: structure value length {0} is not a multiple of 8")]
    StructureUnaligned(u32),
    #[error("ttlv: {0} trailing byte(s) after top-level item")]
    Trailing(usize),
}

// ============================================================
// Encoding
// ============================================================

/// Encode a top-level message (typically a RequestMessage or
/// ResponseMessage Structure). Returns the on-wire bytes.
pub fn encode_message(root: &Field) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    encode_field(root, &mut out);
    out
}

fn encode_field(field: &Field, out: &mut Vec<u8>) {
    // Tag = 3 bytes BE. (u32 truncated to low 24 bits.)
    let tag_be = field.tag.to_be_bytes();
    out.extend_from_slice(&tag_be[1..4]);

    match &field.value {
        Value::Structure(children) => {
            out.push(TYPE_STRUCTURE);
            let len_pos = out.len();
            out.extend_from_slice(&[0u8; 4]); // length placeholder
            let value_start = out.len();
            for child in children {
                encode_field(child, out);
            }
            let value_len = (out.len() - value_start) as u32;
            out[len_pos..len_pos + 4].copy_from_slice(&value_len.to_be_bytes());
            // No outer padding — each child already padded itself.
        }
        Value::Integer(n) => {
            out.push(TYPE_INTEGER);
            out.extend_from_slice(&4u32.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&[0u8; 4]); // pad 4→8
        }
        Value::LongInteger(n) => {
            out.push(TYPE_LONG_INTEGER);
            out.extend_from_slice(&8u32.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes()); // 8 bytes, no pad
        }
        Value::Enumeration(n) => {
            out.push(TYPE_ENUMERATION);
            out.extend_from_slice(&4u32.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&[0u8; 4]); // pad 4→8
        }
        Value::Boolean(b) => {
            out.push(TYPE_BOOLEAN);
            out.extend_from_slice(&8u32.to_be_bytes());
            let mut v = [0u8; 8];
            v[7] = u8::from(*b);
            out.extend_from_slice(&v); // 8 bytes, no pad
        }
        Value::DateTime(n) => {
            out.push(TYPE_DATE_TIME);
            out.extend_from_slice(&8u32.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes()); // 8 bytes, no pad
        }
        Value::Interval(n) => {
            out.push(TYPE_INTERVAL);
            out.extend_from_slice(&4u32.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&[0u8; 4]); // pad 4→8
        }
        Value::TextString(s) => {
            let bytes = s.as_bytes();
            out.push(TYPE_TEXT_STRING);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
            let pad = (8 - bytes.len() % 8) % 8;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        Value::ByteString(b) => {
            out.push(TYPE_BYTE_STRING);
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
            let pad = (8 - b.len() % 8) % 8;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
    }
}

// ============================================================
// Decoding
// ============================================================

/// Decode one top-level TTLV item. Errors if trailing bytes remain.
pub fn decode_message(buf: &[u8]) -> Result<Field, TtlvError> {
    let (field, rest) = decode_field(buf)?;
    if !rest.is_empty() {
        return Err(TtlvError::Trailing(rest.len()));
    }
    Ok(field)
}

fn decode_field(buf: &[u8]) -> Result<(Field, &[u8]), TtlvError> {
    if buf.len() < 8 {
        return Err(TtlvError::Truncated(0, 8, buf.len()));
    }
    let tag = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | (buf[2] as u32);
    let ty = buf[3];
    let len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    // padded_len: bytes the wire actually consumes after the 8-byte
    // header. Structures don't get extra padding (their bodies are
    // self-padded by the children); everything else rounds up to 8.
    let padded_len = if ty == TYPE_STRUCTURE {
        len
    } else {
        (len + 7) & !7
    };
    if buf.len() < 8 + padded_len {
        return Err(TtlvError::Truncated(0, 8 + padded_len, buf.len()));
    }
    let value_bytes = &buf[8..8 + len];
    let rest = &buf[8 + padded_len..];

    let value = match ty {
        TYPE_STRUCTURE => {
            if !len.is_multiple_of(8) {
                return Err(TtlvError::StructureUnaligned(len as u32));
            }
            let mut children = Vec::new();
            let mut inner = value_bytes;
            while !inner.is_empty() {
                let (child, next) = decode_field(inner)?;
                inner = next;
                children.push(child);
            }
            Value::Structure(children)
        }
        TYPE_INTEGER => {
            let bytes = fixed_value_bytes::<4>(value_bytes, ty)?;
            Value::Integer(i32::from_be_bytes(bytes))
        }
        TYPE_LONG_INTEGER => {
            let bytes = fixed_value_bytes::<8>(value_bytes, ty)?;
            Value::LongInteger(i64::from_be_bytes(bytes))
        }
        TYPE_ENUMERATION => {
            let bytes = fixed_value_bytes::<4>(value_bytes, ty)?;
            Value::Enumeration(u32::from_be_bytes(bytes))
        }
        TYPE_BOOLEAN => {
            // KMIP § 9.1.1.3: 8 bytes, treated as a 64-bit big-endian
            // integer; non-zero → true.
            let bytes = fixed_value_bytes::<8>(value_bytes, ty)?;
            Value::Boolean(u64::from_be_bytes(bytes) != 0)
        }
        TYPE_DATE_TIME => {
            let bytes = fixed_value_bytes::<8>(value_bytes, ty)?;
            Value::DateTime(i64::from_be_bytes(bytes))
        }
        TYPE_INTERVAL => {
            let bytes = fixed_value_bytes::<4>(value_bytes, ty)?;
            Value::Interval(u32::from_be_bytes(bytes))
        }
        TYPE_TEXT_STRING => Value::TextString(
            String::from_utf8(value_bytes.to_vec()).map_err(|e| TtlvError::InvalidUtf8(tag, e))?,
        ),
        TYPE_BYTE_STRING => Value::ByteString(value_bytes.to_vec()),
        other => return Err(TtlvError::InvalidType(other, tag)),
    };

    Ok((Field { tag, value }, rest))
}

/// Pull a fixed-width N-byte array out of a value slice. Surfaces
/// `InvalidLength` if the slice's length doesn't match the
/// type-specified width — happens when a malicious or buggy peer
/// sends, e.g., `Length=8` on a `TYPE_INTEGER` record.
fn fixed_value_bytes<const N: usize>(value: &[u8], ty: u8) -> Result<[u8; N], TtlvError> {
    value
        .try_into()
        .map_err(|_| TtlvError::InvalidLength(value.len() as u32, ty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_roundtrip_with_padding() {
        let f = Field::integer(TAG_BATCH_COUNT, 1);
        let buf = encode_message(&f);
        // 8 header + 4 value + 4 pad = 16.
        assert_eq!(buf.len(), 16);
        // Tag 0x42 0x00 0x0D, Type 0x02, Length 0x00000004, value
        // 0x00000001, pad 4×0x00.
        assert_eq!(
            buf,
            &[0x42, 0x00, 0x0D, 0x02, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0]
        );
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.tag, TAG_BATCH_COUNT);
        assert_eq!(back.as_integer(), Some(1));
    }

    #[test]
    fn enumeration_roundtrip() {
        let f = Field::enumeration(TAG_OPERATION, OP_ENCRYPT);
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16);
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.tag, TAG_OPERATION);
        assert_eq!(back.as_enumeration(), Some(OP_ENCRYPT));
    }

    #[test]
    fn long_integer_no_padding() {
        let f = Field {
            tag: TAG_PROTOCOL_VERSION_MAJOR,
            value: Value::LongInteger(0x0102030405060708),
        };
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16); // 8 header + 8 value, no pad
        let back = decode_message(&buf).unwrap();
        match back.value {
            Value::LongInteger(n) => assert_eq!(n, 0x0102030405060708),
            other => panic!("expected LongInteger, got {other:?}"),
        }
    }

    #[test]
    fn text_string_padded_to_8() {
        // "hello" — 5 bytes → 3 bytes pad.
        let f = Field::text_string(TAG_USERNAME, "hello");
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16);
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.as_text_string(), Some("hello"));
    }

    #[test]
    fn byte_string_exact_8() {
        let f = Field::byte_string(TAG_DATA, vec![0xAA; 8]);
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16); // 8 header + 8 value + 0 pad
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.as_byte_string(), Some(vec![0xAA; 8].as_slice()));
    }

    #[test]
    fn byte_string_zero_length() {
        let f = Field::byte_string(TAG_DATA, Vec::<u8>::new());
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 8); // header only
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.as_byte_string(), Some(&[][..]));
    }

    #[test]
    fn structure_with_two_children() {
        let s = Field::structure(
            TAG_PROTOCOL_VERSION,
            vec![
                Field::integer(TAG_PROTOCOL_VERSION_MAJOR, 1),
                Field::integer(TAG_PROTOCOL_VERSION_MINOR, 4),
            ],
        );
        let buf = encode_message(&s);
        // 8 (outer hdr) + 2 × 16 (children) = 40.
        assert_eq!(buf.len(), 40);
        let back = decode_message(&buf).unwrap();
        assert_eq!(back.tag, TAG_PROTOCOL_VERSION);
        let major = back.child(TAG_PROTOCOL_VERSION_MAJOR).unwrap();
        let minor = back.child(TAG_PROTOCOL_VERSION_MINOR).unwrap();
        assert_eq!(major.as_integer(), Some(1));
        assert_eq!(minor.as_integer(), Some(4));
    }

    #[test]
    fn nested_structure_round_trips() {
        // Mimic a real Authentication block.
        let auth = Field::structure(
            TAG_AUTHENTICATION,
            vec![Field::structure(
                TAG_CREDENTIAL,
                vec![
                    Field::enumeration(TAG_CREDENTIAL_TYPE, CRED_TYPE_USERNAME_AND_PASSWORD),
                    Field::structure(
                        TAG_CREDENTIAL_VALUE,
                        vec![
                            Field::text_string(TAG_USERNAME, "alice"),
                            Field::text_string(TAG_PASSWORD, "s3cr3t"),
                        ],
                    ),
                ],
            )],
        );
        let buf = encode_message(&auth);
        let back = decode_message(&buf).unwrap();
        let cred = back.child(TAG_CREDENTIAL).unwrap();
        assert_eq!(
            cred.child(TAG_CREDENTIAL_TYPE).unwrap().as_enumeration(),
            Some(CRED_TYPE_USERNAME_AND_PASSWORD)
        );
        let cv = cred.child(TAG_CREDENTIAL_VALUE).unwrap();
        assert_eq!(
            cv.child(TAG_USERNAME).unwrap().as_text_string(),
            Some("alice")
        );
        assert_eq!(
            cv.child(TAG_PASSWORD).unwrap().as_text_string(),
            Some("s3cr3t")
        );
    }

    #[test]
    fn decode_truncated_header_errors() {
        let buf = [0x42u8, 0x00, 0x77, 0x01, 0, 0, 0]; // 7 bytes
        let err = decode_message(&buf).unwrap_err();
        assert!(matches!(err, TtlvError::Truncated(_, 8, 7)));
    }

    #[test]
    fn decode_truncated_value_errors() {
        // Says length is 16, but only 8 bytes of value follow.
        let mut buf = vec![0x42, 0x00, 0xC2, TYPE_BYTE_STRING, 0, 0, 0, 16];
        buf.extend_from_slice(&[0xAB; 8]);
        let err = decode_message(&buf).unwrap_err();
        assert!(matches!(err, TtlvError::Truncated(0, 24, 16)));
    }

    #[test]
    fn decode_bad_integer_length_errors() {
        // Type Integer but Length=8 — malformed.
        let mut buf = vec![0x42, 0x00, 0x0D, TYPE_INTEGER, 0, 0, 0, 8];
        buf.extend_from_slice(&[0u8; 8]);
        let err = decode_message(&buf).unwrap_err();
        assert!(matches!(err, TtlvError::InvalidLength(8, TYPE_INTEGER)));
    }

    #[test]
    fn decode_trailing_bytes_errors() {
        // One valid Integer record, plus one stray byte.
        let mut buf = encode_message(&Field::integer(TAG_BATCH_COUNT, 1));
        buf.push(0xFF);
        let err = decode_message(&buf).unwrap_err();
        assert!(matches!(err, TtlvError::Trailing(1)));
    }

    #[test]
    fn unsupported_type_errors() {
        // Type 0x04 = BigInteger. We don't emit it and don't decode
        // it either; verify we cleanly reject so a buggy / malicious
        // peer can't drift us into surprising states.
        let buf = vec![0x42, 0x00, 0x0D, 0x04, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1];
        let err = decode_message(&buf).unwrap_err();
        assert!(matches!(err, TtlvError::InvalidType(0x04, _)));
    }

    #[test]
    fn boolean_roundtrip() {
        let f = Field {
            tag: TAG_BATCH_COUNT,
            value: Value::Boolean(true),
        };
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16); // 8 header + 8 value
        let back = decode_message(&buf).unwrap();
        match back.value {
            Value::Boolean(b) => assert!(b),
            other => panic!("expected Boolean(true), got {other:?}"),
        }
    }

    #[test]
    fn datetime_roundtrip() {
        // KMIP response headers carry a TimeStamp of type DateTime —
        // the decoder must walk past it without erroring even though
        // we never construct DateTime values ourselves.
        let f = Field {
            tag: TAG_TIME_STAMP,
            value: Value::DateTime(1_700_000_000),
        };
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16);
        let back = decode_message(&buf).unwrap();
        match back.value {
            Value::DateTime(n) => assert_eq!(n, 1_700_000_000),
            other => panic!("expected DateTime, got {other:?}"),
        }
    }

    #[test]
    fn interval_roundtrip() {
        let f = Field {
            tag: TAG_BATCH_COUNT,
            value: Value::Interval(60),
        };
        let buf = encode_message(&f);
        assert_eq!(buf.len(), 16);
        let back = decode_message(&buf).unwrap();
        match back.value {
            Value::Interval(n) => assert_eq!(n, 60),
            other => panic!("expected Interval, got {other:?}"),
        }
    }
}
