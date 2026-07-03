pub fn push_component(out: &mut String, text: &str) {
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            other => push_percent(out, other),
        }
    }
}

pub fn push_path(out: &mut String, text: &str) {
    for byte in text.bytes() {
        match byte {
            b'/' => out.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            other => push_percent(out, other),
        }
    }
}

fn push_percent(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[usize::from(byte >> 4)] as char);
    out.push(HEX[usize::from(byte & 0x0f)] as char);
}

#[cfg(test)]
mod tests {
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
        push_path(&mut out, "root/pypi");
        assert_eq!(out, "root/pypi");

        let mut out = String::new();
        push_path(&mut out, "root/pypi mirror");
        assert_eq!(out, "root/pypi%20mirror");
    }
}
