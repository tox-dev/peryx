use std::io::{Error, Write};

use peryx_driver::serving::{MirrorAction, MirrorDriver, MirrorRequest};
use peryx_index::IndexKind;

use super::{app_with, oci_index};
use crate::registry::OciRegistry;

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
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
    let configured = images(&["library/example:latest"]);
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

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("manifest\tstore\tlibrary/example:latest"));
    assert!(output.contains("summary\tstore\t\timages\t\t\t1\timages"));
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

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        Err(Error::other("write failed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn mirror_surfaces_output_failures() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_with(&dir, oci_index("store", "oci", IndexKind::Hosted { volatile: false }));
    let configured = images(&["library/example:latest"]);
    let empty = toml::Table::new();

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
            &mut FailingWriter,
        )
        .await
        .unwrap_err();

    assert_eq!(error, "write failed");
}
