// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Argon2id password hashing + verification over self-describing PHC
//! strings. `Argon2::default()` is Argon2id v19 at OWASP-baseline
//! params (m=19456 KiB, t=2, p=1); the full PHC string carries the
//! algorithm, version, params and salt, so verification needs nothing
//! out of band.

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

/// Hash a plaintext password into an Argon2id PHC string
/// (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`). A fresh random
/// salt is drawn per call, so two hashes of the same password differ.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing admin password: {e}"))
}

/// Verify `candidate` against a stored PHC hash. Returns `false` on any
/// parse or verification failure — a malformed stored hash must not
/// authenticate anyone. The hash comparison is constant-time (Argon2
/// internal); callers should still avoid leaking *which* of username /
/// password was wrong (see the middleware).
pub fn verify_phc(phc: &str, candidate: &[u8]) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| Argon2::default().verify_password(candidate, &parsed).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips() {
        let phc = hash_password("correct horse battery").expect("hash");
        assert!(phc.starts_with("$argon2id$"), "PHC string: {phc}");
        assert!(verify_phc(&phc, b"correct horse battery"));
    }

    #[test]
    fn verify_rejects_the_wrong_password() {
        let phc = hash_password("correct horse battery").expect("hash");
        assert!(!verify_phc(&phc, b"Tr0ub4dor&3"));
        assert!(!verify_phc(&phc, b""));
    }

    #[test]
    fn two_hashes_of_the_same_password_differ_by_salt() {
        let a = hash_password("same-password-12").expect("hash a");
        let b = hash_password("same-password-12").expect("hash b");
        assert_ne!(a, b, "distinct salts must yield distinct PHC strings");
        // ...yet both verify against the original password.
        assert!(verify_phc(&a, b"same-password-12"));
        assert!(verify_phc(&b, b"same-password-12"));
    }

    #[test]
    fn verify_on_a_garbage_phc_is_false_not_panic() {
        assert!(!verify_phc("not-a-phc-string", b"whatever"));
        assert!(!verify_phc("", b"whatever"));
        assert!(!verify_phc("$argon2id$broken", b"whatever"));
    }
}
