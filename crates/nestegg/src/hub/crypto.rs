use anyhow::{Result, anyhow};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn hash(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn matches(value: &str, digest: &str) -> bool {
    bool::from(hash(value).as_bytes().ct_eq(digest.as_bytes()))
}

pub fn random_secret() -> String {
    crate::ui::new_token()
}

pub fn code() -> String {
    // A power-of-two alphabet avoids modulo bias. No I, O, 0 or 1.
    const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let bytes: [u8; 12] = rand::random();
    let mut s = String::with_capacity(14);
    for (i, byte) in bytes.iter().enumerate() {
        if i == 4 || i == 8 {
            s.push('-');
        }
        s.push(ALPHABET[(byte & 31) as usize] as char);
    }
    s
}

pub fn password_hash(password: &[u8]) -> Result<String> {
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>())
        .map_err(|_| anyhow!("create password salt"))?;
    Argon2::default()
        .hash_password(password, &salt)
        .map(|h| h.to_string())
        .map_err(|_| anyhow!("hash administrator password"))
}

pub fn password_matches(password: &[u8], hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(password, &h).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_and_credentials() {
        let a = code();
        assert_eq!(a.len(), 14);
        assert_eq!(
            a.split('-').map(str::len).collect::<Vec<_>>(),
            vec![4, 4, 4]
        );
        assert!(!a.contains(['0', '1', 'I', 'O']));
        assert_ne!(a, code());
        assert!(matches(&a, &hash(&a)));
        assert!(!matches("wrong", &hash(&a)));
        assert_eq!(random_secret().len(), 64);
    }
    #[test]
    fn argon2id_hashes_are_salted() {
        let h = password_hash(b"test password").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(password_matches(b"test password", &h));
        assert!(!password_matches(b"wrong", &h));
        assert_ne!(h, password_hash(b"test password").unwrap());
    }
}
