use super::{push_component, push_path};

#[test]
fn test_push_component_escapes_url_delimiters() {
    let mut out = String::new();
    push_component(&mut out, "pkg/data #1?.py");
    assert_eq!(out, "pkg%2Fdata%20%231%3F.py");
}

#[test]
fn test_push_path_keeps_segment_separators() {
    let mut out = String::new();
    push_path(&mut out, "root/alpha");
    assert_eq!(out, "root/alpha");

    let mut out = String::new();
    push_path(&mut out, "root/alpha mirror");
    assert_eq!(out, "root/alpha%20mirror");
}
