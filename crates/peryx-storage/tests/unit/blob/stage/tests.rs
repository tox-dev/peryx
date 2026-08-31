use std::sync::Arc;

use super::PathOwners;

#[test]
fn test_a_path_stays_owned_until_the_last_guard_drops() {
    let owners = Arc::new(PathOwners::default());
    let path = std::path::PathBuf::from("stage");

    let first = owners.own(path.clone());
    let second = owners.own(path.clone());
    drop(first);
    assert!(owners.owns(&path));

    drop(second);
    assert!(!owners.owns(&path));
}
