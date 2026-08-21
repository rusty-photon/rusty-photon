use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

use crate::error::{AuthError, Result};

/// Hash a plaintext password using Argon2id.
///
/// Returns the hash in PHC string format (e.g. `$argon2id$v=19$m=19456,t=2,p=1$...`).
///
/// # Errors
///
/// Returns [`AuthError::HashingError`] if the Argon2 hashing itself fails.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::HashingError(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against an Argon2id hash.
///
/// Returns `true` if the password matches the hash.
#[must_use]
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)] // argon2 hashing is too slow under Miri
    fn hash_then_verify_succeeds() {
        let hash = hash_password("test-password").unwrap();
        assert!(verify_password("test-password", &hash));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // argon2 hashing is too slow under Miri
    fn verify_with_wrong_password_fails() {
        let hash = hash_password("correct-password").unwrap();
        assert!(!verify_password("wrong-password", &hash));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // argon2 hashing is too slow under Miri
    fn hash_produces_argon2id_format() {
        let hash = hash_password("test").unwrap();
        assert!(hash.starts_with("$argon2id$"), "hash: {hash}");
    }

    #[test]
    fn verify_with_invalid_hash_returns_false() {
        assert!(!verify_password("test", "not-a-valid-hash"));
    }

    #[test]
    fn verify_with_empty_hash_returns_false() {
        assert!(!verify_password("test", ""));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // argon2 hashing is too slow under Miri
    fn different_hashes_for_same_password() {
        let hash1 = hash_password("same").unwrap();
        let hash2 = hash_password("same").unwrap();
        assert_ne!(hash1, hash2, "salts should differ");
        assert!(verify_password("same", &hash1));
        assert!(verify_password("same", &hash2));
    }
}
