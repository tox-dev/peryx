#[must_use]
pub(crate) fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[must_use]
#[cfg(any(feature = "hydrate", test))]
pub(crate) fn encode_path(text: &str) -> String {
    text.split('/').map(encode_component).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::{encode_component, encode_path};

    #[test]
    fn test_encode_component_escapes_url_delimiters() {
        assert_eq!(encode_component("pkg/data #1?.py"), "pkg%2Fdata%20%231%3F.py");
        assert_eq!(encode_path("root/pypi"), "root/pypi");
        assert_eq!(encode_path("root/pypi mirror"), "root/pypi%20mirror");
    }
}
