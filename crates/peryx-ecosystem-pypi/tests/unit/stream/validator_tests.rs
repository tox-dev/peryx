use super::*;

#[test]
fn test_hex_digit_reads_a_decimal_digit_by_its_offset_from_zero() {
    assert_eq!(hex_digit(b'5'), Some(5));
}

/// `feed_utf8` folds a continuation byte's bits with `<<`/`|`, which never overlap the accumulator's
/// low bits, into the running codepoint. A value that already exceeds the maximum codepoint before
/// the last continuation byte lands stays over the limit no matter how that byte's low bits are
/// folded in, so this drives the accumulator over `0x10FFFF` before the final byte to isolate the
/// upper-bound arm of the `||` chain on its own.
#[test]
fn test_feed_utf8_rejects_a_value_past_the_maximum_codepoint() {
    let mut validator = JsonValidator::new();

    validator.feed_utf8(0x80, 0x0011_0000, 1, 0x80, false);

    assert!(validator.failed);
}
