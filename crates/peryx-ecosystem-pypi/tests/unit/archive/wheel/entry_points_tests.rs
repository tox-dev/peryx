use super::*;

#[test]
fn test_validate_entry_points_requires_a_closing_bracket_on_a_section_header() {
    let error = validate_entry_points(b"[console_scripts\n").unwrap_err();

    assert!(
        error.to_string().contains("is not a key=value entry"),
        "a line opening with `[` but never closing it is not a section header: {error}"
    );
}
