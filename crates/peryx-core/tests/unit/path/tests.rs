use super::{
    PathSafetyError, decode_path, decode_path_segment, is_local_file_url, local_file_url, validate_filename,
    validate_path_segment, validate_route,
};

#[test]
fn test_path_segments_encode_reserved_characters() {
    assert_eq!(
        local_file_url("root/alpha", "aa", "pkg 1.0#x?.bin"),
        "/root/alpha/files/aa/pkg%201.0%23x%3F.bin"
    );
}

#[test]
fn test_is_local_file_url_matches_only_the_route_files_prefix() {
    assert!(is_local_file_url("root/alpha", "/root/alpha/files/aa/pkg.bin"));
    assert!(!is_local_file_url("root/alpha", "/packages/pkg.bin"));
    assert!(!is_local_file_url("root/alpha", "/other/files/aa/pkg.bin"));
    assert!(!is_local_file_url("root/alpha", "https://files.example/pkg.bin"));
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
    assert_eq!(
        decode_path("peryxpkg-1.0.dist-info%2FMETADATA").unwrap(),
        "peryxpkg-1.0.dist-info/METADATA"
    );
}

#[test]
fn test_route_validation_accepts_nested_unreserved_routes() {
    assert_eq!(validate_route("root/alpha-1.0_~"), Ok(()));
}

#[test]
fn test_route_validation_rejects_unsafe_or_reserved_routes() {
    for route in [
        "",
        "/alpha",
        "alpha/",
        "root//alpha",
        ".",
        "root/..",
        "root/alpha mirror",
        "root/%70ypi",
    ] {
        assert_eq!(
            validate_route(route),
            Err(PathSafetyError::InvalidRoute(route.to_owned()))
        );
    }
    for route in [
        "browse/private",
        "admin/status",
        "search",
        "upload/mine",
        "favicon.svg",
        "_",
        "_/oidc",
    ] {
        assert_eq!(
            validate_route(route),
            Err(PathSafetyError::ReservedRoute(route.to_owned())),
            "{route:?}"
        );
    }
}

#[test]
fn test_filename_validation_rejects_path_inputs() {
    for filename in [
        "",
        ".",
        "..",
        "../pkg.bin",
        "pkg/name.bin",
        "pkg\\name.bin",
        "pkg\u{7}.bin",
    ] {
        assert!(validate_filename(filename).is_err(), "{filename:?}");
    }
    assert!(validate_filename("pkg 1.0#x?.bin").is_ok());
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
