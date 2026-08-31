use peryx_driver::serving::{MirrorAction, MirrorDriver, MirrorRequest};
use peryx_index::IndexKind;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{app_with, oci_digest, oci_index, proxy};
use crate::registry::OciRegistry;

#[rstest::rstest]
#[case::configured_requirements("requirements", false)]
#[case::configured_mode("mode", false)]
#[case::configured_python_tags("python_tags", false)]
#[case::configured_metadata_only("metadata_only", false)]
#[case::override_requirements("requirements", true)]
#[case::override_mode("mode", true)]
#[case::override_python_tags("python_tags", true)]
#[case::override_metadata_only("metadata_only", true)]
#[case::override_packages("packages", true)]
fn prefetch_options_reject_unsupported_keys(#[case] key: &str, #[case] override_option: bool) {
    let mut configured = toml::Table::new();
    let mut overrides = toml::Table::new();
    if override_option {
        overrides.insert(key.to_owned(), toml::Value::Boolean(true));
    } else {
        configured.insert(key.to_owned(), toml::Value::Boolean(true));
    }

    assert_eq!(
        OciRegistry::default()
            .validate_options(&configured, &overrides)
            .unwrap_err(),
        format!("prefetch option {key:?} is not supported by oci")
    );
}

#[rstest::rstest]
#[case::images(images(&[]), images(&[]))]
#[case::packages(
    toml::Table::from_iter([("packages".to_owned(), toml::Value::Array(Vec::new()))]),
    toml::Table::new()
)]
fn prefetch_options_accept_consumed_keys(#[case] configured: toml::Table, #[case] overrides: toml::Table) {
    assert_eq!(OciRegistry::default().validate_options(&configured, &overrides), Ok(()));
}

fn report_rows(output: &str) -> Vec<[&str; 9]> {
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some(crate::MIRROR_REPORT_HEADER.trim_end()));
    lines
        .map(|line| {
            line.split('\t')
                .collect::<Vec<_>>()
                .try_into()
                .expect("mirror report rows have nine columns")
        })
        .collect()
}

fn images(values: &[&str]) -> toml::Table {
    toml::Table::from_iter([(
        "images".to_owned(),
        toml::Value::Array(
            values
                .iter()
                .map(|value| toml::Value::String((*value).to_owned()))
                .collect(),
        ),
    )])
}

#[rstest::rstest]
#[case::array_required(toml::Value::Boolean(true), "images must be an array")]
#[case::string_entries(toml::Value::Array(vec![toml::Value::Integer(1)]), "images entries must be strings")]
#[tokio::test]
async fn mirror_rejects_invalid_image_options(#[case] value: toml::Value, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
    let configured = toml::Table::from_iter([("images".to_owned(), value)]);
    let empty = toml::Table::new();
    let mut output = Vec::new();

    let error = OciRegistry::default()
        .mirror(
            state,
            MirrorRequest {
                action: MirrorAction::Plan,
                index: "oci",
                settings: &empty,
                configured: &configured,
                overrides: &empty,
            },
            &mut output,
        )
        .await
        .unwrap_err();

    assert_eq!(error, expected);
}

#[tokio::test]
async fn mirror_plan_reports_selected_images() {
    for (image, expected_project, expected_filename) in [
        ("library/example:latest", "library/example", "latest"),
        (
            "library/example@sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "library/example",
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        ),
        ("library/example", "library/example", "latest"),
        ("library/example:bad+tag", "library/example:bad+tag", ""),
        (
            "registry.example/library/example:latest",
            "registry.example/library/example:latest",
            "",
        ),
        ("@", "@", ""),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
        let configured = images(&[image]);
        let empty = toml::Table::new();
        let mut output = Vec::new();

        OciRegistry::default()
            .mirror(
                state,
                MirrorRequest {
                    action: MirrorAction::Plan,
                    index: "oci",
                    settings: &empty,
                    configured: &configured,
                    overrides: &empty,
                },
                &mut output,
            )
            .await
            .unwrap();

        assert_eq!(
            report_rows(std::str::from_utf8(&output).unwrap()),
            [
                [
                    "manifest",
                    "store",
                    expected_project,
                    expected_filename,
                    "",
                    "",
                    "0",
                    "selected",
                    "",
                ],
                ["summary", "store", "", "images", "", "", "1", "images", ""],
            ]
        );
    }
}

#[tokio::test]
async fn mirror_reports_stable_columns_across_actions() {
    let server = MockServer::start().await;
    let index_type = "application/vnd.oci.image.index.v1+json";
    let manifest = format!(r#"{{"schemaVersion":2,"mediaType":"{index_type}","manifests":[]}}"#).into_bytes();
    Mock::given(method("GET"))
        .and(path("/v2/library/example/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.clone(), index_type))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = proxy(&dir, &format!("{}/", server.uri()), false);
    let configured = images(&["library/example:latest"]);
    let empty = toml::Table::new();
    let digest = oci_digest(&manifest);
    let bytes = manifest.len().to_string();

    for (action, expected_detail, expected_summary) in [
        (
            MirrorAction::Plan,
            [
                "manifest",
                "hub",
                "library/example",
                "latest",
                "",
                "",
                "0",
                "selected",
                "",
            ],
            ["summary", "hub", "", "images", "", "", "1", "images", ""],
        ),
        (
            MirrorAction::Sync,
            [
                "manifest",
                "hub",
                "library/example",
                "latest",
                &digest,
                "",
                &bytes,
                "synced",
                "",
            ],
            [
                "summary",
                "hub",
                "",
                "",
                "",
                "",
                &bytes,
                "synced",
                "1 synced, 0 cached, 0 errors",
            ],
        ),
        (
            MirrorAction::Verify,
            [
                "manifest",
                "hub",
                "library/example",
                "latest",
                &digest,
                "",
                "0",
                "cached",
                "",
            ],
            [
                "summary",
                "hub",
                "",
                "",
                "",
                "",
                "0",
                "synced",
                "0 synced, 1 cached, 0 errors",
            ],
        ),
    ] {
        let mut output = Vec::new();
        OciRegistry::default()
            .mirror(
                state.clone(),
                MirrorRequest {
                    action,
                    index: "hub",
                    settings: &empty,
                    configured: &configured,
                    overrides: &empty,
                },
                &mut output,
            )
            .await
            .unwrap();

        let rows = report_rows(std::str::from_utf8(&output).unwrap());
        assert_eq!(rows, [expected_detail, expected_summary]);
    }
}

#[tokio::test]
async fn mirror_reports_failed_summary() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = proxy(&dir, "http://127.0.0.1:1/", false);
    let configured = images(&["library/example:latest"]);
    let empty = toml::Table::new();
    let mut output = Vec::new();

    OciRegistry::default()
        .mirror(
            state,
            MirrorRequest {
                action: MirrorAction::Verify,
                index: "hub",
                settings: &empty,
                configured: &configured,
                overrides: &empty,
            },
            &mut output,
        )
        .await
        .expect_err("missing content fails verification");

    let rows = report_rows(std::str::from_utf8(&output).unwrap());
    assert_eq!(
        rows.last().unwrap()[6..],
        ["0", "error", "0 synced, 0 cached, 1 errors"]
    );
}

#[tokio::test]
async fn mirror_rejects_unknown_indexes_and_empty_selections() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
    let empty = toml::Table::new();
    let mut output = Vec::new();
    let request = |index| MirrorRequest {
        action: MirrorAction::Plan,
        index,
        settings: &empty,
        configured: &empty,
        overrides: &empty,
    };

    assert!(
        OciRegistry::default()
            .mirror(state.clone(), request("missing"), &mut output)
            .await
            .is_err()
    );
    assert!(
        OciRegistry::default()
            .mirror(state, request("oci"), &mut output)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn mirror_surfaces_output_failures() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
    let configured = images(&["library/example:latest"]);
    let empty = toml::Table::new();

    let mut output = std::io::Cursor::new(&mut [] as &mut [u8]);
    let error = OciRegistry::default()
        .mirror(
            state,
            MirrorRequest {
                action: MirrorAction::Plan,
                index: "oci",
                settings: &empty,
                configured: &configured,
                overrides: &empty,
            },
            &mut output,
        )
        .await
        .unwrap_err();

    assert_eq!(error, "failed to write whole buffer");
}
