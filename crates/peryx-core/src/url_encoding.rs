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
#[path = "../tests/unit/url_encoding/tests.rs"]
mod tests;
