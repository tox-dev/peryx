use super::{
    PathSafetyError, decode_path, decode_path_segment, is_local_artifact_url, local_artifact_url,
    validate_artifact_name, validate_path_segment, validate_route,
};
use rstest::rstest;

#[test]
fn test_path_segments_encode_reserved_characters() {
    assert_eq!(
        local_artifact_url("root/alpha", "aa", "artifact 1.0#x?.bin"),
        "/root/alpha/files/aa/artifact%201.0%23x%3F.bin"
    );
}

#[test]
fn test_is_local_artifact_url_matches_only_the_route_files_prefix() {
    assert!(is_local_artifact_url("root/alpha", "/root/alpha/files/aa/artifact.bin"));
    assert!(!is_local_artifact_url("root/alpha", "/artifacts/artifact.bin"));
    assert!(!is_local_artifact_url("root/alpha", "/other/files/aa/artifact.bin"));
    assert!(!is_local_artifact_url(
        "root/alpha",
        "https://files.example/artifact.bin"
    ));
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

#[test]
fn test_route_validation_accepts_nested_unreserved_routes() {
    assert_eq!(validate_route("root/alpha-1.0_~"), Ok(()));
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
        validate_route(route),
        Err(PathSafetyError::InvalidRoute(route.to_owned()))
    );
}

#[rstest]
#[case::reserved_browse("browse/private")]
#[case::reserved_admin("admin/status")]
#[case::reserved_search("search")]
#[case::reserved_upload("upload/mine")]
#[case::reserved_favicon("favicon.svg")]
#[case::reserved_root("_")]
#[case::reserved_root_child("_/oidc")]
fn test_route_validation_rejects_reserved_routes(#[case] route: &str) {
    assert_eq!(
        validate_route(route),
        Err(PathSafetyError::ReservedRoute(route.to_owned()))
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
