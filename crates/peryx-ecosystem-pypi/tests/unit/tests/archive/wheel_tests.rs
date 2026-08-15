use std::io::Write as _;

use rstest::rstest;

use super::{raw_zip, temp_archive};
use crate::archive::{ArchiveError, validate_wheel_path};

#[rstest]
#[case::duplicate_metadata(
    &[("pkg-1.0.dist-info/METADATA", b"a".as_slice()), ("pkg-1.0.dist-info/METADATA", b"b".as_slice())],
    "duplicate file member \"pkg-1.0.dist-info/METADATA\""
)]
#[case::duplicate_package_file(
    &[("pkg/module.py", b"a".as_slice()), ("pkg/module.py", b"b".as_slice())],
    "duplicate file member \"pkg/module.py\""
)]
#[case::repeated_directory(
    &[("pkg-1.0.dist-info/", b"".as_slice()), ("pkg-1.0.dist-info/", b"".as_slice())],
    "duplicate directory member \"pkg-1.0.dist-info\""
)]
#[case::file_and_directory(
    &[("pkg/data", b"x".as_slice()), ("pkg/data/", b"".as_slice())],
    "member \"pkg/data\" is both a file and a directory"
)]
fn test_validate_wheel_path_rejects_duplicate_members(#[case] entries: &[(&str, &[u8])], #[case] expected: &str) {
    let file = temp_archive(&raw_zip(entries));

    assert!(matches!(
        validate_wheel_path("pkg-1.0-py3-none-any.whl", file.path()),
        Err(ArchiveError::Invalid(message)) if message == format!("invalid wheel: {expected}")
    ));
}

#[test]
fn test_validate_wheel_path_rejects_non_wheel_filename_before_zip_read() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(b"not a zip").unwrap();
    file.flush().unwrap();

    assert!(matches!(
        validate_wheel_path("pkg.whl", file.path()),
        Err(ArchiveError::Invalid(message)) if message.contains("invalid wheel filename \"pkg.whl\"")
    ));
    assert!(matches!(
        validate_wheel_path("pkg-1.0.tar.gz", file.path()),
        Err(ArchiveError::Invalid(message)) if message == "invalid wheel: \"pkg-1.0.tar.gz\" is not a wheel filename"
    ));
}

#[test]
fn test_validate_wheel_path_rejects_too_many_entries() {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for index in 0..100_001 {
            zip.start_file(format!("flask-1.0.dist-info/empty-{index}.txt"), options)
                .unwrap();
        }
        zip.finish().unwrap();
    }
    let file = temp_archive(&buf);

    assert!(matches!(
        validate_wheel_path("Flask-1.0-py3-none-any.whl", file.path()),
        Err(ArchiveError::Invalid(message)) if message == "invalid wheel: archive has more than 100000 entries"
    ));
}

#[test]
fn test_validate_wheel_path_rejects_member_expanding_past_ratio() {
    let mut bytes = stored_zip(&[("Flask/data.bin", b"payload")]);
    set_declared_uncompressed_size(&mut bytes, "Flask/data.bin", 8_000_000);
    let file = temp_archive(&bytes);

    assert!(matches!(
        validate_wheel_path("Flask-1.0-py3-none-any.whl", file.path()),
        Err(ArchiveError::Invalid(message)) if message.contains("above the 1000:1 expansion limit")
    ));
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (path, data) in entries {
            zip.start_file(*path, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

fn set_declared_uncompressed_size(bytes: &mut [u8], target: &str, declared: u32) {
    const CENTRAL_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    let target = target.as_bytes();
    let index = (0..bytes.len().saturating_sub(46))
        .find(|&index| {
            if bytes[index..index + 4] != CENTRAL_SIGNATURE {
                return false;
            }
            let name_length = u16::from_le_bytes([bytes[index + 28], bytes[index + 29]]) as usize;
            let name_start = index + 46;
            name_start + name_length <= bytes.len() && bytes[name_start..name_start + name_length] == *target
        })
        .expect("the target central-directory record");
    bytes[index + 24..index + 28].copy_from_slice(&declared.to_le_bytes());
}
