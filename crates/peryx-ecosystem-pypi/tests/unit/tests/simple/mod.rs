use std::collections::BTreeMap;

use crate::{CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked};

mod error_tests;
mod file_tests;
mod meta_tests;
mod parse_tests;
mod render_tests;

pub(super) fn sha256(value: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("sha256".to_owned(), value.to_owned())])
}

pub(super) fn sample_detail() -> ProjectDetail {
    ProjectDetail {
        meta: Meta {
            api_version: crate::API_VERSION,
            project_status: Some("active".to_owned()),
            project_status_reason: Some("available".to_owned()),
        },
        name: "proj&<>".to_owned(),
        versions: vec!["1.0".to_owned(), "2.0".to_owned()],
        files: vec![
            File {
                filename: "proj&<>-2.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/a?b=1&c=2".to_owned(),
                hashes: sha256("6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"),
                requires_python: Some(">=3.8,<4".to_owned()),
                size: Some(1234),
                upload_time: Some("2024-03-24T00:00:00.000000Z".to_owned()),
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Hashes(sha256(
                    "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35",
                )),
                dist_info_metadata: CoreMetadata::Hashes(sha256(
                    "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35",
                )),
                gpg_sig: Some(true),
                provenance: Provenance::Url("https://files.example/a.provenance".to_owned()),
            },
            File {
                filename: "proj-1.5.tar.gz".to_owned(),
                url: "https://files.example/q\"uote".to_owned(),
                hashes: BTreeMap::new(),
                requires_python: None,
                size: None,
                upload_time: None,
                yanked: Yanked::Reason("broken build".to_owned()),
                core_metadata: CoreMetadata::Available,
                dist_info_metadata: CoreMetadata::Available,
                gpg_sig: Some(false),
                provenance: Provenance::Absent,
            },
            File {
                filename: "proj-1.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/c.whl".to_owned(),
                hashes: sha256("4e07408562bedb8b60ce05c1decfe3ad16b72230967de01f640b7e4729b49fce"),
                requires_python: None,
                size: Some(9),
                upload_time: None,
                yanked: Yanked::Yes,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::None,
            },
        ],
    }
}

pub(super) fn sample_list() -> ProjectList {
    ProjectList {
        meta: Meta::default(),
        projects: vec![
            ProjectListEntry {
                name: "Flask".to_owned(),
            },
            ProjectListEntry {
                name: "zope.interface".to_owned(),
            },
            ProjectListEntry {
                name: "a&<>".to_owned(),
            },
        ],
    }
}
