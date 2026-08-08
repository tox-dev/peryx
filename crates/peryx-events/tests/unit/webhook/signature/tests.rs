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
