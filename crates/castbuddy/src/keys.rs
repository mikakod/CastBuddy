//! Cryptographic key material for publish (stream) keys.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rand::Rng as _;
use sha2::{Digest, Sha256};

/// Crockford base32 (no I, L, O, U): unambiguous when copied or read aloud.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `n_bytes` of CSPRNG entropy encoded as Crockford base32.
/// 32 bytes -> 52 characters -> 256 bits of entropy.
/// (`rand::rng()` is a cryptographically secure, OS-seeded CSPRNG.)
pub fn secret(n_bytes: usize) -> String {
    let mut rng = rand::rng();
    let buf: Vec<u8> = (0..n_bytes).map(|_| rng.random()).collect();
    encode_base32(&buf)
}

fn encode_base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        acc = (acc << 8) | b as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// MediaMTX credential hash: `sha256:` + base64 of the raw SHA-256 digest.
/// MediaMTX only ever sees the hash, never the plaintext key.
pub fn mediamtx_hash(password: &str) -> String {
    let digest = Sha256::digest(password.as_bytes());
    format!("sha256:{}", STANDARD.encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_256_bits_of_base32() {
        let a = secret(32);
        assert_eq!(a.len(), 52, "32 bytes encode to 52 base32 characters");
        assert!(a.chars().all(|c| ALPHABET.contains(&(c as u8))));
        assert_ne!(a, secret(32), "two secrets must never collide");
    }

    #[test]
    fn hash_matches_known_vector() {
        // sha256("") base64 - canonical test vector.
        assert_eq!(
            mediamtx_hash(""),
            "sha256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }
}
