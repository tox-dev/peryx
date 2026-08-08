use std::collections::BTreeMap;

use crate::{Provenance, Yanked};

use super::*;

#[test]
fn test_present_file_advertises_cached_generated_metadata() {
    let artifact = "a".repeat(64);
    let metadata = "b".repeat(64);
    let file = File {
        filename: "pkg-1.0-py3-none-any.whl".to_owned(),
        url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
        hashes: BTreeMap::from([("sha256".to_owned(), artifact.clone())]),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::default(),
    };

    let file = present_file(file, "pypi", &BTreeMap::from([(artifact.clone(), metadata.clone())]));

    assert_eq!(file.url, local_file_url("pypi", &artifact, "pkg-1.0-py3-none-any.whl"));
    assert!(matches!(file.metadata(), CoreMetadata::Hashes(hashes) if hashes["sha256"] == metadata));
}

#[test]
fn test_present_file_content_addresses_when_sha256_accompanies_other_hashes() {
    let sha256 = "a".repeat(64);
    let file = File {
        filename: "pkg-1.0-py3-none-any.whl".to_owned(),
        url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
        hashes: BTreeMap::from([
            ("md5".to_owned(), "deadbeef".to_owned()),
            ("sha256".to_owned(), sha256.clone()),
        ]),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::default(),
    };

    let file = present_file(file, "pypi", &BTreeMap::new());

    assert_eq!(file.url, local_file_url("pypi", &sha256, "pkg-1.0-py3-none-any.whl"));
    assert_eq!(file.hashes.get("md5").map(String::as_str), Some("deadbeef"));
}

#[test]
fn test_present_file_drops_gpg_sig_once_url_points_at_peryx() {
    let sha256 = "a".repeat(64);
    let file = File {
        filename: "pkg-1.0-py3-none-any.whl".to_owned(),
        url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
        hashes: BTreeMap::from([("sha256".to_owned(), sha256.clone())]),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: Some(true),
        provenance: Provenance::default(),
    };

    let file = present_file(file, "pypi", &BTreeMap::new());

    assert_eq!(file.url, local_file_url("pypi", &sha256, "pkg-1.0-py3-none-any.whl"));
    assert_eq!(file.gpg_sig, None);
}

#[test]
fn test_present_file_keeps_gpg_sig_when_url_stays_upstream() {
    let file = File {
        filename: "pkg-1.0.tar.gz".to_owned(),
        url: "https://files.example/pkg-1.0.tar.gz".to_owned(),
        hashes: BTreeMap::from([("md5".to_owned(), "deadbeef".to_owned())]),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: Some(true),
        provenance: Provenance::default(),
    };

    let file = present_file(file, "pypi", &BTreeMap::new());

    assert_eq!(file.url, "https://files.example/pkg-1.0.tar.gz");
    assert_eq!(file.gpg_sig, Some(true));
}
