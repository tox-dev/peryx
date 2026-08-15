use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use peryx_policy::{PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{
    AccountingClass, LegacyMetadataSource, MetadataMigration, MetadataRecord, MetadataRecordSet, MetadataValueKind,
    PolicyDecisionRecord, PolicyInputGeneration, QuotaLimit, QuotaReservationRecord, QuotaReservationState, QuotaUsage,
    QuotaValue,
};

use crate::PypiPlugin;

impl MetadataMigration for PypiPlugin {
    fn name(&self) -> &'static str {
        "pypi-v1-metadata"
    }

    fn record_sets(&self) -> &'static [MetadataRecordSet] {
        &[
            MetadataRecordSet::QuotaUsage,
            MetadataRecordSet::QuotaResource,
            MetadataRecordSet::QuotaReservation,
            MetadataRecordSet::PolicyDecisionHistory,
            MetadataRecordSet::PolicyDecisionCurrent,
            MetadataRecordSet::PolicyDecisionCurrentById,
            MetadataRecordSet::Analytics,
        ]
    }

    fn legacy_sources(&self) -> &'static [LegacyMetadataSource] {
        &[
            LegacyMetadataSource {
                table: "quota_project",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaResource,
            },
            LegacyMetadataSource {
                table: "quota_version",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaGroup,
            },
        ]
    }

    fn rewrite(
        &self,
        record_set: MetadataRecordSet,
        record: &MetadataRecord,
    ) -> Result<Option<MetadataRecord>, String> {
        Ok(match record_set {
            MetadataRecordSet::QuotaUsage => rewrite_json::<QuotaUsage, LegacyQuotaUsage, _>(record, Into::into),
            MetadataRecordSet::QuotaResource => {
                rewrite_json::<ResourceUsage, LegacyResourceUsage, _>(record, Into::into)
            }
            MetadataRecordSet::QuotaGroup => Some(record.clone()),
            MetadataRecordSet::QuotaReservation => {
                rewrite_json::<QuotaReservationRecord, LegacyQuotaReservation, _>(record, Into::into)
            }
            MetadataRecordSet::PolicyDecisionHistory => {
                rewrite_json::<PolicyDecisionRecord, LegacyPolicyDecisionRecord, _>(record, Into::into)
            }
            MetadataRecordSet::PolicyDecisionCurrent => rewrite_policy_subject(record, true),
            MetadataRecordSet::PolicyDecisionCurrentById => rewrite_policy_subject(record, false),
            MetadataRecordSet::Analytics => rewrite_analytics(record),
        })
    }
}

fn rewrite_json<Current, Legacy, Convert>(record: &MetadataRecord, convert: Convert) -> Option<MetadataRecord>
where
    Current: DeserializeOwned + Serialize,
    Legacy: DeserializeOwned,
    Convert: FnOnce(Legacy) -> Current,
{
    if serde_json::from_slice::<Current>(&record.value).is_ok() {
        return None;
    }
    let Ok(legacy) = serde_json::from_slice::<Legacy>(&record.value) else {
        return None;
    };
    Some(MetadataRecord {
        key: record.key.clone(),
        value: serde_json::to_vec(&convert(legacy)).expect("owned migration records serialize"),
    })
}

fn rewrite_policy_subject(record: &MetadataRecord, key: bool) -> Option<MetadataRecord> {
    let subject = if key {
        record.key.as_bytes()
    } else {
        record.value.as_slice()
    };
    if serde_json::from_slice::<PolicyDecisionSubject>(subject).is_ok() {
        return None;
    }
    let Ok(legacy) = serde_json::from_slice::<LegacyPolicyDecisionSubject>(subject) else {
        return None;
    };
    let encoded =
        serde_json::to_string(&PolicyDecisionSubject::from(legacy)).expect("owned policy decision subjects serialize");
    Some(if key {
        MetadataRecord {
            key: encoded,
            value: record.value.clone(),
        }
    } else {
        MetadataRecord {
            key: record.key.clone(),
            value: encoded.into_bytes(),
        }
    })
}

fn rewrite_analytics(record: &MetadataRecord) -> Option<MetadataRecord> {
    match record.key.as_str() {
        "reads" if serde_json::from_slice::<ReadSnapshot>(&record.value).is_ok() => None,
        "reads" | "downloads" => {
            rewrite_json::<ReadSnapshot, LegacyReadSnapshot, _>(record, Into::into).map(|mut rewritten| {
                "reads".clone_into(&mut rewritten.key);
                rewritten
            })
        }
        "daily_usage" if serde_json::from_slice::<DailySnapshot>(&record.value).is_ok() => None,
        "daily_usage" => rewrite_json::<DailySnapshot, LegacyDailySnapshot, _>(record, Into::into),
        _ => None,
    }
}

#[derive(Deserialize)]
struct LegacyQuotaUsage {
    file_bytes: QuotaValue,
    accounted_bytes: QuotaValue,
    projects: QuotaValue,
}

impl From<LegacyQuotaUsage> for QuotaUsage {
    fn from(usage: LegacyQuotaUsage) -> Self {
        Self {
            artifact_bytes: usage.file_bytes,
            accounted_bytes: usage.accounted_bytes,
            resources: usage.projects,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct References {
    committed: u64,
    reserved: u64,
}

#[derive(Serialize, Deserialize)]
struct ResourceUsage {
    references: References,
    artifact_bytes: QuotaValue,
    groups: QuotaValue,
}

#[derive(Deserialize)]
struct LegacyResourceUsage {
    references: References,
    #[serde(default)]
    file_bytes: QuotaValue,
    versions: QuotaValue,
}

impl From<LegacyResourceUsage> for ResourceUsage {
    fn from(usage: LegacyResourceUsage) -> Self {
        Self {
            references: usage.references,
            artifact_bytes: usage.file_bytes,
            groups: usage.versions,
        }
    }
}

#[derive(Deserialize)]
struct LegacyQuotaReservation {
    id: uuid::Uuid,
    repository: String,
    project: Option<String>,
    version: Option<String>,
    digest: String,
    bytes: u64,
    class: AccountingClass,
    state: QuotaReservationState,
    created_at_unix: i64,
    violations: Vec<LegacyQuotaLimit>,
    #[serde(default)]
    project_file_bytes: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyQuotaLimit {
    #[serde(rename = "file_bytes")]
    ArtifactBytes,
    AccountedBytes,
    #[serde(rename = "projects")]
    Resources,
    #[serde(rename = "versions_per_project")]
    GroupsPerResource,
}

impl From<LegacyQuotaLimit> for QuotaLimit {
    fn from(limit: LegacyQuotaLimit) -> Self {
        match limit {
            LegacyQuotaLimit::ArtifactBytes => Self::ArtifactBytes,
            LegacyQuotaLimit::AccountedBytes => Self::AccountedBytes,
            LegacyQuotaLimit::Resources => Self::Resources,
            LegacyQuotaLimit::GroupsPerResource => Self::GroupsPerResource,
        }
    }
}

impl From<LegacyQuotaReservation> for QuotaReservationRecord {
    fn from(record: LegacyQuotaReservation) -> Self {
        Self {
            id: record.id,
            repository: record.repository,
            resource: record.project,
            group: record.version,
            digest: record.digest,
            bytes: record.bytes,
            class: record.class,
            state: record.state,
            created_at_unix: record.created_at_unix,
            violations: record.violations.into_iter().map(Into::into).collect(),
            resource_artifact_bytes: record.project_file_bytes,
        }
    }
}

#[derive(Deserialize)]
struct LegacyPolicyDecisionRecord {
    id: uuid::Uuid,
    repository: String,
    project: String,
    version: Option<String>,
    filename: Option<String>,
    source: Option<String>,
    action: PolicyAction,
    state: PolicyDecisionState,
    rule: Option<String>,
    reason: Option<String>,
    evaluated_at_unix: i64,
    input_generation: PolicyInputGeneration,
    next_eligible_at_unix: Option<i64>,
}

impl From<LegacyPolicyDecisionRecord> for PolicyDecisionRecord {
    fn from(record: LegacyPolicyDecisionRecord) -> Self {
        Self {
            id: record.id,
            repository: record.repository,
            resource: record.project,
            group: record.version,
            artifact: record.filename,
            source: record.source,
            action: record.action,
            state: record.state,
            rule: record.rule,
            reason: record.reason,
            evaluated_at_unix: record.evaluated_at_unix,
            input_generation: record.input_generation,
            next_eligible_at_unix: record.next_eligible_at_unix,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PolicyDecisionSubject {
    repository: String,
    resource: String,
    group: Option<String>,
    artifact: Option<String>,
    source: Option<String>,
    action: PolicyAction,
}

#[derive(Deserialize)]
struct LegacyPolicyDecisionSubject {
    repository: String,
    project: String,
    version: Option<String>,
    filename: Option<String>,
    source: Option<String>,
    action: PolicyAction,
}

impl From<LegacyPolicyDecisionSubject> for PolicyDecisionSubject {
    fn from(subject: LegacyPolicyDecisionSubject) -> Self {
        Self {
            repository: subject.repository,
            resource: subject.project,
            group: subject.version,
            artifact: subject.filename,
            source: subject.source,
            action: subject.action,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ArtifactUsageRow {
    repository: String,
    resource: String,
    artifact: String,
    reads: u64,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct ReadSnapshot {
    artifacts: Vec<ArtifactUsageRow>,
}

#[derive(Deserialize)]
struct LegacyArtifactUsageRow {
    route: String,
    project: String,
    filename: String,
    downloads: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct LegacyReadSnapshot {
    files: Vec<LegacyArtifactUsageRow>,
}

impl From<LegacyReadSnapshot> for ReadSnapshot {
    fn from(snapshot: LegacyReadSnapshot) -> Self {
        Self {
            artifacts: snapshot
                .files
                .into_iter()
                .map(|row| ArtifactUsageRow {
                    repository: row.route,
                    resource: row.project,
                    artifact: row.filename,
                    reads: row.downloads,
                    bytes: row.bytes,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct DailyUsage {
    day: i64,
    repository: String,
    resource: String,
    group: String,
    source: String,
    reads: u64,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct DailySnapshot {
    schema: u32,
    buckets: Vec<DailyUsage>,
}

#[derive(Deserialize)]
struct LegacyDailyUsage {
    day: i64,
    repository: String,
    project: String,
    version: String,
    source: String,
    downloads: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct LegacyDailySnapshot {
    schema: u32,
    buckets: Vec<LegacyDailyUsage>,
}

impl From<LegacyDailySnapshot> for DailySnapshot {
    fn from(snapshot: LegacyDailySnapshot) -> Self {
        Self {
            schema: snapshot.schema,
            buckets: snapshot
                .buckets
                .into_iter()
                .map(|row| DailyUsage {
                    day: row.day,
                    repository: row.repository,
                    resource: row.project,
                    group: row.version,
                    source: row.source,
                    reads: row.downloads,
                    bytes: row.bytes,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/migration_tests.rs"]
mod tests;
