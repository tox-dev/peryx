use std::path::Path;

use super::{Access, copy_hashed, hashed_parent};

#[test]
fn test_hashed_parent_names_the_enclosing_directory() {
    assert_eq!(
        hashed_parent(Path::new("/backup/metadata/peryx.redb")).unwrap(),
        Path::new("/backup/metadata")
    );
}

#[test]
fn test_hashed_parent_reports_a_root_path_as_parentless() {
    let err = hashed_parent(Path::new("/")).unwrap_err();
    assert!(err.to_string().contains("no parent directory"), "{err}");
}

#[test]
fn test_copy_hashed_reports_a_parentless_destination() {
    let err = copy_hashed(Path::new("/does/not/matter"), Path::new("/"), "root", Access::Shared).unwrap_err();
    assert!(err.to_string().contains("no parent directory"), "{err}");
}
