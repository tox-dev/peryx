use crate::{ProjectDetail, parse_detail, render_legacy_json};

fn info_version(versions: &[&str], files: &[(&str, bool)]) -> String {
    let versions_json = versions
        .iter()
        .map(|version| format!("\"{version}\""))
        .collect::<Vec<_>>()
        .join(",");
    let files_json = files
        .iter()
        .enumerate()
        .map(|(index, (filename, yanked))| {
            format!(
                "{{\"filename\":\"{filename}\",\"url\":\"https://example.test/{filename}\",\
                 \"hashes\":{{\"sha256\":\"{index:064x}\"}},\"yanked\":{yanked}}}"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"pkg\",\"versions\":[{versions_json}],\"files\":[{files_json}]}}"
    );
    let parsed = parse_detail(body.as_bytes()).expect("valid detail");
    let detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    let rendered = render_legacy_json(&detail, None, None).expect("rendered");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("json");
    value["info"]["version"].as_str().expect("version string").to_owned()
}

#[rstest::rstest]
#[case::final_beats_prerelease(
    &["1.0", "2.0rc1"],
    &[("pkg-1.0.tar.gz", false), ("pkg-2.0rc1-py3-none-any.whl", false)],
    "1.0"
)]
#[case::active_beats_higher_yanked(
    &["1.0", "2.0"],
    &[("pkg-1.0.tar.gz", false), ("pkg-2.0.tar.gz", true)],
    "1.0"
)]
#[case::all_prerelease_falls_back_to_greatest(
    &["1.0rc1", "2.0rc1"],
    &[("pkg-1.0rc1-py3-none-any.whl", false), ("pkg-2.0rc1-py3-none-any.whl", false)],
    "2.0rc1"
)]
#[case::all_yanked_falls_back_to_greatest(
    &["1.0", "2.0"],
    &[("pkg-1.0.tar.gz", true), ("pkg-2.0.tar.gz", true)],
    "2.0"
)]
#[case::no_files_falls_back_to_greatest(&["1.0", "2.0"], &[], "2.0")]
#[case::unparseable_version_with_files_loses_to_stable(
    &["1.0", "not_a_version"],
    &[("pkg-1.0.tar.gz", false), ("pkg-not_a_version.tar.gz", false)],
    "1.0"
)]
#[case::all_unparseable_picks_greatest_string(
    &["alpha_build", "zeta_build"],
    &[("pkg-alpha_build.tar.gz", false), ("pkg-zeta_build.tar.gz", false)],
    "zeta_build"
)]
fn test_latest_release_version_matches_web_default(
    #[case] versions: &[&str],
    #[case] files: &[(&str, bool)],
    #[case] expected: &str,
) {
    assert_eq!(info_version(versions, files), expected);
}
