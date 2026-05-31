// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! DH-HMAC-CHAP crypto for the controller (target) side of NVMe in-band
//! authentication (NVMe Base §8.13).
//!
//! This is the cryptographic core the transport state machine
//! (`crate::server::run_auth_phase`) drives. It is deliberately
//! split from the wire (de)serialization in [`nvme_base::auth`]: this
//! module owns the secret parsing, the HMAC response computation, and
//! the optional Diffie-Hellman key agreement; the byte layouts live in
//! `nvme-base`.
//!
//! All crypto goes through OpenSSL — the same backend
//! `crate::tls` already uses for the TLS-PSK HKDF/HMAC, so there is a
//! single crypto source of truth in this crate. The formulas are taken
//! verbatim from the Linux kernel (`drivers/nvme/common/auth.c`,
//! `drivers/nvme/target/auth.c`), the interop reference for a stock
//! `nvme connect --dhchap-secret` host:
//!
//! - **Secret transform** ([`transform_key`]): if the `DHHC-1:` secret
//!   carries a non-zero hash selector, the key fed to the response HMAC
//!   is `HMAC_secret(NQN || "NVMe-over-Fabrics")` using the secret's
//!   own hash; selector 0 means "use the secret bytes verbatim."
//! - **Response** ([`dhchap_response`]): `HMAC_transformed(challenge ||
//!   seqnum(LE32) || t_id(LE16) || sc_c || label || nqn_a || 0x00 ||
//!   nqn_b)` using the negotiated hash. `label` is `b"HostHost"` with
//!   `(hostnqn, subnqn)` for the host response R1, `b"Controller"` with
//!   `(subnqn, hostnqn)` for the controller response R2.
//! - **Augmented challenge** ([`augmented_challenge`], DH groups only):
//!   `HMAC_sesskey(challenge)`, where the session key is the hash of
//!   the (MSB-zero-padded, prime-length) DH shared secret.

use nvme_base::auth::hash_len;
use openssl::bn::{BigNum, BigNumContext};
use openssl::dh::Dh;
use openssl::hash::{MessageDigest, hash};
use openssl::pkey::{PKey, Private};
use openssl::sign::Signer;

use crate::ffdhe::ffdhe_prime_hex;

/// The literal "NVMe-over-Fabrics" label (17 bytes) appended to the NQN
/// in the secret-transform HMAC (kernel `nvme_auth_transform_key`).
const TRANSFORM_LABEL: &[u8] = b"NVMe-over-Fabrics";

/// Errors from the DH-HMAC-CHAP crypto layer. The controller maps these
/// to an AUTH_Failure on the wire (`rescode_exp = FAILED` /
/// `DHGROUP_UNUSABLE` / `HASH_UNUSABLE`).
#[derive(Debug, thiserror::Error)]
pub enum DhchapError {
    #[error("malformed DHHC-1 secret: {0}")]
    BadSecretFormat(&'static str),
    #[error("DHHC-1 secret CRC mismatch")]
    CrcMismatch,
    #[error("unsupported hash id 0x{0:02X}")]
    UnsupportedHash(u8),
    #[error("unknown / unsupported DH group id 0x{0:02X}")]
    UnknownDhGroup(u8),
    #[error("peer DH public value out of range")]
    BadPeerPublic,
    #[error("openssl: {0}")]
    Openssl(#[from] openssl::error::ErrorStack),
}

/// Map an NVMe auth hash id (0x01/0x02/0x03) to its OpenSSL digest.
fn message_digest(hash_id: u8) -> Result<MessageDigest, DhchapError> {
    match hash_id {
        nvme_base::auth::NVME_AUTH_HASH_SHA256 => Ok(MessageDigest::sha256()),
        nvme_base::auth::NVME_AUTH_HASH_SHA384 => Ok(MessageDigest::sha384()),
        nvme_base::auth::NVME_AUTH_HASH_SHA512 => Ok(MessageDigest::sha512()),
        other => Err(DhchapError::UnsupportedHash(other)),
    }
}

/// HMAC `data` under `key` with the digest selected by `hash_id`.
fn hmac(hash_id: u8, key: &[u8], data: &[u8]) -> Result<Vec<u8>, DhchapError> {
    let pkey = PKey::hmac(key)?;
    let mut signer = Signer::new(message_digest(hash_id)?, &pkey)?;
    signer.update(data)?;
    Ok(signer.sign_to_vec()?)
}

// ============================ DHHC-1 secret ============================

/// A parsed `DHHC-1:` DH-HMAC-CHAP secret: the raw key bytes plus the
/// hash selector that drives [`transform_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhchapKey {
    /// Key material with the 4-byte CRC already stripped + verified.
    pub raw: Vec<u8>,
    /// Hash selector from the `DHHC-1:NN:` prefix: 0 = use the key
    /// verbatim (no NQN transform), 1/2/3 = SHA-256/384/512.
    pub hash: u8,
}

/// Parse a `DHHC-1:NN:<base64>:` secret (kernel `nvme_auth_parse_key` +
/// `nvme_auth_extract_key`). `NN` is two decimal digits selecting the
/// transform hash; the base64 decodes to `key || crc32_le(key)` of
/// total length 36, 52, or 68 (key 32/48/64 + 4-byte CRC). The
/// trailing `:` the kernel appends is optional.
pub fn parse_dhchap_secret(secret: &str) -> Result<DhchapKey, DhchapError> {
    let body = secret
        .strip_prefix("DHHC-1:")
        .ok_or(DhchapError::BadSecretFormat("missing DHHC-1: prefix"))?;
    // Exactly two decimal hash-selector digits then ':'.
    if body.len() < 3 || body.as_bytes()[2] != b':' {
        return Err(DhchapError::BadSecretFormat("missing NN: hash selector"));
    }
    let hash: u8 = body[0..2]
        .parse()
        .map_err(|_| DhchapError::BadSecretFormat("non-numeric hash selector"))?;
    if hash > 3 {
        return Err(DhchapError::UnsupportedHash(hash));
    }
    // Base64 runs from after "NN:" to the last ':' (kernel strrchr).
    let rest = &body[3..];
    let b64 = rest.strip_suffix(':').unwrap_or(rest);
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| DhchapError::BadSecretFormat("invalid base64"))?;
    if !matches!(decoded.len(), 36 | 52 | 68) {
        return Err(DhchapError::BadSecretFormat(
            "decoded key length not 36/52/68",
        ));
    }
    // Length validated above; the shared CRC-tail core validates the
    // trailing CRC-32 and returns the key bytes (CRC stripped).
    let key = crate::split_verify_crc_tail(&decoded).ok_or(DhchapError::CrcMismatch)?;
    Ok(DhchapKey {
        raw: key.to_vec(),
        hash,
    })
}

/// Encode raw key bytes into a `DHHC-1:NN:<base64>:` string. Mirrors
/// `nvme gen-dhchap-key`; used by tests and the integration harness.
pub fn encode_dhchap_secret(raw: &[u8], hash: u8) -> String {
    let crc = crc32fast::hash(raw);
    let mut body = raw.to_vec();
    body.extend_from_slice(&crc.to_le_bytes());
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
    format!("DHHC-1:{:02}:{}:", hash, b64)
}

/// Transform a parsed secret into the HMAC key used for the response
/// (kernel `nvme_auth_transform_key`). Selector 0 returns the key
/// verbatim; otherwise the key is `HMAC_secret(nqn || "NVMe-over-Fabrics")`
/// keyed by the secret bytes, using the secret's *own* hash (which may
/// differ from the negotiated session hash).
pub fn transform_key(key: &DhchapKey, nqn: &str) -> Result<Vec<u8>, DhchapError> {
    if key.hash == 0 {
        return Ok(key.raw.clone());
    }
    let pkey = PKey::hmac(&key.raw)?;
    let mut signer = Signer::new(message_digest(key.hash)?, &pkey)?;
    signer.update(nqn.as_bytes())?;
    signer.update(TRANSFORM_LABEL)?;
    Ok(signer.sign_to_vec()?)
}

// ========================= Response computation =========================

/// Inputs to a single DH-HMAC-CHAP response HMAC. The same routine
/// computes both the host response R1 (which the controller validates)
/// and the controller response R2 (which the controller sends for
/// mutual auth) — only the `label` and NQN order differ.
pub struct ResponseInput<'a> {
    /// HMAC key from [`transform_key`].
    pub transformed_key: &'a [u8],
    /// Negotiated session hash id (drives the HMAC digest).
    pub hash_id: u8,
    /// Challenge bytes — the augmented challenge when a DH group is in
    /// use, otherwise the raw challenge value.
    pub challenge: &'a [u8],
    /// Sequence number (S1 for R1, S2 for R2).
    pub seqnum: u32,
    /// Transaction id from the Negotiate message.
    pub t_id: u16,
    /// Secure-channel concatenation byte from Negotiate (0 for us).
    pub sc_c: u8,
    /// `b"HostHost"` for R1, `b"Controller"` for R2.
    pub label: &'a [u8],
    /// First NQN: hostnqn for R1, subnqn for R2.
    pub nqn_first: &'a str,
    /// Second NQN: subnqn for R1, hostnqn for R2.
    pub nqn_second: &'a str,
}

/// Compute a DH-HMAC-CHAP response (kernel `nvmet_auth_host_hash` /
/// `nvmet_auth_ctrl_hash`). The HMAC input is the exact byte
/// concatenation `challenge || seqnum(LE32) || t_id(LE16) || sc_c ||
/// label || nqn_first || 0x00 || nqn_second`.
pub fn dhchap_response(input: &ResponseInput) -> Result<Vec<u8>, DhchapError> {
    let pkey = PKey::hmac(input.transformed_key)?;
    let mut signer = Signer::new(message_digest(input.hash_id)?, &pkey)?;
    signer.update(input.challenge)?;
    signer.update(&input.seqnum.to_le_bytes())?;
    signer.update(&input.t_id.to_le_bytes())?;
    signer.update(&[input.sc_c])?;
    signer.update(input.label)?;
    signer.update(input.nqn_first.as_bytes())?;
    signer.update(&[0u8])?;
    signer.update(input.nqn_second.as_bytes())?;
    Ok(signer.sign_to_vec()?)
}

/// Generate `n` cryptographically-random bytes (the challenge value).
pub fn random_bytes(n: usize) -> Result<Vec<u8>, DhchapError> {
    let mut v = vec![0u8; n];
    openssl::rand::rand_bytes(&mut v)?;
    Ok(v)
}

/// Generate a random sequence number (S1 / the controller seqnum).
pub fn random_seqnum() -> Result<u32, DhchapError> {
    let mut b = [0u8; 4];
    openssl::rand::rand_bytes(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Label for the host response R1.
pub const LABEL_HOST: &[u8] = b"HostHost";
/// Label for the controller response R2.
pub const LABEL_CONTROLLER: &[u8] = b"Controller";

/// Constant-time equality for comparing a received response against the
/// locally computed one. A length mismatch (never expected — both are
/// the negotiated hash length) short-circuits to `false`; the length is
/// not secret.
pub fn responses_equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && openssl::memcmp::eq(a, b)
}

// =========================== Augmented challenge ===========================

/// Augment a challenge with the DH session key (kernel
/// `nvme_auth_augmented_challenge`): `HMAC_sesskey(challenge)` under the
/// negotiated hash. Only used when a non-NULL DH group is negotiated.
pub fn augmented_challenge(
    hash_id: u8,
    session_key: &[u8],
    challenge: &[u8],
) -> Result<Vec<u8>, DhchapError> {
    hmac(hash_id, session_key, challenge)
}

// ============================ Diffie-Hellman ============================

/// An ephemeral FFDHE keypair for one authentication transaction.
pub struct DhKeypair {
    dh: Dh<Private>,
    /// Prime byte length — the fixed width of public values and the DH
    /// shared secret on the wire (MSB-zero-padded).
    prime_len: usize,
}

impl DhKeypair {
    /// Generate an ephemeral keypair for the given DH group id (one of
    /// the RFC 7919 FFDHE groups; the NULL group has no keypair).
    pub fn generate(dhgid: u8) -> Result<Self, DhchapError> {
        let hex = ffdhe_prime_hex(dhgid).ok_or(DhchapError::UnknownDhGroup(dhgid))?;
        let p = BigNum::from_hex_str(hex)?;
        let prime_len = p.num_bytes() as usize;
        let g = BigNum::from_u32(2)?;
        // RFC 7919 groups are safe primes: the subgroup order is
        // q = (p - 1) / 2. Setting q bounds the ephemeral private key
        // to the prime-order subgroup.
        let one = BigNum::from_u32(1)?;
        let mut p_minus_1 = BigNum::new()?;
        p_minus_1.checked_sub(&p, &one)?;
        let mut q = BigNum::new()?;
        q.rshift1(&p_minus_1)?;
        let params = Dh::from_params(p, g, q)?;
        let dh = params.generate_key()?;
        Ok(Self { dh, prime_len })
    }

    /// Prime byte length (= public-value / shared-secret width).
    pub fn prime_len(&self) -> usize {
        self.prime_len
    }

    /// Our DH public value g^x mod p, MSB-zero-padded to the prime
    /// length (the wire width the peer and kernel expect).
    pub fn public_value(&self) -> Result<Vec<u8>, DhchapError> {
        Ok(self.dh.public_key().to_vec_padded(self.prime_len as i32)?)
    }

    /// Derive the session key from the peer's public value:
    /// `H(g^xy mod p)`, where the shared secret is MSB-zero-padded to
    /// the prime length before hashing (matching the kernel, which
    /// hashes the full `crypto_kpp_maxsize` buffer). We compute the
    /// modular exponentiation directly rather than via OpenSSL's
    /// `DH_compute_key`, whose output is not MSB-padded.
    pub fn session_key(&self, peer_public: &[u8], hash_id: u8) -> Result<Vec<u8>, DhchapError> {
        let peer = BigNum::from_slice(peer_public)?;
        let p = self.dh.prime_p();
        // Reject degenerate / out-of-range peer keys (1 < y < p-1).
        let one = BigNum::from_u32(1)?;
        let mut p_minus_1 = BigNum::new()?;
        p_minus_1.checked_sub(p, &one)?;
        if peer <= one || peer >= p_minus_1 {
            return Err(DhchapError::BadPeerPublic);
        }
        let mut ctx = BigNumContext::new()?;
        let mut shared = BigNum::new()?;
        shared.mod_exp(&peer, self.dh.private_key(), p, &mut ctx)?;
        let padded = shared.to_vec_padded(self.prime_len as i32)?;
        // hash digest length always equals the negotiated hash len.
        let _ = hash_len(hash_id).ok_or(DhchapError::UnsupportedHash(hash_id))?;
        Ok(hash(message_digest(hash_id)?, &padded)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_base::auth::{
        NVME_AUTH_DHGROUP_2048, NVME_AUTH_DHGROUP_3072, NVME_AUTH_DHGROUP_4096,
        NVME_AUTH_DHGROUP_6144, NVME_AUTH_DHGROUP_8192, NVME_AUTH_HASH_SHA256,
        NVME_AUTH_HASH_SHA384,
    };

    const NQN_HOST: &str = "nqn.2014-08.org.nvmexpress:uuid:test-host";
    const NQN_SUB: &str = "nqn.2025-10.com.metebalci:thurvsa";

    #[test]
    fn dhchap_secret_round_trip() {
        let raw: Vec<u8> = (0..32).collect();
        let s = encode_dhchap_secret(&raw, 0);
        let k = parse_dhchap_secret(&s).unwrap();
        assert_eq!(k.raw, raw);
        assert_eq!(k.hash, 0);

        // 48-byte key, SHA-384 selector.
        let raw48: Vec<u8> = (0..48).collect();
        let k2 = parse_dhchap_secret(&encode_dhchap_secret(&raw48, 2)).unwrap();
        assert_eq!(k2.raw, raw48);
        assert_eq!(k2.hash, 2);
    }

    #[test]
    fn dhchap_secret_crc_flip_rejected() {
        let raw: Vec<u8> = (0..32).collect();
        let s = encode_dhchap_secret(&raw, 1);
        // Corrupt one base64 char in the key region (not the CRC tail).
        let idx = "DHHC-1:01:".len() + 4;
        let mut chars: Vec<char> = s.chars().collect();
        chars[idx] = if chars[idx] == 'A' { 'B' } else { 'A' };
        let s: String = chars.into_iter().collect();
        assert!(matches!(
            parse_dhchap_secret(&s),
            Err(DhchapError::CrcMismatch) | Err(DhchapError::BadSecretFormat(_))
        ));
    }

    #[test]
    fn dhchap_secret_rejects_bad_prefix_and_length() {
        assert!(matches!(
            parse_dhchap_secret("NVMeTLSkey-1:01:abc:"),
            Err(DhchapError::BadSecretFormat(_))
        ));
        // Valid prefix, but base64 decodes to a wrong length.
        use base64::Engine;
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert!(matches!(
            parse_dhchap_secret(&format!("DHHC-1:00:{short}:")),
            Err(DhchapError::BadSecretFormat(_))
        ));
    }

    // Cross-check vectors computed independently with Python's hmac
    // module (see the implementation notes) — verifies the exact byte
    // concatenation, not just internal self-consistency.

    #[test]
    fn transform_key_matches_reference_vector() {
        let raw: Vec<u8> = (0..32).collect();
        let key = DhchapKey {
            raw,
            hash: NVME_AUTH_HASH_SHA256,
        };
        let tk = transform_key(&key, NQN_HOST).unwrap();
        assert_eq!(
            hex::encode(tk),
            "a567f6f83c24f2634527219d51f671c6a28eec4fd721ffb54aeaedd01c01783b"
        );
    }

    #[test]
    fn transform_key_hash_zero_is_identity() {
        let raw: Vec<u8> = (0..32).collect();
        let key = DhchapKey {
            raw: raw.clone(),
            hash: 0,
        };
        assert_eq!(transform_key(&key, NQN_HOST).unwrap(), raw);
    }

    #[test]
    fn augmented_challenge_matches_reference_vector() {
        let skey = vec![0x5au8; 32];
        let chal = vec![0xa5u8; 32];
        let aug = augmented_challenge(NVME_AUTH_HASH_SHA256, &skey, &chal).unwrap();
        assert_eq!(
            hex::encode(aug),
            "653464639234e41feea14c125fc813753184e62968633ea04fe9fd0f9e9a068d"
        );
    }

    #[test]
    fn host_response_r1_matches_reference_vector() {
        let key: Vec<u8> = (0..32).collect();
        let c1 = vec![0xA5u8; 32];
        let r1 = dhchap_response(&ResponseInput {
            transformed_key: &key,
            hash_id: NVME_AUTH_HASH_SHA256,
            challenge: &c1,
            seqnum: 0x0102_0304,
            t_id: 0x1122,
            sc_c: 0,
            label: LABEL_HOST,
            nqn_first: NQN_HOST,
            nqn_second: NQN_SUB,
        })
        .unwrap();
        assert_eq!(
            hex::encode(r1),
            "46457a3a28eb393208bfab085cd515f09fe7029a52f1313706d6c09e08057085"
        );
    }

    #[test]
    fn ctrl_response_r2_matches_reference_vector() {
        let key: Vec<u8> = (0..48).collect();
        let c2 = vec![0x3cu8; 48];
        let r2 = dhchap_response(&ResponseInput {
            transformed_key: &key,
            hash_id: NVME_AUTH_HASH_SHA384,
            challenge: &c2,
            seqnum: 0x0a0b_0c0d,
            t_id: 0x1122,
            sc_c: 0,
            label: LABEL_CONTROLLER,
            nqn_first: NQN_SUB,
            nqn_second: NQN_HOST,
        })
        .unwrap();
        assert_eq!(
            hex::encode(r2),
            "0da224058467683e34284b37c2836060ef55bfc8c3312f1d2df8e694d5c4252d\
             2e217d6364794ab252579d9d51b70234"
        );
    }

    #[test]
    fn responses_equal_is_constant_time_eq() {
        assert!(responses_equal(&[1, 2, 3], &[1, 2, 3]));
        assert!(!responses_equal(&[1, 2, 3], &[1, 2, 4]));
        assert!(!responses_equal(&[1, 2, 3], &[1, 2]));
    }

    // FFDHE: each group's prime parses, is prime, has the RFC 7919
    // top/bottom-64-bits-all-ones structure, and a two-party agreement
    // yields equal session keys.
    fn ffdhe_agreement(dhgid: u8, prime_bytes: usize) {
        let a = DhKeypair::generate(dhgid).unwrap();
        let b = DhKeypair::generate(dhgid).unwrap();
        assert_eq!(a.prime_len(), prime_bytes);
        let a_pub = a.public_value().unwrap();
        let b_pub = b.public_value().unwrap();
        assert_eq!(a_pub.len(), prime_bytes);
        assert_eq!(b_pub.len(), prime_bytes);
        let ka = a.session_key(&b_pub, NVME_AUTH_HASH_SHA256).unwrap();
        let kb = b.session_key(&a_pub, NVME_AUTH_HASH_SHA256).unwrap();
        assert_eq!(ka, kb, "DH session keys must agree");
        assert_eq!(ka.len(), 32);
    }

    #[test]
    fn ffdhe_groups_agree() {
        ffdhe_agreement(NVME_AUTH_DHGROUP_2048, 256);
        ffdhe_agreement(NVME_AUTH_DHGROUP_3072, 384);
        ffdhe_agreement(NVME_AUTH_DHGROUP_4096, 512);
        ffdhe_agreement(NVME_AUTH_DHGROUP_6144, 768);
        ffdhe_agreement(NVME_AUTH_DHGROUP_8192, 1024);
    }

    #[test]
    fn ffdhe_primes_match_canonical_rfc7919() {
        // SHA-256 of the canonical RFC 7919 prime (big-endian), as
        // extracted from OpenSSL's built-in ffdhe tables. An exact
        // match proves the embedded hex constant is the right prime and
        // un-corrupted (e.g. by the string-continuation wrapping) — far
        // faster and stricter than a Miller-Rabin probe.
        let expected: [(u8, usize, &str); 5] = [
            (
                NVME_AUTH_DHGROUP_2048,
                256,
                "9cd3b7f336872f46c09428d1bbc19877a4d440512cda8d1c1cf0cd6e33698966",
            ),
            (
                NVME_AUTH_DHGROUP_3072,
                384,
                "0eaf67db3a839156d5013494a5318a772b5697d270d721f37f092efc69ea5a17",
            ),
            (
                NVME_AUTH_DHGROUP_4096,
                512,
                "4648414224ac881b3d0dc59b466f96d06a558278776807797ecf1f66ff397b3e",
            ),
            (
                NVME_AUTH_DHGROUP_6144,
                768,
                "227ac9066b3ddd9e193670cda2388fa884f65ba0cf98b742d1fe77a6687c79c7",
            ),
            (
                NVME_AUTH_DHGROUP_8192,
                1024,
                "770b14efaf6f049929c523113b3fa99a8d11dab1b18af3609590122075d19833",
            ),
        ];
        for (gid, bytes, sha) in expected {
            let p = BigNum::from_hex_str(ffdhe_prime_hex(gid).unwrap()).unwrap();
            let be = p.to_vec();
            assert_eq!(be.len(), bytes, "prime byte length");
            // RFC 7919 structure: top and bottom 64 bits are all ones.
            assert_eq!(&be[..8], &[0xFFu8; 8]);
            assert_eq!(&be[be.len() - 8..], &[0xFFu8; 8]);
            let digest = openssl::hash::hash(MessageDigest::sha256(), &be).unwrap();
            assert_eq!(hex::encode(digest), sha, "ffdhe prime must match RFC 7919");
        }
    }

    #[test]
    fn unknown_dhgroup_rejected() {
        assert!(matches!(
            DhKeypair::generate(0xFF),
            Err(DhchapError::UnknownDhGroup(0xFF))
        ));
        // NULL group has no keypair.
        assert!(matches!(
            DhKeypair::generate(0x00),
            Err(DhchapError::UnknownDhGroup(0x00))
        ));
    }

    #[test]
    fn session_key_rejects_out_of_range_peer() {
        let a = DhKeypair::generate(NVME_AUTH_DHGROUP_2048).unwrap();
        // peer public = 1 is out of range.
        let mut one = vec![0u8; 256];
        one[255] = 1;
        assert!(matches!(
            a.session_key(&one, NVME_AUTH_HASH_SHA256),
            Err(DhchapError::BadPeerPublic)
        ));
    }

    #[test]
    fn end_to_end_with_dh_augmented_response() {
        // A full DH-augmented R1: host and controller derive the same
        // session key, augment the challenge identically, and the host
        // response validates against the controller's recomputation.
        let ctrl = DhKeypair::generate(NVME_AUTH_DHGROUP_2048).unwrap();
        let host = DhKeypair::generate(NVME_AUTH_DHGROUP_2048).unwrap();
        let hash = NVME_AUTH_HASH_SHA256;

        let ctrl_skey = ctrl
            .session_key(&host.public_value().unwrap(), hash)
            .unwrap();
        let host_skey = host
            .session_key(&ctrl.public_value().unwrap(), hash)
            .unwrap();
        assert_eq!(ctrl_skey, host_skey);

        let c1 = vec![0x42u8; 32];
        let secret = DhchapKey {
            raw: (0..32).collect(),
            hash: 0,
        };
        let tk = transform_key(&secret, NQN_HOST).unwrap();

        // Host computes R1 over the augmented challenge.
        let host_aug = augmented_challenge(hash, &host_skey, &c1).unwrap();
        let r1 = dhchap_response(&ResponseInput {
            transformed_key: &tk,
            hash_id: hash,
            challenge: &host_aug,
            seqnum: 7,
            t_id: 3,
            sc_c: 0,
            label: LABEL_HOST,
            nqn_first: NQN_HOST,
            nqn_second: NQN_SUB,
        })
        .unwrap();

        // Controller recomputes the expected R1 and they match.
        let ctrl_aug = augmented_challenge(hash, &ctrl_skey, &c1).unwrap();
        let expected = dhchap_response(&ResponseInput {
            transformed_key: &tk,
            hash_id: hash,
            challenge: &ctrl_aug,
            seqnum: 7,
            t_id: 3,
            sc_c: 0,
            label: LABEL_HOST,
            nqn_first: NQN_HOST,
            nqn_second: NQN_SUB,
        })
        .unwrap();
        assert!(responses_equal(&r1, &expected));
    }
}
