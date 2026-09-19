use crate::contact::validate;

#[test]
fn test_validate_accepts_a_single_bare_address() {
    assert_eq!(validate("jane@example.com"), Ok(()));
}

#[test]
fn test_validate_accepts_a_display_name_with_angle_brackets() {
    assert_eq!(validate("Jane Doe <jane@example.com>"), Ok(()));
}

#[test]
fn test_validate_splits_unquoted_addresses_on_comma() {
    assert_eq!(validate("jane@example.com,john@example.com"), Ok(()));
}

/// A comma inside a quoted display name is part of that name, not a separator: splitting on it
/// anyway would leave `"Doe` and `John" <doe@example.com>` as two "addresses", neither of which is
/// a valid email address on its own.
#[test]
fn test_validate_does_not_split_on_a_comma_inside_quotes() {
    assert_eq!(validate(r#""Doe, John" <doe@example.com>"#), Ok(()));
}

#[test]
fn test_validate_rejects_an_invalid_address() {
    assert_eq!(validate("not an address"), Err("is not a valid email address"));
}
