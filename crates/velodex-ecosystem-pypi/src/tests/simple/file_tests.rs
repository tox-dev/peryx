use std::collections::BTreeMap;

use super::sha256;
use crate::{CoreMetadata, File, Provenance, Yanked};

#[test]
fn test_file_metadata_helpers_update_both_spellings() {
    let mut file = File {
        filename: "x-1.whl".to_owned(),
        url: "u".to_owned(),
        hashes: BTreeMap::new(),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Available,
        gpg_sig: None,
        provenance: Provenance::Absent,
    };
    assert_eq!(file.metadata(), &CoreMetadata::Available);
    file.set_metadata(CoreMetadata::Hashes(sha256("abc")));
    assert_eq!(
        (&file.core_metadata, &file.dist_info_metadata),
        (
            &CoreMetadata::Hashes(sha256("abc")),
            &CoreMetadata::Hashes(sha256("abc"))
        )
    );
    file.clear_metadata();
    assert_eq!(
        (&file.core_metadata, &file.dist_info_metadata),
        (&CoreMetadata::Absent, &CoreMetadata::Absent)
    );
}

#[test]
fn test_yanked_deserialize_variants() {
    assert_eq!(serde_json::from_str::<Yanked>("false").unwrap(), Yanked::No);
    assert_eq!(serde_json::from_str::<Yanked>("true").unwrap(), Yanked::Yes);
    assert_eq!(
        serde_json::from_str::<Yanked>("\"why\"").unwrap(),
        Yanked::Reason("why".to_owned())
    );
}

#[test]
fn test_yanked_deserialize_rejects_number() {
    assert!(serde_json::from_str::<Yanked>("123").is_err());
}

#[test]
fn test_core_metadata_deserialize_variants() {
    assert_eq!(
        serde_json::from_str::<CoreMetadata>("false").unwrap(),
        CoreMetadata::Absent
    );
    assert_eq!(
        serde_json::from_str::<CoreMetadata>("true").unwrap(),
        CoreMetadata::Available
    );
    let hashes = serde_json::from_str::<CoreMetadata>(r#"{"sha256":"abc"}"#).unwrap();
    assert_eq!(hashes, CoreMetadata::Hashes(sha256("abc")));
}

#[test]
fn test_core_metadata_deserialize_rejects_number() {
    assert!(serde_json::from_str::<CoreMetadata>("123").is_err());
}

#[test]
fn test_provenance_deserialize_variants() {
    assert_eq!(serde_json::from_str::<Provenance>("null").unwrap(), Provenance::None);
    assert_eq!(
        serde_json::from_str::<Provenance>(r#""https://example.test/provenance""#).unwrap(),
        Provenance::Url("https://example.test/provenance".to_owned())
    );
    assert!(serde_json::from_str::<Provenance>("123").is_err());
}
