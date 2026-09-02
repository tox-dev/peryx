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
//!
//! Nothing about that is enforced by a type: it holds because every write path calls into this module.
//! [`audit_summary_rows`] is the check behind that convention. It recomputes what the derived rows
//! should say from the projects and uploads they describe and names every disagreement, so a write path
//! that forgets to route through here is caught by `fsck` rather than by an operator noticing a wrong
//! number. [`repair_summary_rows`] applies the same computation as a fix.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;

use peryx_driver::serving::{IndexSummary, RecentWrite};
use peryx_storage::meta::{DriverReadTxn, DriverTxn, MetaError, MetaStore};
use serde::{Deserialize, Serialize};

use super::{COUNT_PREFIX, PROJECTS_PREFIX, RECENT_PREFIX, UPLOAD_PREFIX, project_key, upload_key};

/// Whether a derived row travels with the row it describes or stays on the node that wrote it.
///
/// The rows a summary counts are themselves split this way: a hosted index's projects and uploads are
/// journaled, so a replica applies them verbatim and its counts follow; a cached index's project
/// markers are written by whichever node fetched the page and are journaled by nobody. A row-owning
/// index is cached or hosted and never both, so each index's count row has one writer discipline.
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

/// One index whose derived rows the audit checks, and the writer discipline its rows follow.
///
/// A virtual index owns no projects and no uploads, so it is not one of these: derived rows naming one
/// are rows that should not exist rather than rows that disagree.
pub struct AuditedIndex<'a> {
    pub name: &'a str,
    /// Whether this index's rows stay on the node that wrote them, which a cached index's do.
    pub local: bool,
}

impl AuditedIndex<'_> {
    const fn scope(&self) -> RowScope {
        if self.local {
            RowScope::Local
        } else {
            RowScope::Replicated
        }
    }
}

/// One derived row that disagrees with the projects and uploads it summarizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryDefect {
    /// The report namespace, either `summary-count` or `summary-order`.
    pub namespace: &'static str,
    /// The derived row, escaped: these keys carry NUL separators that no report should emit raw.
    pub key: String,
    pub message: String,
}

/// Name every derived row that disagrees with the projects and uploads it summarizes.
///
/// The whole audit reads one snapshot. Counts, order rows and the rows they describe are compared
/// against each other, so reading them from two transactions would let a write in between make correct
/// rows look broken - the failure [#1328](https://github.com/tox-dev/peryx/issues/1328) fixed for the
/// generation listings.
///
/// # Errors
/// Returns a store error if the scan fails.
pub fn audit_summary_rows(meta: &MetaStore, indexes: &[AuditedIndex<'_>]) -> Result<Vec<SummaryDefect>, MetaError> {
    let findings = meta.read_driver_txn(|txn| find_defects(txn, indexes))?;
    Ok(findings.into_iter().map(|finding| finding.defect).collect())
}

/// Rebuild every derived row [`audit_summary_rows`] names, and report what was rebuilt.
///
/// Each repaired row is recomputed from the projects and uploads it summarizes, so the write is the one
/// the write path would have made. Rows are written on the footing their index's writes use, which
/// leaves an offline repair node-local either way: a transaction carrying no journal entry keeps no
/// replicated mutation. That is the behaviour a derived row wants, because every node rebuilds the same
/// rows from underlying rows it already agrees about.
///
/// # Errors
/// Returns a store error if the scan or the write fails.
pub fn repair_summary_rows(meta: &MetaStore, indexes: &[AuditedIndex<'_>]) -> Result<Vec<SummaryDefect>, MetaError> {
    let findings = meta.read_driver_txn(|txn| find_defects(txn, indexes))?;
    if findings.is_empty() {
        return Ok(Vec::new());
    }
    meta.commit_driver_txn(|txn| {
        for finding in &findings {
            match &finding.fix {
                RowFix::Write(value) => write_row(txn, finding.scope, &finding.row, value)?,
                RowFix::Remove => remove_row(txn, finding.scope, &finding.row)?,
            };
        }
        Ok::<_, MetaError>(((), Vec::new()))
    })?;
    Ok(findings.into_iter().map(|finding| finding.defect).collect())
}

/// The derived rows the store holds, for a report that counts every namespace it keeps.
///
/// # Errors
/// Returns a store error if the scan fails.
pub fn summary_row_counts(meta: &MetaStore) -> Result<SummaryRowCounts, MetaError> {
    meta.read_driver_txn(|txn| {
        let mut counts = SummaryRowCounts::default();
        txn.scan_prefix(COUNT_PREFIX, |_, _| {
            counts.count_rows += 1;
            Ok::<_, MetaError>(ControlFlow::Continue(()))
        })?;
        txn.scan_prefix(RECENT_PREFIX, |_, _| {
            counts.order_rows += 1;
            Ok::<_, MetaError>(ControlFlow::Continue(()))
        })?;
        Ok(counts)
    })
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SummaryRowCounts {
    pub count_rows: u64,
    pub order_rows: u64,
}

/// A defect and the write that settles it.
struct Finding {
    defect: SummaryDefect,
    row: String,
    fix: RowFix,
    scope: RowScope,
}

enum RowFix {
    Write(Vec<u8>),
    Remove,
}

/// What one index's derived rows should say, recomputed from the rows they describe.
#[derive(Default)]
struct Expectation {
    counts: Counts,
    orders: BTreeMap<String, ExpectedOrder>,
}

impl Expectation {
    /// A count row is absent until an index holds something, and goes away with the last row it
    /// counted, so an absent row and a pair of zeroes are the same state and only one of them is
    /// written.
    const fn desired_counts(&self) -> Option<Counts> {
        if self.counts.projects == 0 && self.counts.uploads == 0 {
            None
        } else {
            Some(self.counts)
        }
    }
}

struct ExpectedOrder {
    record: RecentRecord,
    upload: String,
}

const COUNT_NAMESPACE: &str = "summary-count";
const ORDER_NAMESPACE: &str = "summary-order";

/// What each audited index's derived rows should say, recomputed from the projects and uploads they
/// summarize.
fn expected_rows<'a>(
    txn: &DriverReadTxn,
    indexes: &[AuditedIndex<'a>],
) -> Result<BTreeMap<&'a str, Expectation>, MetaError> {
    let mut expected: BTreeMap<&str, Expectation> = indexes
        .iter()
        .map(|index| (index.name, Expectation::default()))
        .collect();
    let mut ordered: Vec<&str> = indexes.iter().map(|index| index.name).collect();
    // Longest first, so `root/hosted` claims its own rows before `root` can match their leading segment.
    ordered.sort_by_key(|name| Reverse(name.len()));

    // A key the write path could not have produced is counted for no index. It is damage the key
    // checks already name, and counting it here would report the same row twice and then invent a
    // derived row to match it.
    txn.scan_prefix(PROJECTS_PREFIX, |key, _| {
        if let Some((index, project)) = split_index(&key[PROJECTS_PREFIX.len()..], &ordered)
            && !project.is_empty()
            && let Some(entry) = expected.get_mut(index)
        {
            entry.counts.projects += 1;
        }
        Ok::<_, MetaError>(ControlFlow::Continue(()))
    })?;
    txn.scan_prefix(UPLOAD_PREFIX, |key, value| {
        if let Some((index, rest)) = split_index(&key[UPLOAD_PREFIX.len()..], &ordered)
            && let Some((project, filename)) = rest.split_once('/')
            && !project.is_empty()
            && !filename.is_empty()
            && let Some(entry) = expected.get_mut(index)
        {
            entry.counts.uploads += 1;
            // A record that is not JSON holds no order row by design, and is still a write that counts.
            if let Some(recent) = recent_upload(project, filename, value) {
                entry.orders.insert(
                    order_key(index, &recent, filename),
                    ExpectedOrder {
                        record: RecentRecord::from(&recent),
                        upload: format!("{index}/{project}/{filename}"),
                    },
                );
            }
        }
        Ok::<_, MetaError>(ControlFlow::Continue(()))
    })?;
    Ok(expected)
}

fn find_defects(txn: &DriverReadTxn, indexes: &[AuditedIndex<'_>]) -> Result<Vec<Finding>, MetaError> {
    let expected = expected_rows(txn, indexes)?;
    let scopes: HashMap<&str, RowScope> = indexes.iter().map(|index| (index.name, index.scope())).collect();
    let mut findings = Vec::new();
    let mut counted: BTreeSet<&str> = BTreeSet::new();
    txn.scan_prefix(COUNT_PREFIX, |key, value| {
        let Some((index, entry)) = expected.get_key_value(&key[COUNT_PREFIX.len()..]) else {
            findings.push(unexpected_row(COUNT_NAMESPACE, key));
            return Ok(ControlFlow::Continue(()));
        };
        counted.insert(index);
        let desired = entry.desired_counts();
        let message = match Counts::decode(key, Some(value)) {
            Ok(found) if Some(found) == desired => return Ok(ControlFlow::Continue(())),
            Ok(found) => format!("count row says {found}, rows hold {}", describe(desired)),
            Err(error) => error.to_string(),
        };
        findings.push(count_finding(scopes[index], key, desired, message));
        Ok::<_, MetaError>(ControlFlow::Continue(()))
    })?;
    for (index, entry) in &expected {
        if let Some(desired) = entry.desired_counts()
            && !counted.contains(index)
        {
            let key = count_key(index);
            let message = format!("count row is absent, rows hold {desired}");
            findings.push(count_finding(scopes[index], &key, Some(desired), message));
        }
    }

    let mut matched: BTreeSet<&str> = BTreeSet::new();
    txn.scan_prefix(RECENT_PREFIX, |key, value| {
        // Only the index name is read out of an order key, and only as far as the NUL that terminates
        // it: a key without that terminator sits in no index's order range. Whether the rest of the key
        // is well formed is settled by whether an upload produced that exact key, which no parse of the
        // key can be fooled about - the artifact segment comes from the record's own filename field and
        // is not guaranteed to be NUL-free.
        let Some((index, entry)) = key[RECENT_PREFIX.len()..]
            .split_once('\u{0}')
            .and_then(|(index, _)| expected.get_key_value(index))
        else {
            findings.push(unexpected_row(ORDER_NAMESPACE, key));
            return Ok(ControlFlow::Continue(()));
        };
        let Some((order, want)) = entry.orders.get_key_value(key) else {
            findings.push(order_finding(
                scopes[index],
                key,
                RowFix::Remove,
                "order row has no upload".to_owned(),
            ));
            return Ok(ControlFlow::Continue(()));
        };
        matched.insert(order);
        let message = match serde_json::from_slice::<RecentRecord>(value) {
            Ok(found) if found == want.record => return Ok(ControlFlow::Continue(())),
            Ok(_) => format!("order row does not match upload {}", want.upload),
            Err(error) => error.to_string(),
        };
        findings.push(order_finding(
            scopes[index],
            key,
            RowFix::Write(serde_json::to_vec(&want.record)?),
            message,
        ));
        Ok::<_, MetaError>(ControlFlow::Continue(()))
    })?;
    for (index, entry) in &expected {
        for (order, want) in &entry.orders {
            if !matched.contains(order.as_str()) {
                let message = format!("upload {} has no order row", want.upload);
                let fix = RowFix::Write(serde_json::to_vec(&want.record)?);
                findings.push(order_finding(scopes[index], order, fix, message));
            }
        }
    }
    Ok(findings)
}

/// The index one project or upload key names, and the rest of the key after it.
fn split_index<'a>(key: &'a str, ordered: &[&'a str]) -> Option<(&'a str, &'a str)> {
    ordered
        .iter()
        .find_map(|index| Some((*index, key.strip_prefix(index)?.strip_prefix('/')?)))
}

fn count_finding(scope: RowScope, key: &str, desired: Option<Counts>, message: String) -> Finding {
    Finding {
        defect: SummaryDefect {
            namespace: COUNT_NAMESPACE,
            key: format!("{key:?}"),
            message,
        },
        row: key.to_owned(),
        fix: desired.map_or(RowFix::Remove, |counts| RowFix::Write(counts.encode().into_bytes())),
        scope,
    }
}

fn order_finding(scope: RowScope, key: &str, fix: RowFix, message: String) -> Finding {
    Finding {
        defect: SummaryDefect {
            namespace: ORDER_NAMESPACE,
            key: format!("{key:?}"),
            message,
        },
        row: key.to_owned(),
        fix,
        scope,
    }
}

/// A derived row for an index that owns none. It is removed on the node that finds it rather than
/// through a replicated delete, because nothing establishes which discipline wrote it and no other node
/// holds it legitimately.
fn unexpected_row(namespace: &'static str, key: &str) -> Finding {
    Finding {
        defect: SummaryDefect {
            namespace,
            key: format!("{key:?}"),
            message: "no cached or hosted index owns this row".to_owned(),
        },
        row: key.to_owned(),
        fix: RowFix::Remove,
        scope: RowScope::Local,
    }
}

fn describe(counts: Option<Counts>) -> String {
    counts.map_or_else(|| "none".to_owned(), |counts| counts.to_string())
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

impl std::fmt::Display for Counts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} projects and {} uploads", self.projects, self.uploads)
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[cfg(test)]
#[path = "../../tests/unit/store/summary/audit_tests.rs"]
mod audit_tests;
