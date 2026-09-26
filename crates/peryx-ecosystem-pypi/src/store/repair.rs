use std::collections::BTreeSet;

use peryx_index::{Index, IndexKind};
use peryx_storage::blob::Digest;
use peryx_storage::meta::{DriverReadTxn, DriverTxn, MetaError, MetaStore};

use crate::parse_detail;

use super::{
    CachedIndex, FILE_PREFIX, FRESHNESS_PREFIX, INDEX_PREFIX, METADATA_PREFIX, OVERRIDE_PREFIX, PROJECTS_PREFIX,
    PROVENANCE_PREFIX, PUBLICATION_PREFIX, PypiRecords, UPLOAD_PREFIX, remove_cached_project_row,
    split_file_source_key,
};

pub use peryx_driver::serving::MetadataRepairCounts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRepairFinding {
    pub namespace: &'static str,
    pub key: String,
    pub disposition: &'static str,
    pub message: String,
    fix: RepairFix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepairFix {
    RemoveLocal { key: String },
    RemoveCachedProject { index: String, project: String },
    RemoveCachedPage { key: String },
    ReportOnly,
}

impl MetadataRepairFinding {
    fn actionable(&self) -> bool {
        self.fix != RepairFix::ReportOnly
    }
}

struct RepairSnapshot {
    index: Vec<(String, Vec<u8>)>,
    file_urls: Vec<(String, Vec<u8>)>,
    metadata: Vec<(String, Vec<u8>)>,
    publications: Vec<(String, Vec<u8>)>,
    projects: Vec<(String, Vec<u8>)>,
    uploads: Vec<(String, Vec<u8>)>,
    overrides: Vec<(String, Vec<u8>)>,
    provenance: Vec<(String, Vec<u8>)>,
}

impl RepairSnapshot {
    fn read(txn: &DriverReadTxn) -> Result<Self, MetaError> {
        Ok(Self {
            index: txn.prefix(INDEX_PREFIX)?,
            file_urls: txn.prefix(FILE_PREFIX)?,
            metadata: txn.prefix(METADATA_PREFIX)?,
            publications: txn.prefix(PUBLICATION_PREFIX)?,
            projects: txn.prefix(PROJECTS_PREFIX)?,
            uploads: txn.prefix(UPLOAD_PREFIX)?,
            overrides: txn.prefix(OVERRIDE_PREFIX)?,
            provenance: txn.prefix(PROVENANCE_PREFIX)?,
        })
    }

    fn write(txn: &DriverTxn<'_>) -> Result<Self, MetaError> {
        Ok(Self {
            index: txn.prefix(INDEX_PREFIX)?,
            file_urls: txn.prefix(FILE_PREFIX)?,
            metadata: txn.prefix(METADATA_PREFIX)?,
            publications: txn.prefix(PUBLICATION_PREFIX)?,
            projects: txn.prefix(PROJECTS_PREFIX)?,
            uploads: txn.prefix(UPLOAD_PREFIX)?,
            overrides: txn.prefix(OVERRIDE_PREFIX)?,
            provenance: txn.prefix(PROVENANCE_PREFIX)?,
        })
    }
}

/// Plans every disposition without changing the store.
///
/// # Errors
/// Returns a store error if the snapshot cannot be read.
pub fn plan_metadata_repair(meta: &MetaStore, indexes: &[Index]) -> Result<Vec<MetadataRepairFinding>, MetaError> {
    meta.read_driver_txn(|txn| RepairSnapshot::read(txn).map(|snapshot| plan(&snapshot, indexes)))
}

/// Recomputes and applies actionable dispositions in one metadata transaction.
///
/// # Errors
/// Returns a store error if the snapshot cannot be read or a repair cannot be committed.
pub fn apply_metadata_repair(meta: &MetaStore, indexes: &[Index]) -> Result<Vec<MetadataRepairFinding>, MetaError> {
    meta.commit_driver_txn(|txn| {
        let findings = plan(&RepairSnapshot::write(txn)?, indexes);
        for finding in &findings {
            apply(txn, &finding.fix)?;
        }
        Ok((findings, Vec::new()))
    })
}

#[must_use]
pub fn repair_counts(findings: &[MetadataRepairFinding]) -> MetadataRepairCounts {
    findings
        .iter()
        .fold(MetadataRepairCounts::default(), |mut counts, finding| {
            if finding.actionable() {
                counts.actionable += 1;
            } else {
                counts.report_only += 1;
            }
            counts
        })
}

fn plan(snapshot: &RepairSnapshot, indexes: &[Index]) -> Vec<MetadataRepairFinding> {
    let cached = indexes
        .iter()
        .filter(|index| index.ecosystem == crate::ECOSYSTEM && matches!(index.kind, IndexKind::Cached { .. }))
        .map(|index| index.name.as_str())
        .collect::<BTreeSet<_>>();
    let hosted = indexes
        .iter()
        .filter(|index| index.ecosystem == crate::ECOSYSTEM && matches!(index.kind, IndexKind::Hosted { .. }))
        .map(|index| index.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut findings = index_findings(&snapshot.index, &cached);
    findings.extend(text_findings(
        PypiRecords::FileUrl,
        &snapshot.file_urls,
        &cached,
        &hosted,
    ));
    findings.extend(metadata_findings(&snapshot.metadata));
    findings.extend(text_findings(
        PypiRecords::Publication,
        &snapshot.publications,
        &cached,
        &hosted,
    ));
    findings.extend(project_findings(&snapshot.projects, &cached, &hosted));
    findings.extend(report_only_binary("upload", &snapshot.uploads, invalid_upload));
    findings.extend(report_only_text(
        PypiRecords::Override,
        &snapshot.overrides,
        |key, value| !valid_upload_key(key) || super::FileOverride::decode(key, value).is_err(),
    ));
    findings.extend(report_only_text(
        PypiRecords::Provenance,
        &snapshot.provenance,
        |_, value| {
            !super::split_provenance_value("provenance", value).is_ok_and(|reference| valid_digest(reference.sha256()))
        },
    ));
    findings.sort_by(|left, right| (left.namespace, &left.key).cmp(&(right.namespace, &right.key)));
    findings
}

fn index_findings(rows: &[(String, Vec<u8>)], cached: &BTreeSet<&str>) -> Vec<MetadataRepairFinding> {
    let mut findings = Vec::new();
    for (stored_key, raw) in rows {
        let key = &stored_key[INDEX_PREFIX.len()..];
        if CachedIndex::decode(raw).is_ok_and(|record| parse_detail(&record.body).is_ok()) {
            continue;
        }
        findings.push(if split_index_project(key, cached.iter().copied()).is_some() {
            remove_page(stored_key, key, "remove unreadable cached page; refetch the project")
        } else {
            report_only("index", key, "row ownership is unknown; inspect or restore it")
        });
    }
    findings
}

fn text_findings(
    namespace: PypiRecords,
    rows: &[(String, Vec<u8>)],
    cached: &BTreeSet<&str>,
    hosted: &BTreeSet<&str>,
) -> Vec<MetadataRepairFinding> {
    rows.iter()
        .filter_map(|(stored_key, raw)| {
            let key = &stored_key[namespace.prefix().len()..];
            let valid = std::str::from_utf8(raw).is_ok_and(|value| {
                if namespace == PypiRecords::FileUrl {
                    valid_file_source_key(key, cached, hosted) && valid_file_source(value)
                } else {
                    valid_publication(value)
                }
            });
            (!valid).then(|| cache_derived_finding(namespace.label(), stored_key, key, cached, hosted))
        })
        .collect()
}

fn cache_derived_finding(
    namespace: &'static str,
    stored_key: &str,
    key: &str,
    cached: &BTreeSet<&str>,
    hosted: &BTreeSet<&str>,
) -> MetadataRepairFinding {
    let owner = split_index_project(key, cached.iter().copied().chain(hosted.iter().copied())).map(|(index, _)| index);
    if namespace == "file-url" && owner.is_none() && split_file_source_key(key).is_none() {
        return actionable(
            namespace,
            key,
            "remove",
            "remove legacy or malformed cache locator with no publication owner",
            RepairFix::RemoveLocal {
                key: stored_key.to_owned(),
            },
        );
    }
    let message = if owner.is_some_and(|index| cached.contains(index)) {
        "cached row is not safely derivable; refetch the project or restore the row"
    } else if owner.is_some_and(|index| hosted.contains(index)) {
        "hosted row is not derivable; restore it from backup or retire it through the normal delete workflow"
    } else {
        "row ownership is unknown; inspect or restore it"
    };
    report_only(namespace, key, message)
}

fn metadata_findings(rows: &[(String, Vec<u8>)]) -> Vec<MetadataRepairFinding> {
    rows.iter()
        .filter_map(|(stored_key, raw)| {
            let key = &stored_key[METADATA_PREFIX.len()..];
            let value = std::str::from_utf8(raw).ok();
            if valid_digest(key) && value.is_some_and(valid_digest) {
                return None;
            }
            Some(if valid_digest(key) {
                report_only(
                    "pep658",
                    key,
                    "metadata digest is not derivable; restore the row or rebuild metadata from the artifact",
                )
            } else {
                actionable(
                    "pep658",
                    key,
                    "remove",
                    "remove unusable row with an invalid artifact digest",
                    RepairFix::RemoveLocal {
                        key: stored_key.to_owned(),
                    },
                )
            })
        })
        .collect()
}

fn project_findings(
    rows: &[(String, Vec<u8>)],
    cached: &BTreeSet<&str>,
    hosted: &BTreeSet<&str>,
) -> Vec<MetadataRepairFinding> {
    rows.iter()
        .filter_map(|(stored_key, raw)| {
            let key = &stored_key[PROJECTS_PREFIX.len()..];
            let valid = valid_project_key(key) && std::str::from_utf8(raw).is_ok_and(|display| !display.is_empty());
            if valid {
                return None;
            }
            let Some((index, project)) = split_index_project(key, cached.iter().copied().chain(hosted.iter().copied()))
            else {
                return Some(report_only(
                    "project",
                    key,
                    "invalid project key; inspect or restore the row",
                ));
            };
            if cached.contains(index) {
                return Some(actionable(
                    "project",
                    key,
                    "remove",
                    "remove corrupt cached project row; refetch the project",
                    RepairFix::RemoveCachedProject {
                        index: index.to_owned(),
                        project: project.to_owned(),
                    },
                ));
            }
            debug_assert!(hosted.contains(index));
            Some(report_only(
                "project",
                key,
                "hosted project name is not derivable; restore it from backup",
            ))
        })
        .collect()
}

fn report_only_binary(
    namespace: &'static str,
    rows: &[(String, Vec<u8>)],
    invalid: impl Fn(&str, &[u8]) -> bool,
) -> Vec<MetadataRepairFinding> {
    rows.iter()
        .filter_map(|(stored_key, value)| {
            let key = &stored_key[UPLOAD_PREFIX.len()..];
            invalid(key, value).then(|| {
                report_only(
                    namespace,
                    key,
                    "record is not derivable; restore it from backup or retire it through the normal delete workflow",
                )
            })
        })
        .collect()
}

fn report_only_text(
    namespace: PypiRecords,
    rows: &[(String, Vec<u8>)],
    invalid: impl Fn(&str, &str) -> bool,
) -> Vec<MetadataRepairFinding> {
    rows.iter()
        .filter_map(|(stored_key, raw)| {
            let key = &stored_key[namespace.prefix().len()..];
            let invalid = std::str::from_utf8(raw).map_or(true, |value| invalid(key, value));
            invalid.then(|| {
                report_only(
                    namespace.label(),
                    key,
                    "record is not derivable; restore it from backup or retire it through the normal delete workflow",
                )
            })
        })
        .collect()
}

fn remove_page(stored_key: &str, key: &str, message: &str) -> MetadataRepairFinding {
    actionable(
        "index",
        key,
        "remove",
        message,
        RepairFix::RemoveCachedPage {
            key: stored_key.to_owned(),
        },
    )
}

fn actionable(
    namespace: &'static str,
    key: &str,
    disposition: &'static str,
    message: &str,
    fix: RepairFix,
) -> MetadataRepairFinding {
    MetadataRepairFinding {
        namespace,
        key: escape_key(key),
        disposition,
        message: message.to_owned(),
        fix,
    }
}

fn report_only(namespace: &'static str, key: &str, message: &str) -> MetadataRepairFinding {
    actionable(namespace, key, "report-only", message, RepairFix::ReportOnly)
}

fn apply(txn: &mut DriverTxn<'_>, fix: &RepairFix) -> Result<(), MetaError> {
    match fix {
        RepairFix::RemoveLocal { key } => txn.remove_local(key).map(|_| ()),
        RepairFix::RemoveCachedProject { index, project } => remove_cached_project_row(txn, index, project).map(|_| ()),
        RepairFix::RemoveCachedPage { key } => {
            txn.remove_local(key)?;
            txn.remove_local(&format!("{FRESHNESS_PREFIX}{}", &key[INDEX_PREFIX.len()..]))
                .map(|_| ())
        }
        RepairFix::ReportOnly => Ok(()),
    }
}

fn split_index_project<'a>(key: &'a str, indexes: impl Iterator<Item = &'a str>) -> Option<(&'a str, &'a str)> {
    indexes
        .filter_map(|index| {
            key.strip_prefix(index)
                .and_then(|rest| rest.strip_prefix('/'))
                .map(|project| (index, project))
        })
        .filter(|(_, project)| !project.is_empty())
        .max_by_key(|(index, _)| index.len())
}

fn valid_file_source(value: &str) -> bool {
    value.split_once('\n').is_some()
}

fn valid_file_source_key(key: &str, cached: &BTreeSet<&str>, hosted: &BTreeSet<&str>) -> bool {
    split_index_project(key, cached.iter().copied().chain(hosted.iter().copied())).map_or_else(
        || split_file_source_key(key).is_some_and(|(.., digest)| valid_digest(digest)),
        |(_, project_digest)| {
            project_digest
                .rsplit_once('/')
                .is_some_and(|(project, digest)| !project.is_empty() && valid_digest(digest))
        },
    )
}

fn valid_publication(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let mut parts = value.splitn(3, '\n');
    parts.next().is_some() && parts.next().is_some_and(valid_digest) && parts.next().is_some()
}

fn valid_digest(value: &str) -> bool {
    Digest::from_hex(value).is_some()
}

fn valid_project_key(key: &str) -> bool {
    key.split_once('/')
        .is_some_and(|(index, project)| !index.is_empty() && !project.is_empty())
}

fn valid_upload_key(key: &str) -> bool {
    let mut parts = key.splitn(3, '/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
}

fn invalid_upload(key: &str, raw: &[u8]) -> bool {
    if !valid_upload_key(key) {
        return true;
    }
    let Ok(upload) = serde_json::from_slice::<crate::upload::Uploaded>(raw) else {
        return true;
    };
    upload.file.sha256().is_none()
}

fn escape_key(key: &str) -> String {
    key.chars().flat_map(char::escape_default).collect()
}
