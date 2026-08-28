use rstest::rstest;

use peryx_core::BrowseSection;

use super::{MemberChunk, manifest_content_from_bytes, manifest_page, member_page, members_from_bytes, pull_command};
use crate::name::Reference;

#[test]
fn test_members_from_bytes_parses_a_listing() {
    let members =
        members_from_bytes(br#"{"members":[{"path":"a.txt","size":3,"kind":"text","previewable":true}]}"#).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].path, "a.txt");
}

#[rstest]
#[case::tag(Reference::Tag("latest".to_owned()), "docker pull <host>/team/app:latest")]
#[case::digest(
    Reference::Digest("sha256:abc".to_owned()),
    "docker pull <host>/team/app@sha256:abc"
)]
fn test_pull_command_uses_the_reference_separator(#[case] reference: Reference, #[case] expected: &str) {
    assert_eq!(pull_command("team/app", &reference), expected);
}

#[test]
fn test_members_from_bytes_rejects_invalid_json() {
    assert!(members_from_bytes(b"not json").is_err());
}

#[test]
fn test_manifest_content_rejects_invalid_json() {
    assert!(manifest_content_from_bytes(b"not json").is_err());
}

#[rstest]
#[case::image(br#"{"config":{"size":10},"layers":[{"size":3},{"size":4}]}"#, false, 17)]
#[case::index(br#"{"manifests":[{"size":5},{"size":6}]}"#, true, 11)]
#[case::image_saturates(
    br#"{"config":{"size":18446744073709551615},"layers":[{"size":1}]}"#,
    false,
    u64::MAX
)]
#[case::index_saturates(
    br#"{"manifests":[{"size":18446744073709551615},{"size":18446744073709551615}]}"#,
    true,
    u64::MAX
)]
fn test_manifest_page_totals_sizes_and_saturates_overflow(
    #[case] bytes: &[u8],
    #[case] is_index: bool,
    #[case] total_size: u64,
) {
    let page = manifest_page("oci", "team/app", "latest", manifest_content_from_bytes(bytes).unwrap());
    let BrowseSection::Properties { entries, .. } = &page.sections[0] else {
        panic!("manifest properties missing");
    };
    let BrowseSection::Table { heading, .. } = &page.sections[1] else {
        panic!("manifest table missing");
    };
    assert_eq!(
        (entries[1].value.clone(), heading.as_str()),
        (
            total_size.to_string(),
            if is_index { "Platform manifests" } else { "Layers" },
        ),
    );
}

#[rstest]
#[case::oci_tar("application/vnd.oci.image.layer.v1.tar", true)]
#[case::oci_gzip("APPLICATION/VND.OCI.IMAGE.LAYER.V1.TAR+GZIP", true)]
#[case::oci_zstd("application/vnd.oci.image.layer.v1.tar+zstd", false)]
#[case::oci_nondistributable_tar("application/vnd.oci.image.layer.nondistributable.v1.tar", true)]
#[case::oci_nondistributable_gzip("application/vnd.oci.image.layer.nondistributable.v1.tar+gzip", true)]
#[case::oci_nondistributable_zstd("application/vnd.oci.image.layer.nondistributable.v1.tar+zstd", false)]
#[case::docker_gzip("application/vnd.docker.image.rootfs.diff.tar.gzip", true)]
#[case::docker_foreign_gzip("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip", true)]
#[case::artifact("application/vnd.example.layer.tar+gzip", false)]
#[case::parameter("application/vnd.oci.image.layer.v1.tar+gzip; level=9", false)]
#[case::invalid("vnd.oci.image.layer.v1.tar+gzip", false)]
fn test_manifest_page_only_links_supported_layer_media_types(#[case] media_type: &str, #[case] browsable: bool) {
    let manifest = format!(r#"{{"layers":[{{"digest":"sha256:abc","size":1,"mediaType":"{media_type}"}}]}}"#);
    let page = manifest_page(
        "oci",
        "team/app",
        "latest",
        manifest_content_from_bytes(manifest.as_bytes()).unwrap(),
    );
    let BrowseSection::Table { rows, .. } = &page.sections[1] else {
        panic!("manifest table missing");
    };

    assert_eq!(
        (
            rows[0].cells[0].href.is_some(),
            rows[0].cells[3].href.is_some(),
            rows[0].cells[3].text.as_str(),
        ),
        (browsable, browsable, if browsable { "contents" } else { "" }),
    );
}

#[test]
fn test_member_page_links_the_next_chunk() {
    let page = member_page(
        "oci",
        "team/app",
        "latest",
        "sha256:abc",
        "large.txt",
        MemberChunk {
            text: "part".to_owned(),
            size: Some(10),
            offset: 4,
            next_offset: Some(8),
        },
    );

    assert_eq!(
        page.sections,
        vec![BrowseSection::Content {
            heading: "Preview".to_owned(),
            text: "part".to_owned(),
            size: Some(10),
            offset: 4,
            next: Some(peryx_core::BrowseLink {
                label: "Next chunk".to_owned(),
                href: "/browse?index=oci&project=team%2Fapp&ref=latest&layer=sha256%3Aabc&member=large.txt&offset=8"
                    .to_owned(),
            }),
        }]
    );
}
