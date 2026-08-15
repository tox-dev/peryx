use peryx_storage::blob::Digest;

use super::{blob_size, unix_now, write_count, write_file_row, write_file_row_bytes, write_page_row, write_row};
use crate::mirror::test_support;
use crate::mirror::{PrefetchFile, PrefetchMetadata, Row};

fn file() -> PrefetchFile {
    PrefetchFile {
        filename: "demo-1.0.tar.gz".to_owned(),
        digest: "a".repeat(64),
        url: "https://example.test/demo-1.0.tar.gz".to_owned(),
        size: Some(7),
        metadata: Some(PrefetchMetadata {
            url: "https://example.test/demo-1.0.tar.gz.metadata".to_owned(),
            digest: "b".repeat(64),
        }),
        source: None,
    }
}

#[test]
fn report_writes_each_row_shape() {
    let mut output = Vec::new();
    let file = file();

    write_page_row(&mut output, "pypi", "demo", "selected", "").unwrap();
    write_file_row(&mut output, "pypi", "demo", &file, "cached", "").unwrap();
    write_file_row_bytes(&mut output, "pypi", "demo", &file, None, "missing", "not cached").unwrap();
    write_count(&mut output, "pypi", "files", 2).unwrap();
    write_row(
        &mut output,
        Row::metadata(
            "pypi",
            "demo",
            "demo.metadata",
            file.metadata.as_ref().unwrap(),
            Some(3),
            "cached",
            "",
        ),
    )
    .unwrap();

    let rows = String::from_utf8(output).unwrap();
    assert!(rows.contains("page\tpypi\tdemo\t\t\t\t\tselected\t\n"));
    assert!(rows.contains("file\tpypi\tdemo\tdemo-1.0.tar.gz"));
    assert!(rows.contains("summary\tpypi\t\tfiles\t\t\t2\tfiles\t\n"));
    assert!(rows.contains("metadata\tpypi\tdemo\tdemo.metadata"));
}

#[tokio::test]
async fn blob_size_reports_present_and_missing_blobs() {
    let fixture = test_support::state(Vec::new());
    let present = Digest::of(b"present");
    fixture
        .state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(b"present", &present)
        .unwrap();

    assert_eq!(blob_size(&fixture.state.serving, &present).await, 7);
    assert_eq!(blob_size(&fixture.state.serving, &Digest::of(b"missing")).await, 0);
    assert!(fixture.dir.path().exists());
}

#[test]
fn unix_now_is_after_the_epoch() {
    assert!(unix_now() > 0);
}
