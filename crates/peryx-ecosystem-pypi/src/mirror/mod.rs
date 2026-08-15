use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use peryx_driver::serving::{MirrorAction, MirrorDriver, MirrorRequest};
use peryx_upstream::UpstreamClient;

use crate::{DistributionFilename, PypiServing, Version, VersionSpecifiers};

mod report;
mod run;
mod selection;

#[async_trait::async_trait]
impl MirrorDriver for PypiServing {
    async fn mirror(
        &self,
        state: Arc<peryx_driver::AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn Write + Send),
    ) -> Result<(), String> {
        let configured = PrefetchConfig::from_table(request.configured)?;
        let options = PrefetchOptions::from_table(request.overrides)?;
        match request.action {
            MirrorAction::Plan => run::pypi_plan(&configured, &state, request.index, &options, output).await,
            MirrorAction::Sync => run::pypi_sync(&configured, &state, request.index, &options, output).await,
            MirrorAction::Verify => run::pypi_verify(&configured, &state, request.index, &options, output).await,
        }
        .map_err(|error| error.to_string())
    }
}

const HEADER: &str = "kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefetchMode {
    All,
    Selected,
    MetadataOnly,
}

#[derive(Debug, Clone)]
struct PrefetchConfig {
    mode: PrefetchMode,
    packages: Vec<String>,
    requirements: Vec<PathBuf>,
    include_wheels: bool,
    include_sdists: bool,
    python_tags: Vec<String>,
    abi_tags: Vec<String>,
    platform_tags: Vec<String>,
    max_file_size_bytes: Option<u64>,
    metadata_only: bool,
}

impl PrefetchConfig {
    fn from_table(table: &toml::Table) -> Result<Self, String> {
        Ok(Self {
            mode: mode(table.get("mode").and_then(toml::Value::as_str).unwrap_or("selected"))?,
            packages: table_strings(table, "packages")?,
            requirements: table_strings(table, "requirements")?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            include_wheels: table_bool(table, "include_wheels", true)?,
            include_sdists: table_bool(table, "include_sdists", true)?,
            python_tags: table_strings(table, "python_tags")?,
            abi_tags: table_strings(table, "abi_tags")?,
            platform_tags: table_strings(table, "platform_tags")?,
            max_file_size_bytes: table_u64(table, "max_file_size_bytes")?,
            metadata_only: table_bool(table, "metadata_only", false)?,
        })
    }
}

#[derive(Debug)]
struct PrefetchOptions {
    packages: Vec<String>,
    requirements: Vec<PathBuf>,
    mode: Option<PrefetchMode>,
    metadata_only: bool,
    no_wheels: bool,
    no_sdists: bool,
    python_tags: Vec<String>,
    abi_tags: Vec<String>,
    platform_tags: Vec<String>,
    max_file_size_bytes: Option<u64>,
}

impl PrefetchOptions {
    fn from_table(table: &toml::Table) -> Result<Self, String> {
        Ok(Self {
            packages: table_strings(table, "packages")?,
            requirements: table_strings(table, "requirements")?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            mode: table.get("mode").and_then(toml::Value::as_str).map(mode).transpose()?,
            metadata_only: table_bool(table, "metadata_only", false)?,
            no_wheels: table_bool(table, "no_wheels", false)?,
            no_sdists: table_bool(table, "no_sdists", false)?,
            python_tags: table_strings(table, "python_tags")?,
            abi_tags: table_strings(table, "abi_tags")?,
            platform_tags: table_strings(table, "platform_tags")?,
            max_file_size_bytes: table_u64(table, "max_file_size_bytes")?,
        })
    }
}

fn mode(value: &str) -> Result<PrefetchMode, String> {
    match value {
        "all" => Ok(PrefetchMode::All),
        "selected" => Ok(PrefetchMode::Selected),
        "metadataonly" | "metadata-only" => Ok(PrefetchMode::MetadataOnly),
        _ => Err(format!("unknown mirror mode {value:?}")),
    }
}

fn table_strings(table: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(format!("{key} must be an array"));
    };
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.as_str() else {
            return Err(format!("{key} entries must be strings"));
        };
        strings.push(value.to_owned());
    }
    Ok(strings)
}

fn table_bool(table: &toml::Table, key: &str, default: bool) -> Result<bool, String> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    let Some(value) = value.as_bool() else {
        return Err(format!("{key} must be a boolean"));
    };
    Ok(value)
}

fn table_u64(table: &toml::Table, key: &str) -> Result<Option<u64>, String> {
    table.get(key).map_or(Ok(None), |value| {
        match value {
            toml::Value::Integer(value) => u64::try_from(*value).ok(),
            toml::Value::String(value) => value.parse().ok(),
            _ => None,
        }
        .map(Some)
        .ok_or_else(|| format!("{key} must be an integer"))
    })
}

#[cfg(test)]
#[path = "../../tests/unit/mirror/config_contract_tests.rs"]
mod config_contract_tests;

#[cfg(test)]
#[path = "../../tests/unit/mirror/ecosystem_config_tests.rs"]
mod ecosystem_config_tests;

#[cfg(test)]
#[path = "../../tests/unit/mirror/support.rs"]
mod test_support;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionSource {
    Upstream,
    Cache,
}

#[derive(Default)]
struct SyncSummary {
    projects: u64,
    downloaded: u64,
    bytes: u64,
    skipped: u64,
    failures: u64,
}

struct Selection {
    projects: Vec<String>,
    rules: BTreeMap<String, ProjectRule>,
    filters: ArtifactFilters,
}

#[derive(Default)]
struct ProjectRule {
    specs: Vec<Option<VersionSpecifiers>>,
}

impl ProjectRule {
    fn allows(&self, version: &Version) -> bool {
        if self.specs.is_empty() {
            return true;
        }
        for spec in &self.specs {
            match spec {
                None => return true,
                Some(spec) if spec.contains(version) => return true,
                Some(_) => {}
            }
        }
        false
    }
}

#[derive(Clone, Copy)]
struct BlobCheck<'a> {
    kind: &'a str,
    filename: &'a str,
    digest_hex: &'a str,
    url: &'a str,
}

#[derive(Clone, Copy)]
struct Row<'a> {
    kind: &'a str,
    index: &'a str,
    project: &'a str,
    filename: &'a str,
    digest: &'a str,
    url: &'a str,
    bytes: Option<u64>,
    status: &'a str,
    reason: &'a str,
}

impl<'a> Row<'a> {
    const fn page(index: &'a str, project: &'a str, status: &'a str, reason: &'a str) -> Self {
        Self {
            kind: "page",
            index,
            project,
            filename: "",
            digest: "",
            url: "",
            bytes: None,
            status,
            reason,
        }
    }

    fn metadata(
        index: &'a str,
        project: &'a str,
        filename: &'a str,
        metadata: &'a PrefetchMetadata,
        bytes: Option<u64>,
        status: &'a str,
        reason: &'a str,
    ) -> Self {
        Self {
            kind: "metadata",
            index,
            project,
            filename,
            digest: &metadata.digest,
            url: &metadata.url,
            bytes,
            status,
            reason,
        }
    }

    const fn check(
        index: &'a str,
        project: &'a str,
        check: BlobCheck<'a>,
        digest: &'a str,
        status: &'a str,
        reason: &'a str,
    ) -> Self {
        Self {
            kind: check.kind,
            index,
            project,
            filename: check.filename,
            digest,
            url: check.url,
            bytes: None,
            status,
            reason,
        }
    }
}

struct ProjectSelector {
    project: String,
    spec: Option<VersionSpecifiers>,
}

struct ArtifactFilters {
    include_wheels: bool,
    include_sdists: bool,
    python_tags: BTreeSet<String>,
    abi_tags: BTreeSet<String>,
    platform_tags: BTreeSet<String>,
    max_file_size_bytes: Option<u64>,
    metadata_only: bool,
}

impl From<PrefetchConfig> for ArtifactFilters {
    fn from(config: PrefetchConfig) -> Self {
        Self {
            include_wheels: config.include_wheels,
            include_sdists: config.include_sdists,
            python_tags: config.python_tags.into_iter().collect(),
            abi_tags: config.abi_tags.into_iter().collect(),
            platform_tags: config.platform_tags.into_iter().collect(),
            max_file_size_bytes: config.max_file_size_bytes,
            metadata_only: config.metadata_only,
        }
    }
}

enum FileCandidate {
    Include(PrefetchFile),
    Skip(PrefetchFile, &'static str),
}

struct PrefetchFile {
    filename: String,
    digest: String,
    url: String,
    size: Option<u64>,
    metadata: Option<PrefetchMetadata>,
    source: Option<DistributionFilename>,
}

struct PrefetchMetadata {
    url: String,
    digest: String,
}

struct Target {
    index: String,
    route: String,
    position: usize,
    cached: String,
    client: UpstreamClient,
    offline: bool,
    prefetch: PrefetchConfig,
}

enum SyncOutcome {
    Cached(u64),
    Downloaded(u64),
}
