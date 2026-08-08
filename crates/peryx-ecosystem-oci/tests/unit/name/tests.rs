use super::*;

#[test]
fn test_catalog_route_is_recognized() {
    assert_eq!(classify("/v2/_catalog"), Some(OciRoute::Catalog));
    assert_eq!(classify("/v2/_catalog/"), Some(OciRoute::Catalog));
}

#[test]
fn test_valid_name_component_enforces_the_oci_grammar() {
    for ok in ["foo", "foo-bar", "foo--bar", "foo__bar", "foo.bar", "a1b2"] {
        assert!(valid_name_component(ok), "{ok} should be valid");
    }
    for bad in ["", "-foo", "foo-", ".foo", "foo..bar", "foo._bar", "___", "Foo"] {
        assert!(!valid_name_component(bad), "{bad} should be rejected");
    }
}

#[test]
fn test_parse_digest_requires_a_lowercase_canonical_encoding() {
    assert!(parse_digest("sha256:abc123").is_some());
    assert!(parse_digest("sha256:ABC123").is_none());
    assert!(parse_digest("nocolon").is_none());
    assert!(parse_digest("sha256:").is_none());
}

#[test]
fn test_manifest_by_tag_splits_a_multi_segment_name() {
    assert_eq!(
        classify("/v2/dockerhub/library/nginx/manifests/latest"),
        Some(OciRoute::Manifest {
            name: "dockerhub/library/nginx".to_owned(),
            reference: Reference::Tag("latest".to_owned()),
        })
    );
}

#[test]
fn test_manifest_by_digest_is_a_digest_reference() {
    let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    assert_eq!(
        classify(&format!("/v2/alpine/manifests/{digest}")),
        Some(OciRoute::Manifest {
            name: "alpine".to_owned(),
            reference: Reference::Digest(digest.to_owned()),
        })
    );
}

#[test]
fn test_manifest_restore_keeps_the_reference_out_of_the_name() {
    assert_eq!(
        classify("/v2/store/team/app/manifests/latest/restore"),
        Some(OciRoute::ManifestRestore {
            name: "store/team/app".to_owned(),
            reference: Reference::Tag("latest".to_owned()),
        })
    );
}

#[test]
fn test_blob_route_carries_the_digest() {
    let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    assert_eq!(
        classify(&format!("/v2/alpine/blobs/{digest}")),
        Some(OciRoute::Blob {
            name: "alpine".to_owned(),
            digest: digest.to_owned(),
        })
    );
}

#[test]
fn test_blob_contents_route() {
    let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    assert_eq!(
        classify(&format!("/v2/team/app/blobs/{digest}/contents")),
        Some(OciRoute::BlobContents {
            name: "team/app".to_owned(),
            digest: digest.to_owned(),
        })
    );
    assert_eq!(classify("/v2/app/blobs/not-a-digest/contents"), None);
}

#[test]
fn test_referrers_route() {
    let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    assert_eq!(
        classify(&format!("/v2/team/app/referrers/{digest}")),
        Some(OciRoute::Referrers {
            name: "team/app".to_owned(),
            digest: digest.to_owned(),
        })
    );
    // A malformed digest still routes; the handler answers `400`, so classify keeps the raw tail.
    assert_eq!(
        classify("/v2/app/referrers/not-a-digest"),
        Some(OciRoute::Referrers {
            name: "app".to_owned(),
            digest: "not-a-digest".to_owned(),
        })
    );
}

#[test]
fn test_valid_content_digest_enforces_the_registered_length() {
    for ok in [
        format!("sha256:{}", "a".repeat(64)),
        format!("sha512:{}", "0".repeat(128)),
        // An unregistered algorithm keeps only the general grammar; its encoding may carry `=_-`.
        "multihash+base58:Qm-x_y=".to_owned(),
    ] {
        assert!(valid_content_digest(&ok), "{ok} should be valid");
    }
    for bad in [
        "sha256:bad",
        "sha256:",
        "not-a-digest",
        ":abc",
        "sha256:ABCDEF",
        "custom:ab$cd",
        &format!("sha256:{}", "a".repeat(63)),
        &format!("sha256:{}", "g".repeat(64)),
        &format!("sha512:{}", "f".repeat(64)),
    ] {
        assert!(!valid_content_digest(bad), "{bad} should be rejected");
    }
}

#[test]
fn test_upload_start_route() {
    assert_eq!(
        classify("/v2/team/app/blobs/uploads/"),
        Some(OciRoute::UploadStart {
            name: "team/app".to_owned(),
        })
    );
    assert_eq!(
        classify("/v2/app/blobs/uploads"),
        Some(OciRoute::UploadStart { name: "app".to_owned() })
    );
}

#[test]
fn test_upload_session_route() {
    assert_eq!(
        classify("/v2/team/app/blobs/uploads/abc123"),
        Some(OciRoute::UploadSession {
            name: "team/app".to_owned(),
            session: "abc123".to_owned(),
        })
    );
}

#[test]
fn test_upload_session_without_a_name_is_rejected() {
    assert_eq!(classify("/v2/blobs/uploads/abc"), None);
}

#[test]
fn test_tags_list_route() {
    assert_eq!(
        classify("/v2/team/app/tags/list"),
        Some(OciRoute::TagsList {
            name: "team/app".to_owned(),
        })
    );
}

#[test]
fn test_trailing_slash_is_tolerated() {
    assert_eq!(
        classify("/v2/app/tags/list/"),
        Some(OciRoute::TagsList { name: "app".to_owned() })
    );
}

#[test]
fn test_unknown_verb_is_rejected() {
    assert_eq!(classify("/v2/app/frobnicate/latest"), None);
}

#[test]
fn test_missing_v2_prefix_is_rejected() {
    assert_eq!(classify("/simple/app/manifests/latest"), None);
}

#[test]
fn test_empty_name_is_rejected() {
    assert_eq!(classify("/v2/manifests/latest"), None);
}

#[test]
fn test_double_slash_is_rejected() {
    assert_eq!(classify("/v2/app//manifests/latest"), None);
}

#[test]
fn test_dot_dot_name_component_is_rejected() {
    assert_eq!(classify("/v2/../secret/manifests/latest"), None);
}

#[test]
fn test_uppercase_name_is_rejected() {
    assert_eq!(classify("/v2/App/manifests/latest"), None);
}

#[test]
fn test_bad_tag_is_rejected() {
    assert_eq!(classify("/v2/app/manifests/-bad"), None);
}

#[test]
fn test_bad_digest_is_rejected() {
    assert_eq!(classify("/v2/app/blobs/sha256:"), None);
}

#[test]
fn test_too_short_path_is_rejected() {
    assert_eq!(classify("/v2/app"), None);
}

#[test]
fn test_valid_tag_edge_cases() {
    assert!(!valid_tag(""));
    assert!(valid_tag("_leading-underscore"));
    assert!(!valid_tag(&"x".repeat(129)));
    assert!(valid_tag(&"x".repeat(128)));
}

#[test]
fn test_referrers_tag_builds_the_fallback_tag_schema() {
    let cases = [
        (
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256-{}", "a".repeat(64)),
        ),
        (
            format!("sha512:{}", "b".repeat(128)),
            format!("sha512-{}", "b".repeat(64)),
        ),
        (format!("{}:eee", "a".repeat(40)), format!("{}-eee", "a".repeat(32))),
        ("sha256+test:ab+cd".to_owned(), "sha256-test-ab-cd".to_owned()),
        ("sha256".to_owned(), "sha256-".to_owned()),
    ];
    for (digest, expected) in cases {
        assert_eq!(referrers_tag(&digest), expected, "digest {digest}");
    }
}

#[test]
fn test_authority_key_prefixes_the_repository_and_keeps_paths_distinct() {
    assert_eq!(authority_key("library/nginx"), "oci:library/nginx");
    // The path is preserved verbatim, so two repositories keep two keys and never share a home.
    assert_ne!(authority_key("library/nginx"), authority_key("library/redis"));
}

#[test]
fn test_authority_key_cannot_collide_with_a_pypi_project_key() {
    // A single-segment repository would collide with a PyPI project of the same name in a shared
    // keyspace; the scheme prefix, which a PEP 503 normalized name can never contain, keeps them apart.
    let repo_key = authority_key("nginx");
    assert!(repo_key.starts_with("oci:"));
    assert_ne!(repo_key, "nginx");
}
