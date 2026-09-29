//! HMAC-SHA256 and single-iteration PBKDF2-SHA256, exactly as yespower uses them.
//! Together they are well under 1% of a hash; clarity wins here.
use sha2::{Digest, Sha256};

/// Keyed HMAC state: inner and outer hashes with the padded key absorbed.
#[derive(Clone)]
struct Hmac {
    inner: Sha256,
    outer: Sha256,
}

impl Hmac {
    /// Keys up to one SHA-256 block (64 bytes), which is all yespower uses.
    fn new(key: &[u8]) -> Self {
        debug_assert!(key.len() <= 64);
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for (i, k) in key.iter().enumerate() {
            ipad[i] ^= k;
            opad[i] ^= k;
        }
        Self {
            inner: Sha256::new_with_prefix(ipad),
            outer: Sha256::new_with_prefix(opad),
        }
    }

    fn finish(self, message: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner;
        for part in message {
            inner.update(part);
        }
        let mut outer = self.outer;
        outer.update(inner.finalize());
        outer.finalize().into()
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    Hmac::new(key).finish(&[message])
}

/// PBKDF2-HMAC-SHA256 with one iteration and an output of whole 32-byte blocks.
pub fn pbkdf2_sha256_1(password: &[u8], salt: &[u8], out: &mut [u8]) {
    debug_assert!(out.len().is_multiple_of(32));
    let keyed = Hmac::new(password);
    for (i, chunk) in out.chunks_exact_mut(32).enumerate() {
        let counter = (i as u32 + 1).to_be_bytes();
        chunk.copy_from_slice(&keyed.clone().finish(&[salt, &counter]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_rfc7914_style_vector() {
        // PBKDF2-HMAC-SHA256("passwd", "salt", c=1, 64) from RFC 7914 section 11.
        let mut out = [0u8; 64];
        pbkdf2_sha256_1(b"passwd", b"salt", &mut out);
        assert_eq!(
            hex(&out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
    }
}
