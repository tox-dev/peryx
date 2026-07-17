//! TSV report row writers and small blob/time helpers shared across the flows.

use std::time::{SystemTime, UNIX_EPOCH};

use peryx_driver::AppState;
use peryx_storage::blob::Digest;

use super::{Output, PrefetchFile, Row};

pub(super) fn write_page_row(
    out: &mut Output,
    index: &str,
    project: &str,
    status: &str,
    reason: &str,
) -> anyhow::Result<()> {
    write_row(out, Row::page(index, project, status, reason))
}

pub(super) fn write_file_row(
    out: &mut Output,
    index: &str,
    project: &str,
    file: &PrefetchFile,
    status: &str,
    reason: &str,
) -> anyhow::Result<()> {
    write_file_row_bytes(out, index, project, file, file.size, status, reason)
}

pub(super) fn write_file_row_bytes(
    out: &mut Output,
    index: &str,
    project: &str,
    file: &PrefetchFile,
    bytes: Option<u64>,
    status: &str,
    reason: &str,
) -> anyhow::Result<()> {
    write_row(
        out,
        Row {
            kind: "file",
            index,
            project,
            filename: &file.filename,
            digest: &file.digest,
            url: &file.url,
            bytes,
            status,
            reason,
        },
    )
}

pub(super) fn write_row(out: &mut Output, row: Row<'_>) -> anyhow::Result<()> {
    let bytes = row.bytes.map_or_else(String::new, |bytes| bytes.to_string());
    let cells = [
        row.kind,
        row.index,
        row.project,
        row.filename,
        row.digest,
        row.url,
        &bytes,
        row.status,
        row.reason,
    ];
    let mut separator = "";
    for cell in cells {
        out.write_all(separator.as_bytes())?;
        out.write_all(cell.as_bytes())?;
        separator = "\t";
    }
    out.write_all(b"\n")?;
    Ok(())
}

pub(super) fn write_count(out: &mut Output, index: &str, name: &str, value: u64) -> anyhow::Result<()> {
    write_row(
        out,
        Row {
            kind: "summary",
            index,
            project: "",
            filename: name,
            digest: "",
            url: "",
            bytes: Some(value),
            status: name,
            reason: "",
        },
    )
}

pub(super) async fn blob_size(state: &AppState, digest: &Digest) -> u64 {
    state
        .blobs
        .head(digest)
        .await
        .ok()
        .flatten()
        .map_or(0, |metadata| metadata.bytes)
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
