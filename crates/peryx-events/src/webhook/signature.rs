//! HMAC-SHA256 signing of webhook payloads.

use sha2::{Digest as _, Sha256};

#[must_use]
pub fn signature(secret: &str, timestamp: i64, delivery: &str, body: &[u8]) -> String {
    let mut message = timestamp.to_string();
    message.push('.');
    message.push_str(delivery);
    message.push('.');
    let mut mac = HmacSha256::new(secret.as_bytes());
    mac.update(message.as_bytes());
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize()))
}

struct HmacSha256 {
    inner: Sha256,
    outer_key: [u8; 64],
}

impl HmacSha256 {
    fn new(key: &[u8]) -> Self {
        let mut block = [0_u8; 64];
        if key.len() > block.len() {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner_key = [0x36_u8; 64];
        let mut outer_key = [0x5c_u8; 64];
        for (index, byte) in block.iter().enumerate() {
            inner_key[index] ^= byte;
            outer_key[index] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_key);
        Self { inner, outer_key }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finalize(self) -> [u8; 32] {
        let mut outer = Sha256::new();
        outer.update(self.outer_key);
        outer.update(self.inner.finalize());
        let digest = outer.finalize();
        let mut out = [0_u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_matches_hmac_sha256_vector() {
        assert_eq!(
            signature("key", 123, "wd_1", b"body"),
            "sha256=1c3e3ab3893bda6e5538c2f6f4dfaecb81b85dd27ea9243206d7237a65a33355"
        );
    }

    #[test]
    fn test_hmac_hashes_long_keys() {
        let mut mac = HmacSha256::new(&[0xaa; 131]);
        mac.update(b"Test Using Larger Than Block-Size Key - Hash Key First");

        assert_eq!(
            hex(&mac.finalize()),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
