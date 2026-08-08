use rstest::rstest;

use super::{
    admin_project_url, admin_version_url, browse_archive_listing_url, browse_archive_member_url, browse_archive_url,
    browse_index_url, browse_project_file_search_url, browse_project_release_url, browse_project_url,
    browser_http_origin, inspect_url, search_api_url, search_page_url, stats_api_url, stats_index_url,
    stats_project_url, ui_manifest_url, ui_member_url, ui_members_url, ui_project_url, ui_projects_url,
};

#[rstest]
#[case("http:", "localhost", "", "http://localhost")]
#[case("http:", "localhost", "80", "http://localhost")]
#[case("https:", "packages.example", "443", "https://packages.example")]
#[case("https:", "packages.example", "8443", "https://packages.example:8443")]
fn test_browser_http_origin_formats_ports(
    #[case] protocol: &str,
    #[case] hostname: &str,
    #[case] port: &str,
    #[case] expected: &str,
) {
    assert_eq!(browser_http_origin(protocol, hostname, port).as_deref(), Some(expected));
}

#[rstest]
#[case("ftp:", "packages.example", "21")]
#[case("https:", "", "")]
fn test_browser_http_origin_rejects_unsupported_locations(
    #[case] protocol: &str,
    #[case] hostname: &str,
    #[case] port: &str,
) {
    assert_eq!(browser_http_origin(protocol, hostname, port), None);
}

#[test]
fn test_package_urls_encode_paths_and_queries() {
    assert_eq!(browse_index_url("root/alpha"), "/browse?index=root%2Falpha");
    assert_eq!(
        browse_project_url("root/alpha", "pkg name"),
        "/browse?index=root%2Falpha&project=pkg%20name"
    );
    assert_eq!(
        browse_archive_url("root/alpha", "pkg name", "aa", "pkg 1.0#x?.bin"),
        "/browse?index=root%2Falpha&project=pkg%20name&sha256=aa&file=pkg%201.0%23x%3F.bin"
    );
    assert_eq!(
        browse_project_file_search_url("root/alpha", "pkg name", None, "cp313.*\\.bin", true),
        "/browse?index=root%2Falpha&project=pkg%20name&filename=cp313.%2A%5C.bin&filename_match=regex"
    );
    assert_eq!(
        browse_project_release_url("root/alpha", "pkg name", "1!2+local.1", "cp313", false),
        "/browse?index=root%2Falpha&project=pkg%20name&version=1%212%2Blocal.1&filename=cp313"
    );
}

#[test]
fn test_ui_endpoint_urls_encode_arguments() {
    assert_eq!(ui_projects_url("root/beta"), "/+ui/projects?index=root%2Fbeta");
    assert_eq!(
        ui_project_url("root/beta", "team/app"),
        "/+ui/project?index=root%2Fbeta&project=team%2Fapp"
    );
    assert_eq!(
        ui_manifest_url("root/beta", "team/app", "1.0"),
        "/+ui/manifest?index=root%2Fbeta&project=team%2Fapp&ref=1.0"
    );
    assert_eq!(
        ui_members_url("root/beta", "team/app", "sha256:aa"),
        "/+ui/members?index=root%2Fbeta&project=team%2Fapp&digest=sha256%3Aaa"
    );
    assert_eq!(
        ui_member_url("root/beta", "team/app", "sha256:aa", "etc/os #1", 1024),
        "/+ui/member?index=root%2Fbeta&project=team%2Fapp&digest=sha256%3Aaa&member=etc%2Fos%20%231&offset=1024"
    );
}

#[test]
fn test_archive_urls_encode_nested_members() {
    let containers = vec!["vendor/inner #1.zip".to_owned()];
    assert_eq!(
        browse_archive_listing_url("root/alpha", "pkg", "aa", "pkg.bin", &containers),
        "/browse?index=root%2Falpha&project=pkg&sha256=aa&file=pkg.bin&container=vendor%2Finner%20%231.zip"
    );
    assert_eq!(
        browse_archive_member_url("root/alpha", "pkg", "aa", "pkg.bin", &containers, "pkg/mod #1.py", 1024),
        "/browse?index=root%2Falpha&project=pkg&sha256=aa&file=pkg.bin&container=vendor%2Finner%20%231.zip&member=pkg%2Fmod%20%231.py&offset=1024"
    );
    assert_eq!(
        inspect_url(
            "root/alpha",
            "aa",
            "pkg 1.0.bin",
            &containers,
            Some("pkg/mod #1.py"),
            1024
        ),
        "/root/alpha/inspect/aa/pkg%201.0.bin?container=vendor%2Finner%20%231.zip&member=pkg%2Fmod%20%231.py&offset=1024"
    );
}

#[test]
fn test_stats_and_admin_urls_encode_arguments() {
    assert_eq!(
        search_page_url("flask cache", "override", "local", 2, 50),
        "/search?q=flask%20cache&type=override&availability=local&page=2&page_size=50"
    );
    assert_eq!(
        search_api_url(Some("root/alpha"), "flask", "all", "all", 1, 25),
        "/+search?route=root%2Falpha&q=flask&page_size=25"
    );
    assert_eq!(stats_index_url("root/alpha"), "/stats?index=root%2Falpha");
    assert_eq!(
        stats_project_url("root/alpha", "pkg name"),
        "/stats?index=root%2Falpha&project=pkg%20name"
    );
    assert_eq!(
        stats_api_url(Some("root/alpha"), Some("pkg name")),
        "/+stats?index=root%2Falpha&project=pkg%20name"
    );
    assert_eq!(admin_project_url("root/alpha", "pkg name"), "/root/alpha/pkg%20name/");
    assert_eq!(
        admin_version_url("root/alpha", "pkg name", "1.0+local", Some("yank")),
        "/root/alpha/pkg%20name/1.0%2Blocal/yank"
    );
}
