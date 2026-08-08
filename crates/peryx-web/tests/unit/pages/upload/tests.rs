use leptos::prelude::*;
use rstest::rstest;

use peryx_core::UiUploadSpec;

use crate::model::{UiIndex, UiSnapshot};

use super::{
    UploadUi, accepted_filename, begin_upload, cancel_active_upload, selected_filename, upload_form, upload_outcome,
};

#[rstest]
#[case::artifact("pkg-1.0-py3-none-any.bin")]
#[case::artifact_uppercase("pkg-1.0-py3-none-any.BIN")]
#[case::source("pkg-1.0.tar.gz")]
fn test_accepted_filename_allows_browser_formats(#[case] filename: &str) {
    assert!(accepted_filename(filename, &[".bin".to_owned(), ".tar.gz".to_owned()]));
}

#[rstest]
#[case::zip("pkg-1.0.zip")]
#[case::egg("pkg-1.0.egg")]
#[case::bare("pkg")]
fn test_accepted_filename_rejects_other_formats(#[case] filename: &str) {
    assert!(!accepted_filename(filename, &[".bin".to_owned(), ".tar.gz".to_owned()]));
}

#[rstest]
#[case::success(200, "upload accepted", false, "pkg.bin: uploaded")]
#[case::denial(
    403,
    "token does not grant this action",
    false,
    "pkg.bin: token does not grant this action"
)]
#[case::empty_denial(403, "", false, "pkg.bin: request rejected (403)")]
#[case::store_failure(500, "temporary path /secret", false, "pkg.bin: server could not store the upload")]
#[case::network(0, "", false, "pkg.bin: connection ended before the upload completed")]
#[case::cancelled(0, "", true, "pkg.bin: upload cancelled")]
fn test_upload_outcome_bounds_browser_messages(
    #[case] status: u16,
    #[case] body: &str,
    #[case] cancelled: bool,
    #[case] expected: &str,
) {
    assert_eq!(upload_outcome("pkg.bin", status, body, cancelled), expected);
}

#[rstest]
#[case::missing_token("", "pkg-1.0-py3-none-any.bin", "Enter an upload token.")]
#[case::missing_file("secret", "", "Choose an artifact.")]
#[case::unsupported("secret", "pkg-1.0.zip", "pkg-1.0.zip: unsupported artifact type")]
#[case::valid("secret", "pkg-1.0-py3-none-any.bin", "")]
fn test_begin_upload_validates_browser_input(#[case] token: &str, #[case] filename: &str, #[case] expected: &str) {
    Owner::new().with(|| {
        let (outcome, set_outcome) = signal(String::new());
        let (_, set_progress) = signal(0.0_f64);
        let (_, set_uploading) = signal(false);
        begin_upload(
            NodeRef::new(),
            token,
            filename,
            upload_spec("root/packages"),
            UploadUi {
                outcome: set_outcome,
                progress: set_progress,
                uploading: set_uploading,
            },
        );
        assert_eq!(outcome.get_untracked(), expected);
    });
}

#[test]
fn test_server_side_file_selection_and_cancel_are_inert() {
    Owner::new().with(|| assert_eq!(selected_filename(NodeRef::new()), ""));
    cancel_active_upload();
}

#[test]
fn test_upload_form_lists_only_indexes_with_browser_uploads() {
    Owner::new().with(|| {
        let mut virtual_index = index("root/packages", true);
        virtual_index.upload_to = Some("hosted".to_owned());
        let html = upload_form(UiSnapshot {
            indexes: vec![virtual_index, index("internal", true), index("cache", false)],
            ..UiSnapshot::default()
        })
        .to_html();

        assert!(
            html.contains(r#"<option value="root/packages">root/packages (stores in hosted)</option>"#),
            "{html}"
        );
        assert!(html.contains(r#"<option value="internal">internal</option>"#), "{html}");
        assert!(!html.contains(r#"value="cache""#), "{html}");
    });
}

#[test]
fn test_upload_form_reports_no_browser_upload() {
    let html = upload_form(UiSnapshot::default()).to_html();
    assert!(html.contains("No index exposes a browser upload."), "{html}");
}

fn index(route: &str, uploads: bool) -> UiIndex {
    UiIndex {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: "example".to_owned(),
        endpoint: format!("/{route}/"),
        kind: "hosted".to_owned(),
        layers: Vec::new(),
        uploads,
        upload: uploads.then(|| upload_spec(route)),
        upload_to: None,
        upstream: None,
        hosted: None,
        project_count: 0,
        upload_count: 0,
        recent_uploads: Vec::new(),
    }
}

fn upload_spec(route: &str) -> UiUploadSpec {
    UiUploadSpec {
        endpoint: format!("/{route}/"),
        form_field: "content".to_owned(),
        authorization_username: Some("token".to_owned()),
        token_label: "Token".to_owned(),
        file_label: "Artifact".to_owned(),
        accept: ".bin,.tar.gz".to_owned(),
        help: "Choose an artifact.".to_owned(),
        allowed_suffixes: vec![".bin".to_owned(), ".tar.gz".to_owned()],
    }
}
