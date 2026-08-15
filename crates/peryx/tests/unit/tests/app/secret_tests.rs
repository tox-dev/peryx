use std::io::Cursor;

use rstest::rstest;

use super::*;

#[rstest]
#[case::crlf(b"secret\r\n", "secret")]
#[case::lf(b"secret\n", "secret")]
#[case::none(b"secret", "secret")]
fn test_read_secret_removes_one_line_ending(#[case] input: &[u8], #[case] expected: &str) {
    assert_eq!(read_secret(None, &mut Cursor::new(input), "token").unwrap(), expected);
}

#[test]
fn test_read_secret_rejects_oversized_input() {
    let error = read_secret(None, &mut Cursor::new(vec![b'a'; MAX_SECRET_BYTES + 1]), "token").unwrap_err();

    assert_eq!(error.to_string(), "read token from standard input");
    assert!(format!("{error:#}").contains("token input exceeds the 1048576-byte limit"));
}

#[test]
fn test_read_secret_rejects_invalid_utf8() {
    let error = read_secret(None, &mut Cursor::new([0xff]), "token").unwrap_err();

    assert!(format!("{error:#}").contains("token input must be UTF-8"));
}

#[test]
fn test_read_secret_reports_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");

    let error = read_secret(Some(&path), &mut Cursor::new([]), "token").unwrap_err();

    assert!(
        error
            .to_string()
            .contains(&format!("open token file {}", path.display()))
    );
}
