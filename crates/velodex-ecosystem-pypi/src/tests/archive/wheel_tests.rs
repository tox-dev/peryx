use std::io::Write as _;

use crate::archive::{ArchiveError, validate_wheel_path};

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
