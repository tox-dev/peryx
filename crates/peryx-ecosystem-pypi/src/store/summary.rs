//! The per-index summary the status contract exposes, and the rows that answer it.
//!
//! A summary asks a bounded question - how many projects, how many uploads, and the newest few - of a
//! history with no bound at all, so it is maintained as rows rather than recomputed by walking the
//! namespace it describes. Every write that adds, rewrites, or removes a project or upload row goes
//! through this module, which carries a per-index count row and one order row per upload in the same
//! transaction as the row it describes. A crash, a replica replay, or a purge therefore cannot leave a
//! count that disagrees with the rows it counts.
//!
//! Reading is then one point lookup per index plus the first `recent_limit` rows of that index's order
//! range, whether the index holds a hundred uploads or a hundred million.

use std::collections::HashMap;
use std::ops::ControlFlow;

use peryx_driver::serving::{IndexSummary, RecentWrite};
use peryx_storage::meta::{DriverTxn, MetaError, MetaStore};
use serde::{Deserialize, Serialize};

use super::{COUNT_PREFIX, RECENT_PREFIX, project_key, upload_key};

/// Whether a derived row travels with the row it describes or stays on the node that wrote it.
///
/// The rows a summary counts are themselves split this way: a hosted index's projects and uploads are
/// journaled, so a replica applies them verbatim and its counts follow; a cached index's project
/// markers are written by whichever node fetched the page and are journaled by nobody. An index is
/// cached or hosted and never both, so each index's count row has one writer discipline.
#[derive(Clone, Copy)]
enum RowScope {
    Replicated,
    Local,
}

/// Record a published project on a hosted index, keeping the index's project count in step.
pub fn put_project_row(txn: &mut DriverTxn, index: &str, normalized: &str, display: &str) -> Result<(), MetaError> {
    write_project_row(txn, RowScope::Replicated, index, normalized, display)
}

/// Record a project a cached page brought in, whose marker no journal carries.
pub fn put_cached_project_row(
    txn: &mut DriverTxn,
    index: &str,
    normalized: &str,
    display: &str,
) -> Result<(), MetaError> {
    write_project_row(txn, RowScope::Local, index, normalized, display)
}

/// Drop a cached project's marker, keeping the index's project count in step. Returns whether it was
/// there.
pub fn remove_cached_project_row(txn: &mut DriverTxn, index: &str, normalized: &str) -> Result<bool, MetaError> {
    let scope = RowScope::Local;
    let removed = remove_row(txn, scope, &project_key(index, normalized))?;
    if removed {
        adjust_counts(txn, scope, index, -1, 0)?;
    }
    Ok(removed)
}

fn write_project_row(
    txn: &mut DriverTxn,
    scope: RowScope,
    index: &str,
    normalized: &str,
    display: &str,
) -> Result<(), MetaError> {
    if write_row(txn, scope, &project_key(index, normalized), display.as_bytes())? {
        adjust_counts(txn, scope, index, 1, 0)?;
    }
    Ok(())
}

/// Write one upload record, keeping the index's upload count and order row in step.
///
/// The order row is keyed from the record being replaced as well as the one being written, because a
/// rewrite that moves an upload's time or filename has to retire the position it used to hold.
pub fn put_upload_row(
    txn: &mut DriverTxn,
    index: &str,
    normalized: &str,
    filename: &str,
    record: &[u8],
) -> Result<(), MetaError> {
    let scope = RowScope::Replicated;
    let key = upload_key(index, normalized, filename);
    let previous = txn.get(&key)?;
    retire_order_row(txn, scope, index, normalized, filename, previous.as_deref())?;
    if write_row(txn, scope, &key, record)? {
        adjust_counts(txn, scope, index, 0, 1)?;
    }
    if let Some(recent) = recent_upload(normalized, filename, record) {
        let value = serde_json::to_vec(&RecentRecord::from(&recent))?;
        write_row(txn, scope, &order_key(index, &recent, filename), &value)?;
    }
    Ok(())
}

/// Remove one upload record the caller has already read in this transaction, keeping the index's upload
/// count and order row in step.
pub fn remove_upload_row(
    txn: &mut DriverTxn,
    index: &str,
    normalized: &str,
    filename: &str,
    record: &[u8],
) -> Result<(), MetaError> {
    let scope = RowScope::Replicated;
    retire_order_row(txn, scope, index, normalized, filename, Some(record))?;
    remove_row(txn, scope, &upload_key(index, normalized, filename))?;
    adjust_counts(txn, scope, index, 0, -1)
}

/// The counts and newest writes of each named index, read from the maintained rows.
///
/// # Errors
/// Returns a store error if the read fails or a maintained row cannot be decoded.
pub fn summarize_indexes(
    meta: &MetaStore,
    index_names: &[String],
    recent_limit: usize,
) -> Result<HashMap<String, IndexSummary>, MetaError> {
    meta.read_driver_txn(|txn| {
        index_names
            .iter()
            .map(|name| {
                let key = count_key(name);
                let counts = Counts::decode(&key, txn.get(&key)?.as_deref())?;
                let mut recent_writes = Vec::new();
                txn.scan_prefix(&order_prefix(name), |key, value| {
                    if recent_writes.len() == recent_limit {
                        return Ok(ControlFlow::Break(()));
                    }
                    recent_writes.push(RecentWrite::from(
                        serde_json::from_slice::<RecentRecord>(value).map_err(|source| {
                            MetaError::DriverRecordMalformed {
                                key: key.to_owned(),
                                source,
                            }
                        })?,
                    ));
                    Ok::<_, MetaError>(ControlFlow::Continue(()))
                })?;
                Ok((
                    name.clone(),
                    IndexSummary {
                        resource_count: counts.projects,
                        write_count: counts.uploads,
                        recent_writes,
                    },
                ))
            })
            .collect()
    })
}

fn count_key(index: &str) -> String {
    format!("{COUNT_PREFIX}{index}")
}

/// The order range of one index. The index name is terminated rather than joined with a separator that
/// could also occur inside it, so the range of `root` cannot swallow the range of `root/hosted`.
fn order_prefix(index: &str) -> String {
    format!("{RECENT_PREFIX}{index}\u{0}")
}

/// One upload's position in its index's order range: newest time first, then the filename the record
/// carries, then the project and stored filename that make the position unique.
///
/// That is the order the previous full-history sort produced, minus the sort: the summary reads the
/// front of the range and stops.
fn order_key(index: &str, recent: &RecentWrite, filename: &str) -> String {
    format!(
        "{}{}/{}\u{0}{}\u{0}{filename}",
        order_prefix(index),
        order_segment(recent.written_at.as_deref()),
        recent.artifact,
        recent.resource,
    )
}

/// The segment that sorts an upload time descending inside an ascending key range.
///
/// A time peryx cannot read as RFC 3339 - absent, or damaged - sorts behind every readable one, which
/// is where a missing time already sat, and matches the release-delay rule's reading of the same field.
fn order_segment(written_at: Option<&str>) -> String {
    written_at.and_then(crate::policy::parse_upload_time).map_or_else(
        || UNREADABLE_TIME_SEGMENT.to_owned(),
        |seconds| format!("0{:020}", !order_rank(seconds)),
    )
}

/// Sorts behind every `order_segment` of a readable time, which all start `0`.
const UNREADABLE_TIME_SEGMENT: &str = "100000000000000000000";

/// Map a signed Unix second onto an unsigned rank with the same order, so the bitwise complement of the
/// rank reverses it.
const fn order_rank(seconds: i64) -> u64 {
    seconds.cast_unsigned() ^ (1 << 63)
}

/// Drop the order row a record held before this write replaced or removed it. A record that never
/// parsed never had one.
fn retire_order_row(
    txn: &mut DriverTxn,
    scope: RowScope,
    index: &str,
    normalized: &str,
    filename: &str,
    previous: Option<&[u8]>,
) -> Result<(), MetaError> {
    let Some(recent) = previous.and_then(|record| recent_upload(normalized, filename, record)) else {
        return Ok(());
    };
    remove_row(txn, scope, &order_key(index, &recent, filename)).map(|_| ())
}

fn write_row(txn: &mut DriverTxn, scope: RowScope, key: &str, value: &[u8]) -> Result<bool, MetaError> {
    match scope {
        RowScope::Replicated => txn.upsert(key, value),
        RowScope::Local => txn.upsert_local(key, value),
    }
}

fn remove_row(txn: &mut DriverTxn, scope: RowScope, key: &str) -> Result<bool, MetaError> {
    match scope {
        RowScope::Replicated => txn.remove(key),
        RowScope::Local => txn.remove_local(key),
    }
}

fn adjust_counts(
    txn: &mut DriverTxn,
    scope: RowScope,
    index: &str,
    projects: i64,
    uploads: i64,
) -> Result<(), MetaError> {
    let key = count_key(index);
    let counts = Counts::decode(&key, txn.get(&key)?.as_deref())?.shifted(projects, uploads);
    if counts == Counts::default() {
        remove_row(txn, scope, &key)?;
    } else {
        write_row(txn, scope, &key, counts.encode().as_bytes())?;
    }
    Ok(())
}

/// One index's row counts, absent until its first project or upload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    projects: u64,
    uploads: u64,
}

impl Counts {
    fn decode(key: &str, raw: Option<&[u8]>) -> Result<Self, MetaError> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        let value = super::record_str(key, raw.to_vec())?;
        let (projects, uploads) = value.split_once('\n').ok_or_else(|| MetaError::DriverRecordMissing {
            key: key.to_owned(),
            field: "uploads",
        })?;
        Ok(Self {
            projects: parse_count(key, "projects", projects)?,
            uploads: parse_count(key, "uploads", uploads)?,
        })
    }

    fn encode(self) -> String {
        format!("{}\n{}", self.projects, self.uploads)
    }

    const fn shifted(self, projects: i64, uploads: i64) -> Self {
        Self {
            projects: self.projects.saturating_add_signed(projects),
            uploads: self.uploads.saturating_add_signed(uploads),
        }
    }
}

fn parse_count(key: &str, field: &'static str, value: &str) -> Result<u64, MetaError> {
    value.parse().map_err(|source| MetaError::DriverRecordInteger {
        key: key.to_owned(),
        field,
        source,
    })
}

/// The stored form of one recent write. The order key fixes where the row sits; this is what the
/// summary reports, so a read decodes exactly the rows it returns.
#[derive(Debug, Serialize, Deserialize)]
struct RecentRecord {
    resource: String,
    artifact: String,
    group: String,
    written_at: Option<String>,
    size: Option<u64>,
}

impl From<&RecentWrite> for RecentRecord {
    fn from(write: &RecentWrite) -> Self {
        Self {
            resource: write.resource.clone(),
            artifact: write.artifact.clone(),
            group: write.group.clone(),
            written_at: write.written_at.clone(),
            size: write.size,
        }
    }
}

impl From<RecentRecord> for RecentWrite {
    fn from(record: RecentRecord) -> Self {
        Self {
            resource: record.resource,
            artifact: record.artifact,
            group: record.group,
            written_at: record.written_at,
            size: record.size,
        }
    }
}

/// What a stored upload record says about itself, or `None` when it is not JSON at all. A record that
/// parses but names none of these fields still counts as a write; it just reports empty ones.
fn recent_upload(project: &str, fallback_filename: &str, bytes: &[u8]) -> Option<RecentWrite> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(RecentWrite {
        resource: project.to_owned(),
        artifact: value["file"]["filename"]
            .as_str()
            .unwrap_or(fallback_filename)
            .to_owned(),
        group: value["version"].as_str().unwrap_or_default().to_owned(),
        written_at: value["file"]["upload-time"].as_str().map(str::to_owned),
        size: value["file"]["size"].as_u64(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/store/summary/tests.rs"]
mod tests;
