use super::{
    CORE_ROUTE_PREFIXES, PathSafetyError, canonicalize_path, decode_path, decode_path_segment, is_local_artifact_url,
    local_artifact_url, validate_artifact_name, validate_path_segment, validate_route,
};
use rstest::rstest;

#[test]
fn test_path_segments_encode_reserved_characters() {
    assert_eq!(
        local_artifact_url("root/alpha", "aa", "artifact 1.0#x?.bin"),
        "/root/alpha/files/aa/artifact%201.0%23x%3F.bin"
    );
}

#[rstest]
#[case::complete("/root/alpha/files/aa/artifact.bin", true)]
#[case::different_route("/other/files/aa/artifact.bin", false)]
#[case::digest_prefix("/root/alpha/files/aa0/artifact.bin", false)]
#[case::digest_suffix("/root/alpha/files/a/artifact.bin", false)]
#[case::different_filename("/root/alpha/files/aa/other.bin", false)]
#[case::extra_path_segment("/root/alpha/files/aa/extra/artifact.bin", false)]
#[case::absolute("https://files.example/artifact.bin", false)]
fn test_is_local_artifact_url_matches_the_complete_url(#[case] url: &str, #[case] expected: bool) {
    assert_eq!(is_local_artifact_url("root/alpha", "aa", "artifact.bin", url), expected);
}

#[test]
fn test_path_segments_decode_percent_encoding() {
    assert_eq!(decode_path_segment("pkg.bin").unwrap(), "pkg.bin");
    assert_eq!(decode_path_segment("pkg%201.0%23x%3F.bin").unwrap(), "pkg 1.0#x?.bin");
    assert_eq!(decode_path_segment("pkg%252Fname.bin").unwrap(), "pkg%2Fname.bin");
    assert_eq!(
        decode_path_segment("pkg%2"),
        Err(PathSafetyError::InvalidEncoding("pkg%2".to_owned()))
    );
    assert_eq!(
        decode_path_segment("pkg%xx"),
        Err(PathSafetyError::InvalidEncoding("pkg%xx".to_owned()))
    );
    assert_eq!(
        decode_path_segment("pkg%0x"),
        Err(PathSafetyError::InvalidEncoding("pkg%0x".to_owned()))
    );
    assert_eq!(
        decode_path_segment("pkg%ff"),
        Err(PathSafetyError::InvalidEncoding("pkg%ff".to_owned()))
    );
}

#[test]
fn test_paths_decode_member_separators() {
    assert_eq!(decode_path("artifact%2Fmetadata").unwrap(), "artifact/metadata");
}

#[rstest]
#[case::unescaped("/alpha/simple/", "/alpha/simple/")]
#[case::unreserved_letter("/%52PC2", "/RPC2")]
#[case::unreserved_run("/%61%6Cpha%2Done%2E%5F%7E", "/alpha-one._~")]
#[case::separator_kept("/alpha%2F", "/alpha%2F")]
#[case::excluded_kept("/alpha/pkg%201.0%23x%3F.bin", "/alpha/pkg%201.0%23x%3F.bin")]
#[case::double_escape_kept("/alpha/pkg%252Fname", "/alpha/pkg%252Fname")]
#[case::malformed_digits_kept("/alpha%xx/tail", "/alpha%xx/tail")]
#[case::truncated_escape_kept("/alpha%2", "/alpha%2")]
#[case::bare_percent_kept("/alpha%", "/alpha%")]
fn test_canonical_paths_unescape_only_unreserved_octets(#[case] path: &str, #[case] expected: &str) {
    assert_eq!(canonicalize_path(path), expected);
}

#[test]
fn test_route_validation_accepts_nested_unreserved_routes() {
    assert_eq!(validate_route("root/alpha-1.0_~", &[]), Ok(()));
}

#[rstest]
#[case::empty("")]
#[case::leading_slash("/alpha")]
#[case::trailing_slash("alpha/")]
#[case::empty_segment("root//alpha")]
#[case::current_directory(".")]
#[case::parent_directory("root/..")]
#[case::space("root/alpha mirror")]
#[case::encoded_segment("root/%61lpha")]
fn test_route_validation_rejects_unsafe_routes(#[case] route: &str) {
    assert_eq!(
        validate_route(route, &[]),
        Err(PathSafetyError::InvalidRoute(route.to_owned()))
    );
}

#[rstest]
#[case::reserved_favicon("favicon.svg")]
#[case::reserved_root("_")]
#[case::reserved_root_child("_/oidc")]
fn test_route_validation_rejects_reserved_routes(#[case] route: &str) {
    let prefix = route.split('/').next().unwrap();
    let reserved = CORE_ROUTE_PREFIXES
        .iter()
        .map(|prefix| (*prefix, "peryx core"))
        .collect::<Vec<_>>();
    assert_eq!(
        validate_route(route, &reserved),
        Err(PathSafetyError::ReservedRoute {
            route: route.to_owned(),
            prefix: prefix.to_owned(),
            owner: "peryx core".to_owned(),
        })
    );
}

#[test]
fn test_route_validation_reports_a_supplied_absolute_prefix_owner() {
    assert_eq!(
        validate_route("v2/simple", &[("/v2/", "oci")]),
        Err(PathSafetyError::ReservedRoute {
            route: "v2/simple".to_owned(),
            prefix: "/v2/".to_owned(),
            owner: "oci".to_owned(),
        })
    );
}

#[rstest]
#[case::empty("")]
#[case::current_directory(".")]
#[case::parent_directory("..")]
#[case::parent_path("../pkg.bin")]
#[case::decoded_separator("pkg/name.bin")]
#[case::windows_separator("pkg\\name.bin")]
#[case::control_character("pkg\u{7}.bin")]
fn test_artifact_name_validation_rejects_path_inputs(#[case] artifact: &str) {
    assert!(validate_artifact_name(artifact).is_err(), "{artifact:?}");
}

#[test]
fn test_artifact_name_validation_accepts_reserved_url_characters() {
    assert!(validate_artifact_name("artifact 1.0#x?.bin").is_ok());
}

#[test]
fn test_path_segment_validation_rejects_decoded_separators() {
    assert_eq!(validate_path_segment("version", "1.0+local"), Ok(()));
    assert_eq!(
        validate_path_segment("version", "1.0/local"),
        Err(PathSafetyError::InvalidPathSegment {
            kind: "version",
            value: "1.0/local".to_owned()
        })
    );
}
