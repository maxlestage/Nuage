//! SHA-256, HMAC-SHA256 and constant-time comparison (RustCrypto: pure Rust), shared by the
//! engine, the SQL layer, the server and the S3 client.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// HMAC-SHA256 of `data` under `key`.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Compares two secrets in constant time. Digests first: equal lengths, so not even the
/// length leaks.
#[must_use]
pub fn same_secret(a: &[u8], b: &[u8]) -> bool {
    let (x, y) = (sha256(a), sha256(b));
    x.iter().zip(&y).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn known_answers() {
        // FIPS 180-2 ("abc") and RFC 4231, test case 2.
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(same_secret(b"jeton", b"jeton"));
        assert!(!same_secret(b"jeton", b"jetons"));
    }
}
