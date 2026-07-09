use std::error::Error as _;

use crate::{ProjectStatus, SimpleError, parse_index};

#[test]
fn test_parse_meta_reads_project_status() {
    let meta =
        crate::parse_meta(br#"{"api-version":"1.4","project-status":"archived","project-status-reason":"read only"}"#)
            .unwrap();
    assert_eq!(meta.project_status.as_deref(), Some("archived"));
    assert_eq!(meta.project_status_reason.as_deref(), Some("read only"));
    assert_eq!(meta.status(), ProjectStatus::Archived);
    assert!(!meta.status().allows_uploads());
    assert!(meta.status().offers_downloads());
}

#[test]
fn test_parse_meta_rejects_invalid_project_status() {
    let err = crate::parse_meta(br#"{"api-version":"1.4","project-status":"frozen"}"#).unwrap_err();
    assert!(matches!(&err, SimpleError::InvalidProjectStatus(status) if status == "frozen"));
    assert_eq!(err.to_string(), "invalid upstream project status marker \"frozen\"");
    assert!(err.source().is_none());
}

#[test]
fn test_project_status_policy() {
    assert_eq!(ProjectStatus::Active.marker(), "active");
    assert_eq!(ProjectStatus::Archived.marker(), "archived");
    assert_eq!(ProjectStatus::Quarantined.marker(), "quarantined");
    assert_eq!(ProjectStatus::Deprecated.marker(), "deprecated");
    assert!(ProjectStatus::Active.allows_uploads());
    assert!(ProjectStatus::Deprecated.allows_uploads());
    assert!(!ProjectStatus::Archived.allows_uploads());
    assert!(!ProjectStatus::Quarantined.allows_uploads());
    assert!(!ProjectStatus::Quarantined.offers_downloads());
}

#[test]
fn test_parse_detail_rejects_unsupported_major_api_version() {
    let err = crate::parse_detail(br#"{"meta":{"api-version":"2.0"},"name":"x"}"#).unwrap_err();
    assert!(matches!(err, SimpleError::UnsupportedApiVersion(version) if version == "2.0"));
}

#[test]
fn test_parse_detail_rejects_invalid_api_version() {
    for version in ["1", "x.0", "1.x"] {
        let page = format!(r#"{{"meta":{{"api-version":"{version}"}},"name":"x"}}"#);
        let err = crate::parse_detail(page.as_bytes()).unwrap_err();
        assert!(matches!(&err, SimpleError::InvalidApiVersion(invalid) if invalid == version));
        assert_eq!(
            err.to_string(),
            format!("invalid upstream Simple API version {version:?}; expected Major.Minor")
        );
        assert!(err.source().is_none());
    }
}

#[test]
fn test_parse_index_rejects_unsupported_major_api_version() {
    let err = parse_index(br#"{"meta":{"api-version":"2.0"},"projects":[]}"#).unwrap_err();
    assert!(matches!(err, SimpleError::UnsupportedApiVersion(version) if version == "2.0"));
}
