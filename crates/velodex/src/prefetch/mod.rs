//! Prefetch planning, synchronization, and verification.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use velodex_ecosystem_pypi::{DistributionFilename, Version, VersionSpecifiers};
use velodex_upstream::UpstreamClient;

use crate::config::PrefetchConfig;

mod dispatch;
mod oci;
mod pypi;
mod report;
mod selection;

pub use dispatch::run;

const HEADER: &str = "kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n";

type Output = dyn Write + Send;

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
        self.specs.is_empty()
            || self
                .specs
                .iter()
                .any(|spec| spec.as_ref().is_none_or(|spec| spec.contains(version)))
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
