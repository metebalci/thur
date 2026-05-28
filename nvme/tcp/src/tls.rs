// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! TLS-PSK helpers for NVMe/TCP (NVMe-TCP §3.6.1.5).
//!
//! Two distinct PSK strings the spec keeps separate; this module
//! parses and derives between them.
//!
//! **PSK interchange format** (operator config / `nvme-cli` keyring):
//! ```text
//! NVMeTLSkey-<ver>:<hash>:<base64(key || crc32_le(key))>:
//! ```
//! - `<ver>` — single hex digit. `1` is what `nvme-cli` emits today.
//! - `<hash>` — two hex digits. `01` = SHA-256 (TLS_AES_128_GCM_SHA256);
//!   `02` = SHA-384 (TLS_AES_256_GCM_SHA384). These are the two
//!   cipher suites NVMe-TCP §3.6.1.5 mandates.
//! - base64 body = raw key bytes followed by a 4-byte little-endian
//!   CRC-32 of the key. The CRC is for typo detection — the actual
//!   value is the "Retained PSK" used in the next derivation.
//!
//! **PSK identity on the wire** (sent in the TLS 1.3 ClientHello
//! `pre_shared_key` extension):
//! ```text
//! NVMe<ver>R<hash> <hostnqn> <subnqn> [<digest>]
//! ```
//! - `<ver>` — `0` (no digest) or `1` (with digest, defense-in-depth).
//! - `<hash>` — `01` / `02` as above.
//! - `<digest>` — present only for v1. Validated against the PSK
//!   binding `HMAC(retained_psk, hostnqn || subnqn)` (libnvme:
//!   `derive_psk_digest`).
//!
//! **TLS PSK derivation** (libnvme: `derive_tls_key`):
//! ```text
//! PRK     = HKDF-Extract(salt = <hash_len zero bytes>, IKM = RetainedPSK)
//! TLS PSK = HKDF-Expand-Label(PRK, "nvme-tls-psk",
//!                             context = PskIdentity, L = hash_len)
//! ```
//! `HKDF-Expand-Label` is the TLS 1.3 construction (RFC 8446 §7.1):
//! `HKDF-Expand(PRK, HkdfLabel, L)` where `HkdfLabel` is the
//! length-prefixed serialization of `length`, `"tls13 " + label`,
//! and `context`.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::sign::Signer;
use s2n_tls::callbacks::{ClientHelloCallback, ConnectionFuture};
use s2n_tls::config::Config;
use s2n_tls::connection::{Builder as ConnBuilder, Connection};
use s2n_tls::enums::{Mode, PskHmac};
use s2n_tls::psk::{Builder as S2nPskBuilder, Psk};
use s2n_tls::security;
use s2n_tls_tokio::TlsAcceptor;

use crate::identity::{NvmetcpPsksFile, PskTable};

/// Hash algorithm tied to a cipher-suite code per NVMe-TCP
/// §3.6.1.5. `01` = SHA-256 (TLS_AES_128_GCM_SHA256), `02` = SHA-384
/// (TLS_AES_256_GCM_SHA384). No other values are spec-legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashKind {
    Sha256,
    Sha384,
}

impl HashKind {
    /// Output length in bytes — also the length of the retained PSK
    /// and of the derived TLS PSK.
    #[allow(clippy::len_without_is_empty)] // hash output is never empty
    pub fn len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha384 => 48,
        }
    }

    /// Cipher-suite code (`01` / `02`) as a 2-char ASCII string.
    fn code_str(self) -> &'static str {
        match self {
            Self::Sha256 => "01",
            Self::Sha384 => "02",
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0x01 => Some(Self::Sha256),
            0x02 => Some(Self::Sha384),
            _ => None,
        }
    }

    fn message_digest(self) -> MessageDigest {
        match self {
            Self::Sha256 => MessageDigest::sha256(),
            Self::Sha384 => MessageDigest::sha384(),
        }
    }
}

/// A parsed `NVMeTLSkey-...` interchange string. `configured_psk` is
/// the raw key bytes after CRC-32 validation. To derive the Retained
/// PSK (used for the v1 identity digest and as input to the TLS PSK
/// derivation), call [`derive_retained_psk`] with the host NQN.
#[derive(Debug, Clone)]
pub struct ParsedInterchangeKey {
    pub version: u8,
    pub hash: HashKind,
    pub configured_psk: Vec<u8>,
}

/// Parse the operator-facing interchange format.
///
/// Returns an error on bad prefix, unknown hash code, malformed
/// base64, length mismatch, or CRC-32 mismatch.
pub fn parse_interchange_key(s: &str) -> Result<ParsedInterchangeKey, PskError> {
    // Format: NVMeTLSkey-X:NN:base64:
    let stripped = s
        .strip_prefix("NVMeTLSkey-")
        .ok_or(PskError::BadInterchangePrefix)?;
    let trimmed = stripped
        .strip_suffix(':')
        .ok_or(PskError::BadInterchangeFormat)?;
    let mut parts = trimmed.splitn(3, ':');
    let ver_str = parts.next().ok_or(PskError::BadInterchangeFormat)?;
    let hash_str = parts.next().ok_or(PskError::BadInterchangeFormat)?;
    let b64 = parts.next().ok_or(PskError::BadInterchangeFormat)?;
    if parts.next().is_some() {
        return Err(PskError::BadInterchangeFormat);
    }

    let version = u8::from_str_radix(ver_str, 16).map_err(|_| PskError::BadInterchangeFormat)?;
    let hash_code = u8::from_str_radix(hash_str, 16).map_err(|_| PskError::BadInterchangeFormat)?;
    let hash = HashKind::from_code(hash_code).ok_or(PskError::UnknownHashCode(hash_code))?;

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|_| PskError::BadInterchangeBase64)?;

    // Body is key_bytes || crc32_le(key_bytes). Expect hash.len() + 4.
    let expected = hash.len() + 4;
    if decoded.len() != expected {
        return Err(PskError::InterchangeLengthMismatch {
            expected,
            got: decoded.len(),
        });
    }
    let (key, crc_bytes) = decoded.split_at(hash.len());
    // safe: split_at(hash.len()) leaves exactly 4 bytes (we verified
    // the total length above as hash.len() + 4).
    let crc_arr: [u8; 4] = crc_bytes
        .try_into()
        .map_err(|_| PskError::BadInterchangeFormat)?;
    let want_crc = u32::from_le_bytes(crc_arr);
    let got_crc = crc32fast::hash(key);
    if want_crc != got_crc {
        return Err(PskError::CrcMismatch);
    }

    Ok(ParsedInterchangeKey {
        version,
        hash,
        configured_psk: key.to_vec(),
    })
}

/// Derive the **Retained PSK** from the Configured PSK + host NQN
/// (libnvme: `derive_retained_key`). The Retained PSK is host-bound
/// — different host NQNs produce different Retained PSKs from the
/// same Configured PSK. The chain is:
///
/// ```text
/// PRK         = HKDF-Extract(salt = <hash_len zero bytes>, IKM = ConfiguredPSK)
/// RetainedPSK = HKDF-Expand-Label(PRK, "HostNQN", context = hostnqn, L = hash_len)
/// ```
///
/// The Retained PSK is what's stored in the Linux kernel `.nvme`
/// keyring by `nvme gen-tls-key --insert`, and what's input to both
/// the v1 PSK-identity digest and [`derive_tls_psk`].
pub fn derive_retained_psk(
    configured_psk: &[u8],
    hash: HashKind,
    hostnqn: &str,
) -> Result<Vec<u8>, PskError> {
    let salt = vec![0u8; hash.len()];
    let prk = hkdf_extract(&salt, configured_psk, hash)?;
    let label = build_hkdf_label("HostNQN", hostnqn.as_bytes(), hash.len() as u16);
    hkdf_expand(&prk, &label, hash, hash.len())
}

/// A parsed wire-format PSK identity. The digest is present only on
/// v1 identities; the lookup table is keyed on `hostnqn` regardless.
#[derive(Debug, Clone)]
pub struct ParsedPskIdentity<'a> {
    pub version: u8,
    pub hash: HashKind,
    pub hostnqn: &'a str,
    pub subnqn: &'a str,
    pub digest: Option<&'a str>,
}

/// Parse the on-the-wire PSK identity string. Returns an error on
/// bad prefix, malformed field structure, or unknown hash code.
pub fn parse_psk_identity(s: &str) -> Result<ParsedPskIdentity<'_>, PskError> {
    // Format: NVMe<ver>R<hash> <hostnqn> <subnqn> [<digest>]
    let rest = s.strip_prefix("NVMe").ok_or(PskError::BadIdentityPrefix)?;
    // version is exactly one ASCII digit
    if rest.len() < 4 {
        return Err(PskError::BadIdentityFormat);
    }
    let ver_byte = rest.as_bytes()[0];
    if !ver_byte.is_ascii_digit() {
        return Err(PskError::BadIdentityFormat);
    }
    let version = ver_byte - b'0';
    if rest.as_bytes()[1] != b'R' {
        return Err(PskError::BadIdentityFormat);
    }
    // hash is two ASCII hex digits
    let hash_str = &rest[2..4];
    let hash_code = u8::from_str_radix(hash_str, 16).map_err(|_| PskError::BadIdentityFormat)?;
    let hash = HashKind::from_code(hash_code).ok_or(PskError::UnknownHashCode(hash_code))?;
    let body = &rest[4..];
    if !body.starts_with(' ') {
        return Err(PskError::BadIdentityFormat);
    }
    let body = &body[1..];

    let mut fields = body.split(' ');
    let hostnqn = fields.next().ok_or(PskError::BadIdentityFormat)?;
    let subnqn = fields.next().ok_or(PskError::BadIdentityFormat)?;
    let digest = fields.next();
    if fields.next().is_some() {
        return Err(PskError::BadIdentityFormat);
    }
    if hostnqn.is_empty() || subnqn.is_empty() {
        return Err(PskError::BadIdentityFormat);
    }
    if version == 1 && digest.is_none() {
        return Err(PskError::V1MissingDigest);
    }
    if version == 0 && digest.is_some() {
        return Err(PskError::V0UnexpectedDigest);
    }

    Ok(ParsedPskIdentity {
        version,
        hash,
        hostnqn,
        subnqn,
        digest,
    })
}

/// Build the v0 PSK identity string for a given (hash, hostnqn, subnqn).
/// Used by tests and to validate the identity bytes the client sent
/// against what we'd construct independently.
pub fn build_psk_identity_v0(hash: HashKind, hostnqn: &str, subnqn: &str) -> String {
    format!("NVMe0R{} {} {}", hash.code_str(), hostnqn, subnqn)
}

/// Build the v1 PSK identity string including the digest binding.
pub fn build_psk_identity_v1(
    retained_psk: &[u8],
    hash: HashKind,
    hostnqn: &str,
    subnqn: &str,
) -> Result<String, PskError> {
    let digest = derive_psk_digest(retained_psk, hash, hostnqn, subnqn)?;
    Ok(format!(
        "NVMe1R{} {} {} {}",
        hash.code_str(),
        hostnqn,
        subnqn,
        digest
    ))
}

/// Compute the v1 PSK identity digest (libnvme: `derive_psk_digest`).
///
/// libnvme HMACs `hostnqn || " " || subnqn || " " || "NVMe-over-Fabrics"`
/// under the retained-PSK key, then base64-encodes (STANDARD alphabet
/// with padding). Must match byte-for-byte or tlshd's PSK identity
/// lookup against our pre-registered entry misses.
pub fn derive_psk_digest(
    retained_psk: &[u8],
    hash: HashKind,
    hostnqn: &str,
    subnqn: &str,
) -> Result<String, PskError> {
    const HMAC_SEED: &[u8] = b"NVMe-over-Fabrics";
    let key = openssl::pkey::PKey::hmac(retained_psk).map_err(PskError::Openssl)?;
    let mut signer = Signer::new(hash.message_digest(), &key).map_err(PskError::Openssl)?;
    signer
        .update(hostnqn.as_bytes())
        .map_err(PskError::Openssl)?;
    signer.update(b" ").map_err(PskError::Openssl)?;
    signer
        .update(subnqn.as_bytes())
        .map_err(PskError::Openssl)?;
    signer.update(b" ").map_err(PskError::Openssl)?;
    signer.update(HMAC_SEED).map_err(PskError::Openssl)?;
    let mac = signer.sign_to_vec().map_err(PskError::Openssl)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(mac))
}

/// Derive the TLS PSK that s2n-tls registers as the secret for a
/// given PSK identity (libnvme: `derive_tls_key`).
///
/// ```text
/// PRK     = HKDF-Extract(salt = <hash.len() zero bytes>, IKM = retained_psk)
/// TLS PSK = HKDF-Expand-Label(PRK, "nvme-tls-psk",
///                             context = ctx_bytes, L = hash.len())
/// ```
///
/// The exact `ctx_bytes` depend on the identity version:
/// - v0: the full wire identity string (`"NVMe0R<hash> hostnqn subnqn"`)
/// - v1: just `"<hash> <digest>"` (2-char cipher code, space, base64 digest)
///
/// libnvme deliberately uses different context for v1 — the digest
/// already binds hostnqn+subnqn via HMAC, so re-including the full
/// identity in the HKDF context would be redundant. Mismatch here
/// produces an unparseable PSK binder on the wire.
pub fn derive_tls_psk(
    retained_psk: &[u8],
    hash: HashKind,
    ctx_bytes: &[u8],
) -> Result<Vec<u8>, PskError> {
    let salt = vec![0u8; hash.len()];
    let prk = hkdf_extract(&salt, retained_psk, hash)?;
    let label = build_hkdf_label("nvme-tls-psk", ctx_bytes, hash.len() as u16);
    hkdf_expand(&prk, &label, hash, hash.len())
}

/// HKDF-Extract(salt, ikm) per RFC 5869 §2.2.
///
/// Implemented as raw HMAC rather than via openssl's HKDF mode
/// helpers because the HKDF-mode salt handling has subtle differences
/// (empty vs all-zeros vs unset) that produced mismatches against
/// libnvme. Raw HMAC is what libnvme effectively does and matches
/// byte-for-byte.
fn hkdf_extract(salt: &[u8], ikm: &[u8], hash: HashKind) -> Result<Vec<u8>, PskError> {
    let key = openssl::pkey::PKey::hmac(salt).map_err(PskError::Openssl)?;
    let mut signer = Signer::new(hash.message_digest(), &key).map_err(PskError::Openssl)?;
    signer.update(ikm).map_err(PskError::Openssl)?;
    signer.sign_to_vec().map_err(PskError::Openssl)
}

/// HKDF-Expand(prk, info, len) per RFC 5869 §2.3 (raw HMAC iteration:
/// T(0)=empty, T(i)=HMAC(prk, T(i-1)||info||i)). Same reason as
/// `hkdf_extract` — bypasses openssl's HKDF mode wrapper for
/// byte-exact interop with libnvme.
fn hkdf_expand(prk: &[u8], info: &[u8], hash: HashKind, len: usize) -> Result<Vec<u8>, PskError> {
    let hash_len = hash.len();
    let n = len.div_ceil(hash_len);
    if n > 255 {
        return Err(PskError::HkdfExpandTooLong);
    }
    let key = openssl::pkey::PKey::hmac(prk).map_err(PskError::Openssl)?;
    let mut okm = Vec::with_capacity(n * hash_len);
    let mut t: Vec<u8> = Vec::new();
    for i in 1..=n as u8 {
        let mut signer = Signer::new(hash.message_digest(), &key).map_err(PskError::Openssl)?;
        signer.update(&t).map_err(PskError::Openssl)?;
        signer.update(info).map_err(PskError::Openssl)?;
        signer.update(&[i]).map_err(PskError::Openssl)?;
        t = signer.sign_to_vec().map_err(PskError::Openssl)?;
        okm.extend_from_slice(&t);
    }
    okm.truncate(len);
    Ok(okm)
}

/// Build the `HkdfLabel` structure per RFC 8446 §7.1:
///
/// ```text
/// struct {
///     uint16 length;
///     opaque label<7..255> = "tls13 " + label;
///     opaque context<0..255>;
/// } HkdfLabel;
/// ```
fn build_hkdf_label(label: &str, context: &[u8], out_len: u16) -> Vec<u8> {
    let full_label = format!("tls13 {}", label);
    let mut buf = Vec::with_capacity(2 + 1 + full_label.len() + 1 + context.len());
    buf.extend_from_slice(&out_len.to_be_bytes());
    buf.push(full_label.len() as u8);
    buf.extend_from_slice(full_label.as_bytes());
    buf.push(context.len() as u8);
    buf.extend_from_slice(context);
    buf
}

/// Free function: derive every TLS PSK (v0 + v1 identities × current +
/// optional previous-key during grace) for the host entries in a
/// parsed `NvmetcpPsksFile`, using `subnqn` as the subsystem NQN. The
/// `ClientHelloCallback` calls this once per handshake; the boot-time
/// log line in `build_psk_acceptor` does NOT — it just verifies the
/// file parses + derivation succeeds before opening the listener.
pub fn derive_all_psks(file: &NvmetcpPsksFile, subnqn: &str) -> Result<Vec<Psk>, PskError> {
    let table = PskTable::from_file(file).map_err(|e| PskError::Identity(e.to_string()))?;
    let mut psks: Vec<Psk> = Vec::with_capacity(table.len() * 4);
    for (host_nqn, entries) in table.iter() {
        // Per host: 1 entry in steady state, 2 during a rotation
        // grace window (current + previous). Each yields 2 PSKs
        // (v0 + v1 identity forms), so the per-host cap is 4.
        for entry in entries {
            let hash = entry.hash;
            // Per nvme-tls(8) the v1 digest and the TLS PSK are both
            // computed from the Retained PSK, not the Configured PSK
            // in the operator's `NVMeTLSkey-...` string. The Retained
            // PSK is also what the Linux kernel `.nvme` keyring stores.
            let retained = derive_retained_psk(&entry.configured_psk, hash, host_nqn)?;

            // v0 identity (no digest). TLS PSK context = full identity.
            let id_v0 = build_psk_identity_v0(hash, host_nqn, subnqn);
            let tls_psk_v0 = derive_tls_psk(&retained, hash, id_v0.as_bytes())?;
            psks.push(make_psk(id_v0.as_bytes(), &tls_psk_v0, hash)?);

            // v1 identity (HMAC digest binding hostnqn+subnqn). TLS
            // PSK context is `"<cipher2> <digest>"` per libnvme — NOT
            // the full identity. Mismatch produces an unparseable PSK
            // binder on the wire and the handshake silently fails.
            let digest = derive_psk_digest(&retained, hash, host_nqn, subnqn)?;
            let id_v1 = format!(
                "NVMe1R{} {} {} {}",
                hash.code_str(),
                host_nqn,
                subnqn,
                digest
            );
            let ctx_v1 = format!("{} {}", hash.code_str(), digest);
            let tls_psk_v1 = derive_tls_psk(&retained, hash, ctx_v1.as_bytes())?;
            psks.push(make_psk(id_v1.as_bytes(), &tls_psk_v1, hash)?);
        }
    }
    Ok(psks)
}

/// `ClientHelloCallback` that loads `nvmetcp-psks.json` and derives
/// every PSK on every TLS handshake. Lets operator CLI verbs edit
/// the file in-place — the next handshake sees the new state with
/// no daemon restart and no reload primitive.
///
/// Cost per handshake: 1 file read (sub-KB, page-cache-hot) + N
/// HKDF derivations + 2N..4N `append_psk` calls where N = registered
/// hosts. Identical to the boot-time pre-derivation work the old
/// `NvmePskBuilder` was already doing per accept — the difference
/// is the source: current file contents instead of a boot snapshot.
///
/// Parse / derive failure on a given handshake fails that one
/// handshake (s2n-tls returns an error → `accept_loop` logs at
/// `WARN` with peer + reason and closes the connection). The
/// daemon keeps running; previously-good PSKs stay good.
struct NvmePskCallback {
    path: PathBuf,
    subnqn: String,
}

impl ClientHelloCallback for NvmePskCallback {
    fn on_client_hello(
        &self,
        connection: &mut Connection,
    ) -> Result<Option<Pin<Box<dyn ConnectionFuture>>>, s2n_tls::error::Error> {
        let file = NvmetcpPsksFile::load(&self.path).map_err(|e| {
            tracing::warn!(
                identity_file = %self.path.display(),
                error = %e,
                "nvme-tcp: PSK identity file load failed at ClientHello",
            );
            s2n_tls::error::Error::application(e.to_string().into())
        })?;
        let psks = derive_all_psks(&file, &self.subnqn).map_err(|e| {
            tracing::warn!(
                identity_file = %self.path.display(),
                error = %e,
                "nvme-tcp: PSK derivation failed at ClientHello",
            );
            s2n_tls::error::Error::application(e.to_string().into())
        })?;
        for psk in &psks {
            connection.append_psk(psk)?;
        }
        Ok(None)
    }
}

/// Thin `ConnBuilder` that delegates straight to the inner [`Config`]
/// — no per-connection PSK loop, because that work now happens in
/// the registered [`NvmePskCallback`] on every handshake.
#[derive(Clone)]
pub struct NvmePskConnBuilder {
    config: Config,
}

impl ConnBuilder for NvmePskConnBuilder {
    type Output = Connection;

    fn build_connection(&self, mode: Mode) -> Result<Connection, s2n_tls::error::Error> {
        self.config.build_connection(mode)
    }
}

/// Type alias for the concrete TLS acceptor the daemon stores in
/// [`crate::server::ServerConfig`].
pub type NvmePskAcceptor = TlsAcceptor<NvmePskConnBuilder>;

/// Build the TLS acceptor for parse-on-handshake PSK lookup.
///
/// Registers a [`ClientHelloCallback`] that reads `path` and derives
/// every PSK on every handshake. The boot-time validation here just
/// confirms the file is parseable today — actual PSKs come from
/// whatever the file says at handshake time.
pub fn build_psk_acceptor(path: &Path, subnqn: &str) -> Result<NvmePskAcceptor, PskError> {
    // Boot-time sanity: confirm the file parses + at least one
    // derivation pass succeeds. Operators want a clear error
    // *before* the listener opens, not the first handshake.
    let initial_file =
        NvmetcpPsksFile::load(path).map_err(|e| PskError::Identity(e.to_string()))?;
    let _ = derive_all_psks(&initial_file, subnqn)?;

    let callback = NvmePskCallback {
        path: path.to_path_buf(),
        subnqn: subnqn.to_string(),
    };

    // TLS 1.3 only. The DEFAULT_TLS13 policy covers both NVMe-TCP
    // §3.6.1.5 mandated cipher suites (TLS_AES_128_GCM_SHA256,
    // TLS_AES_256_GCM_SHA384) and rejects TLS 1.2 fallback — host
    // negotiation that doesn't reach TLS 1.3 fails the handshake
    // cleanly rather than silently dropping to 1.2 ciphers.
    let mut config_b = Config::builder();
    config_b
        .set_security_policy(&security::DEFAULT_TLS13)
        .map_err(PskError::S2n)?;
    config_b
        .set_client_hello_callback(callback)
        .map_err(PskError::S2n)?;
    let config = config_b.build().map_err(PskError::S2n)?;

    Ok(TlsAcceptor::new(NvmePskConnBuilder { config }))
}

fn make_psk(identity: &[u8], secret: &[u8], hash: HashKind) -> Result<Psk, PskError> {
    let mut b = S2nPskBuilder::new().map_err(PskError::S2n)?;
    b.set_identity(identity).map_err(PskError::S2n)?;
    b.set_secret(secret).map_err(PskError::S2n)?;
    let hmac = match hash {
        HashKind::Sha256 => PskHmac::SHA256,
        HashKind::Sha384 => PskHmac::SHA384,
    };
    b.set_hmac(hmac).map_err(PskError::S2n)?;
    b.build().map_err(PskError::S2n)
}

#[derive(Debug, thiserror::Error)]
pub enum PskError {
    #[error("interchange string missing NVMeTLSkey- prefix")]
    BadInterchangePrefix,
    #[error("interchange string format invalid")]
    BadInterchangeFormat,
    #[error("interchange base64 body malformed")]
    BadInterchangeBase64,
    #[error("interchange body length {got} != expected {expected}")]
    InterchangeLengthMismatch { expected: usize, got: usize },
    #[error("interchange CRC-32 mismatch")]
    CrcMismatch,
    #[error("PSK identity missing NVMe prefix")]
    BadIdentityPrefix,
    #[error("PSK identity format invalid")]
    BadIdentityFormat,
    #[error("unknown hash code 0x{0:02x}")]
    UnknownHashCode(u8),
    #[error("v1 PSK identity missing digest field")]
    V1MissingDigest,
    #[error("v0 PSK identity must not carry a digest field")]
    V0UnexpectedDigest,
    #[error("openssl: {0}")]
    Openssl(#[from] openssl::error::ErrorStack),
    #[error("s2n-tls: {0}")]
    S2n(#[from] s2n_tls::error::Error),
    #[error("identity file: {0}")]
    Identity(String),
    #[error("HKDF-Expand output length exceeds 255 * hash_len")]
    HkdfExpandTooLong,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: build a known-good interchange string by hand and
    /// confirm parse_interchange_key recovers it byte-exact.
    #[test]
    fn parse_interchange_sha256_round_trip() {
        let key = vec![0xAB; 32];
        let crc = crc32fast::hash(&key);
        let mut body = key.clone();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let s = format!("NVMeTLSkey-1:01:{}:", b64);

        let parsed = parse_interchange_key(&s).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.hash, HashKind::Sha256);
        assert_eq!(parsed.configured_psk, key);
    }

    #[test]
    fn parse_interchange_sha384_round_trip() {
        let key = vec![0xCD; 48];
        let crc = crc32fast::hash(&key);
        let mut body = key.clone();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let s = format!("NVMeTLSkey-1:02:{}:", b64);

        let parsed = parse_interchange_key(&s).unwrap();
        assert_eq!(parsed.hash, HashKind::Sha384);
        assert_eq!(parsed.configured_psk, key);
    }

    #[test]
    fn parse_interchange_bad_prefix_rejected() {
        assert!(matches!(
            parse_interchange_key("NotAKey-1:01:abc:"),
            Err(PskError::BadInterchangePrefix)
        ));
    }

    #[test]
    fn parse_interchange_unknown_hash_rejected() {
        let key = vec![0xAB; 32];
        let crc = crc32fast::hash(&key);
        let mut body = key.clone();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let s = format!("NVMeTLSkey-1:09:{}:", b64);
        assert!(matches!(
            parse_interchange_key(&s),
            Err(PskError::UnknownHashCode(0x09))
        ));
    }

    #[test]
    fn parse_interchange_bad_crc_rejected() {
        let key = vec![0xAB; 32];
        let bad_crc: u32 = 0xDEAD_BEEF;
        let mut body = key.clone();
        body.extend_from_slice(&bad_crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let s = format!("NVMeTLSkey-1:01:{}:", b64);
        assert!(matches!(
            parse_interchange_key(&s),
            Err(PskError::CrcMismatch)
        ));
    }

    #[test]
    fn parse_interchange_wrong_length_rejected() {
        // SHA-256 expects 32 + 4. Use 30 + 4 to trip the length check.
        let key = vec![0xAB; 30];
        let crc = crc32fast::hash(&key);
        let mut body = key.clone();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let s = format!("NVMeTLSkey-1:01:{}:", b64);
        assert!(matches!(
            parse_interchange_key(&s),
            Err(PskError::InterchangeLengthMismatch { .. })
        ));
    }

    #[test]
    fn parse_identity_v0_round_trip() {
        let s = "NVMe0R01 nqn.2014-08.org.nvmexpress:uuid:abc nqn.2025-10.com.metebalci:thurvsa";
        let parsed = parse_psk_identity(s).unwrap();
        assert_eq!(parsed.version, 0);
        assert_eq!(parsed.hash, HashKind::Sha256);
        assert_eq!(parsed.hostnqn, "nqn.2014-08.org.nvmexpress:uuid:abc");
        assert_eq!(parsed.subnqn, "nqn.2025-10.com.metebalci:thurvsa");
        assert_eq!(parsed.digest, None);
    }

    #[test]
    fn parse_identity_v1_round_trip() {
        let s = "NVMe1R02 nqn.host nqn.sub DIGESTBYTES";
        let parsed = parse_psk_identity(s).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.hash, HashKind::Sha384);
        assert_eq!(parsed.hostnqn, "nqn.host");
        assert_eq!(parsed.subnqn, "nqn.sub");
        assert_eq!(parsed.digest, Some("DIGESTBYTES"));
    }

    #[test]
    fn parse_identity_v1_missing_digest_rejected() {
        let s = "NVMe1R01 nqn.host nqn.sub";
        assert!(matches!(
            parse_psk_identity(s),
            Err(PskError::V1MissingDigest)
        ));
    }

    #[test]
    fn parse_identity_v0_with_digest_rejected() {
        let s = "NVMe0R01 nqn.host nqn.sub trailing";
        assert!(matches!(
            parse_psk_identity(s),
            Err(PskError::V0UnexpectedDigest)
        ));
    }

    #[test]
    fn parse_identity_bad_prefix_rejected() {
        assert!(matches!(
            parse_psk_identity("nvme0R01 a b"),
            Err(PskError::BadIdentityPrefix)
        ));
    }

    #[test]
    fn build_identity_v0_matches_format() {
        let s = build_psk_identity_v0(HashKind::Sha256, "nqn.host", "nqn.sub");
        assert_eq!(s, "NVMe0R01 nqn.host nqn.sub");
    }

    #[test]
    fn build_and_parse_v1_identity_round_trip() {
        let retained = vec![0x42; 32];
        let s = build_psk_identity_v1(&retained, HashKind::Sha256, "nqn.host", "nqn.sub").unwrap();
        let parsed = parse_psk_identity(&s).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.hostnqn, "nqn.host");
        assert_eq!(parsed.subnqn, "nqn.sub");
        assert!(parsed.digest.is_some());
    }

    /// HKDF-Extract with known RFC 5869 Test Case 1 vector
    /// (SHA-256). Confirms our HKDF plumbing matches the reference.
    #[test]
    fn hkdf_extract_rfc5869_test_case_1() {
        let ikm = hex::decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b").unwrap();
        let salt = hex::decode("000102030405060708090a0b0c").unwrap();
        let expected_prk =
            hex::decode("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5")
                .unwrap();
        let prk = hkdf_extract(&salt, &ikm, HashKind::Sha256).unwrap();
        assert_eq!(prk, expected_prk);
    }

    #[test]
    fn hkdf_label_structure_matches_rfc8446() {
        // length=32, label="tls13 nvme-tls-psk" (18 bytes),
        // context="X" (1 byte)
        let label = build_hkdf_label("nvme-tls-psk", b"X", 32);
        assert_eq!(&label[0..2], &[0x00, 0x20]); // length = 32
        assert_eq!(label[2], 18); // label-len
        assert_eq!(&label[3..21], b"tls13 nvme-tls-psk");
        assert_eq!(label[21], 1); // ctx-len
        assert_eq!(&label[22..23], b"X");
    }

    /// Whole-pipeline regression: derive_tls_psk produces a
    /// deterministic 32-byte output for a fixed input. Re-running the
    /// test must give the same bytes. If openssl HKDF semantics ever
    /// drift, this catches it.
    #[test]
    fn derive_tls_psk_is_deterministic_sha256() {
        let retained = vec![0x11; 32];
        let identity = "NVMe0R01 nqn.host nqn.sub";
        let a = derive_tls_psk(&retained, HashKind::Sha256, identity.as_bytes()).unwrap();
        let b = derive_tls_psk(&retained, HashKind::Sha256, identity.as_bytes()).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
    }

    #[test]
    fn derive_tls_psk_different_identity_yields_different_key() {
        let retained = vec![0x11; 32];
        let a = derive_tls_psk(&retained, HashKind::Sha256, b"NVMe0R01 nqn.a nqn.s").unwrap();
        let b = derive_tls_psk(&retained, HashKind::Sha256, b"NVMe0R01 nqn.b nqn.s").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn derive_tls_psk_sha384_is_48_bytes() {
        let retained = vec![0x22; 48];
        let out = derive_tls_psk(&retained, HashKind::Sha384, b"NVMe0R02 h s").unwrap();
        assert_eq!(out.len(), 48);
    }

    fn make_interchange_sha256(key: &[u8]) -> String {
        let crc = crc32fast::hash(key);
        let mut body = key.to_vec();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        format!("NVMeTLSkey-1:01:{}:", b64)
    }

    #[test]
    fn derive_all_psks_doubles_per_host_for_v0_v1_identities() {
        use crate::identity::{NvmetcpPsksFile, PskEntry};
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![
                PskEntry {
                    host_nqn: "nqn.host.a".into(),
                    interchange_key: make_interchange_sha256(&[0xAA; 32]),
                    disabled: false,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
                PskEntry {
                    host_nqn: "nqn.host.b".into(),
                    interchange_key: make_interchange_sha256(&[0xBB; 32]),
                    disabled: false,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
            ],
        };
        let psks = derive_all_psks(&file, "nqn.subsystem.test").unwrap();
        // 2 hosts × (v0 + v1) = 4
        assert_eq!(psks.len(), 4);
    }

    #[test]
    fn derive_all_psks_doubles_again_for_grace_window() {
        use crate::identity::{NvmetcpPsksFile, PskEntry};
        use chrono::{Duration, Utc};
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![PskEntry {
                host_nqn: "nqn.host.a".into(),
                interchange_key: make_interchange_sha256(&[0xAA; 32]),
                disabled: false,
                volumes: None,
                previous_interchange_key: Some(make_interchange_sha256(&[0xBB; 32])),
                previous_expires_at: Some(Utc::now() + Duration::hours(1)),
            }],
        };
        let psks = derive_all_psks(&file, "nqn.subsystem.test").unwrap();
        // 1 host × 2 keys (current + previous) × (v0 + v1) = 4
        assert_eq!(psks.len(), 4);
    }

    #[test]
    fn derive_all_psks_skips_disabled() {
        use crate::identity::{NvmetcpPsksFile, PskEntry};
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![
                PskEntry {
                    host_nqn: "nqn.host.active".into(),
                    interchange_key: make_interchange_sha256(&[0xAA; 32]),
                    disabled: false,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
                PskEntry {
                    host_nqn: "nqn.host.off".into(),
                    interchange_key: make_interchange_sha256(&[0xBB; 32]),
                    disabled: true,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
            ],
        };
        let psks = derive_all_psks(&file, "nqn.subsystem.test").unwrap();
        // Only the active host yields PSKs: 1 × (v0 + v1) = 2
        assert_eq!(psks.len(), 2);
    }

    #[test]
    fn build_psk_acceptor_picks_up_file_edits() {
        // The acceptor is built once; the ClientHelloCallback is
        // expected to re-read the file on every handshake. This
        // test exercises the seam without doing a full TLS
        // handshake: we build the acceptor, save a NEW file to
        // the same path, then re-invoke `derive_all_psks` via the
        // same load path to confirm fresh state would be visible.
        let tmp = std::env::temp_dir().join(format!("tls-test-rotate-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        use crate::identity::{NvmetcpPsksFile, PskEntry};
        let initial = NvmetcpPsksFile {
            version: 1,
            psks: vec![PskEntry {
                host_nqn: "nqn.host.a".into(),
                interchange_key: make_interchange_sha256(&[0xAA; 32]),
                disabled: false,
                volumes: None,
                previous_interchange_key: None,
                previous_expires_at: None,
            }],
        };
        initial.save(&tmp).unwrap();

        let acceptor = build_psk_acceptor(&tmp, "nqn.subsystem.test").expect("initial build OK");
        // Drop the acceptor reference to make it clear we don't
        // need to rebuild — the callback inside still owns `path`.
        drop(acceptor);

        // Edit: add host.b, then verify a fresh load+derive sees it.
        let edited = NvmetcpPsksFile {
            version: 1,
            psks: vec![
                PskEntry {
                    host_nqn: "nqn.host.a".into(),
                    interchange_key: make_interchange_sha256(&[0xAA; 32]),
                    disabled: false,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
                PskEntry {
                    host_nqn: "nqn.host.b".into(),
                    interchange_key: make_interchange_sha256(&[0xBB; 32]),
                    disabled: false,
                    volumes: None,
                    previous_interchange_key: None,
                    previous_expires_at: None,
                },
            ],
        };
        edited.save(&tmp).unwrap();
        let reloaded = NvmetcpPsksFile::load(&tmp).unwrap();
        let psks = derive_all_psks(&reloaded, "nqn.subsystem.test").unwrap();
        assert_eq!(psks.len(), 4); // 2 hosts × (v0 + v1)

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn build_psk_acceptor_rejects_corrupt_file_at_boot() {
        let tmp = std::env::temp_dir().join(format!("tls-test-bad-{}.json", std::process::id()));
        std::fs::write(&tmp, b"{not json").unwrap();
        let r = build_psk_acceptor(&tmp, "nqn.subsystem.test");
        assert!(r.is_err());
        let _ = std::fs::remove_file(&tmp);
    }
}
