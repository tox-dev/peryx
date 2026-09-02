//! Stores `PyPI` records in neutral driver key-value namespaces.

mod attestations;
mod files;
mod index;
mod journal;
mod overrides;
mod projects;
mod record;
mod summary;
mod uploads;

pub use files::{
    FilePublication, FileSource, MetadataClaim, PypiArtifactOrigin, get_file_publication, get_file_url,
    get_metadata_digest, get_metadata_digests, get_provenance, put_file_url, put_metadata, put_provenance,
    scan_file_publications, scan_file_urls, scan_metadata_records, scan_provenance_records,
};
pub use index::{
    CachedPageWrite, PublishedFileWrite, abort_project_generation, active_project_generation, begin_project_generation,
    get_index, get_project_status, list_index_pages, list_project_files, project_meta_state,
    publish_project_generation, put_cached_page, put_index, put_project_files, recover_project_generations,
    refresh_project_generation, scan_index_pages, scan_index_records, touch_index_freshness,
};
pub(crate) use journal::{ChangelogReadError, read_changelog_page};
pub use journal::{JournalEntry, JournalSnapshot, read_journal_entries};
pub use overrides::{FileOverride, OverrideMutation};
pub use peryx_driver::serving::{IndexSummary, RecentWrite};
pub use projects::{
    CatalogGeneration, CatalogState, ProjectCachePurgeCounts, abort_catalog_generation, begin_catalog_generation,
    catalog_state, count_project_cache_purge, delete_project_cache, get_project, list_catalog_projects, list_projects,
    publish_catalog_generation, put_catalog_projects, put_project, recover_catalog_generations,
    refresh_catalog_generation, scan_project_records,
};
pub use record::{
    AttestationAvailability, CachedIndex, CachedIndexPage, CachedIndexSummary, FreshnessOverlay, ProjectGeneration,
    ProjectMetaState, ProjectStatusRecord, UpstreamAttestation,
};
pub use summary::{
    AuditedIndex, SummaryDefect, SummaryRowCounts, audit_summary_rows, repair_summary_rows, summarize_indexes,
    summary_row_counts,
};
pub(crate) use summary::{
    put_cached_project_row, put_project_row, put_upload_row, remove_cached_project_row, remove_upload_row,
};
pub(crate) use uploads::publish_file_in_txn;
pub(crate) use uploads::publish_file_with_commit_if;
pub(crate) use uploads::scan_upload_policy_snapshot;
pub use uploads::{
    Guard, MetadataSibling, PromotedRelease, ProvenanceSibling, PublishedFile, PublishedState, UploadMutation,
    delete_upload, get_upload, list_overrides, list_upload_entries, mutate_uploads, promote_files_checked,
    publish_file_if, put_upload, scan_override_records, scan_upload_records, set_override,
};
pub(crate) use uploads::{UploadMutationPlan, mutate_uploads_and_overrides};

/// The former `index_document` table: cached simple-index pages, keyed by the caller's route key.
const INDEX_PREFIX: &str = "pypi\u{0}i\u{0}";
/// The freshness overlay a `304 Not Modified` writes: the fetch time and lifetime for a page whose
/// body did not change, keyed by the same route key so a revalidation rewrites a header, not a body.
const FRESHNESS_PREFIX: &str = "pypi\u{0}h\u{0}";
/// The former `artifact_source` table: upstream source URLs, keyed by artifact digest.
const FILE_PREFIX: &str = "pypi\u{0}f\u{0}";
/// Metadata peryx derived from an artifact's own verified bytes - extracted from the archive, or
/// uploaded alongside it - keyed by artifact digest. The bytes are a function of the digest, so this
/// record is immutable and every publication of that digest shares it.
const METADATA_PREFIX: &str = "pypi\u{0}d\u{0}";
/// One cached index's publication of one file, keyed by `{index}/{normalized}/{sha256}/{filename}`,
/// holding the PEP 658 sidecar that publication advertised (empty when it advertised none). A claim
/// is scoped this way because it is the publisher's word about its own URL, not a property of the
/// artifact bytes: two indexes serving the same wheel must not inherit each other's sidecar.
const PUBLICATION_PREFIX: &str = "pypi\u{0}n\u{0}";
/// One hosted publication's PEP 740 provenance bundle, keyed by
/// `{index}/{normalized}/{sha256}/{filename}`, holding the bundle blob's digest and byte length.
/// A bundle is what one publisher attested about its own release, not a property of the artifact
/// bytes, so two hosted indexes carrying the same wheel keep separate bundles; the blob store
/// deduplicates the bytes underneath when they happen to be identical.
const PROVENANCE_PREFIX: &str = "pypi\u{0}a\u{0}";
/// Mutable provenance objects advertised by upstream indexes, keyed by source, artifact digest,
/// filename, and owning project.
const UPSTREAM_ATTESTATION_PREFIX: &str = "pypi\u{0}t\u{0}";
/// Provenance registrations collected while a project generation is staging. Publication replaces
/// the project's live registrations atomically; abort recovery discards these rows with the files.
const PROJECT_ATTESTATION_PREFIX: &str = "pypi\u{0}v\u{0}";
/// The former `projects` table: observed display names, keyed by `{index}/{normalized}`.
const PROJECTS_PREFIX: &str = "pypi\u{0}p\u{0}";
const CATALOG_PREFIX: &str = "pypi\u{0}c\u{0}";
const CATALOG_GENERATION_PREFIX: &str = "pypi\u{0}g\u{0}";
/// Per-project remote file-metadata publication state, keyed by `{index}/{normalized}`.
const PROJECT_META_PREFIX: &str = "pypi\u{0}m\u{0}";
/// One remote file's parsed metadata, keyed by `{index}/{normalized}/{generation}/{filename}` so a
/// generation's rows sort together and delete by prefix.
const PROJECT_FILE_PREFIX: &str = "pypi\u{0}r\u{0}";
/// The former `project_status` table: explicit status markers, keyed by `{index}/{normalized}`.
const PROJECT_STATUS_PREFIX: &str = "pypi\u{0}s\u{0}";
/// The former `uploads` table: hosted file records, keyed by `{index}/{normalized}/{filename}`.
const UPLOAD_PREFIX: &str = "pypi\u{0}u\u{0}";
/// The former `overrides` table: yanked/hidden markers, keyed by `{index}/{normalized}/{filename}`.
const OVERRIDE_PREFIX: &str = "pypi\u{0}o\u{0}";
/// One index's project and upload row counts, keyed by index name. Maintained by every write that adds
/// or removes one of those rows, so a status request reads a count instead of walking a history.
const COUNT_PREFIX: &str = "pypi\u{0}k\u{0}";
/// One published upload's place in its index's newest-first order, keyed by index name, an inverted
/// upload time, and the names that make the position unique. Holds what the summary reports about that
/// upload, so reading the newest few decodes only the newest few.
const RECENT_PREFIX: &str = "pypi\u{0}w\u{0}";

fn index_key(key: &str) -> String {
    format!("{INDEX_PREFIX}{key}")
}

fn freshness_key(key: &str) -> String {
    format!("{FRESHNESS_PREFIX}{key}")
}

fn file_key(sha256: &str) -> String {
    format!("{FILE_PREFIX}{sha256}")
}

fn metadata_key(sha256: &str) -> String {
    format!("{METADATA_PREFIX}{sha256}")
}

fn publication_prefix(index: &str, normalized: &str) -> String {
    format!("{PUBLICATION_PREFIX}{index}/{normalized}/")
}

fn publication_key(index: &str, normalized: &str, sha256: &str, filename: &str) -> String {
    format!("{}{sha256}/{filename}", publication_prefix(index, normalized))
}

fn provenance_prefix(index: &str, normalized: &str) -> String {
    format!("{PROVENANCE_PREFIX}{index}/{normalized}/")
}

fn provenance_key(index: &str, normalized: &str, sha256: &str, filename: &str) -> String {
    format!("{}{sha256}/{filename}", provenance_prefix(index, normalized))
}

fn upstream_attestation_prefix(index: &str, sha256: &str, filename: &str) -> String {
    format!("{UPSTREAM_ATTESTATION_PREFIX}{index}/{sha256}/{filename}/")
}

fn upstream_attestation_key(index: &str, sha256: &str, filename: &str, project: &str) -> String {
    format!("{}{project}", upstream_attestation_prefix(index, sha256, filename))
}

fn project_attestation_prefix(index: &str, normalized: &str) -> String {
    format!("{PROJECT_ATTESTATION_PREFIX}{index}/{normalized}/")
}

fn project_attestation_live_prefix(index: &str, normalized: &str) -> String {
    format!("{}active/", project_attestation_prefix(index, normalized))
}

fn project_attestation_live_key(index: &str, normalized: &str, sha256: &str, filename: &str) -> String {
    format!(
        "{}{sha256}/{filename}",
        project_attestation_live_prefix(index, normalized)
    )
}

fn project_generation_attestation_prefix(index: &str, normalized: &str, generation: u64) -> String {
    format!("{}{generation:020}/", project_attestation_prefix(index, normalized))
}

fn project_generation_attestation_key(
    index: &str,
    normalized: &str,
    generation: u64,
    sha256: &str,
    filename: &str,
) -> String {
    format!(
        "{}{sha256}/{filename}",
        project_generation_attestation_prefix(index, normalized, generation)
    )
}

fn project_key(index: &str, normalized: &str) -> String {
    format!("{PROJECTS_PREFIX}{index}/{normalized}")
}

/// The `(index, normalized project)` a replicated authoritative key names, or `None` when the key
/// belongs to no project. A replica maps the keys it just applied to the projects whose derived views
/// need rebuilding through this.
///
/// Publishing a file, and retiring or restoring a project, write or remove that project's marker under
/// the projects prefix. A per-file yank, unyank, or delete goes through `mutate_uploads`, which rewrites
/// the file's own record under the upload prefix; a yank of a file served from a read-only layer records
/// an override. Both name a project too, so a replicated per-file change retires the affected project's
/// page and search document rather than leaving them stale. A mirror caches a project's upstream page
/// under the index prefix and stamps its revalidation under the freshness prefix, both keyed by the same
/// project. File, metadata, and journal keys carry no project.
///
/// Configuration validation makes index and project names single path segments. A project-marker,
/// cached-page or freshness key is `{index}/{normalized}`; an upload or override key is
/// `{index}/{normalized}/{filename}`.
pub(crate) fn project_of_key(key: &str) -> Option<(&str, &str)> {
    for prefix in [PROJECTS_PREFIX, INDEX_PREFIX, FRESHNESS_PREFIX] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return split_index_project(rest);
        }
    }
    for prefix in [UPLOAD_PREFIX, OVERRIDE_PREFIX] {
        if let Some(rest) = key.strip_prefix(prefix) {
            let (head, _filename) = rest.rsplit_once('/')?;
            return split_index_project(head);
        }
    }
    None
}

/// The artifact a replicated PEP 658 metadata pointer names, or `None` for any other key.
///
/// The row is keyed by artifact digest alone, so it names no project on its own; a replica pairs it
/// with the artifacts of the projects it rebuilt to tell whether one of those already covers it.
pub(crate) fn metadata_artifact_of_key(key: &str) -> Option<&str> {
    key.strip_prefix(METADATA_PREFIX).filter(|sha256| !sha256.is_empty())
}

/// The replicated namespaces no derived view reads.
///
/// A file-url row resolves a byte route from an artifact digest, and a count row and an order row
/// answer an index's summary. Neither the search document a project derives nor the representations a
/// replica caches for it reads one, so a publish that maintains them alongside its project's own rows
/// leaves both current.
///
/// A namespace this omits is one a replica cannot vouch for, and it re-derives the whole index rather
/// than guess. That is the safe default for a row kind added later: slow until it is classified here,
/// never stale.
const VIEW_NEUTRAL_PREFIXES: &[&str] = &[FILE_PREFIX, COUNT_PREFIX, RECENT_PREFIX];

/// Whether `key` belongs to a namespace no derived view reads.
pub(crate) fn derives_no_view(key: &str) -> bool {
    VIEW_NEUTRAL_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

fn split_index_project(rest: &str) -> Option<(&str, &str)> {
    let (index, normalized) = rest.rsplit_once('/')?;
    (!index.is_empty() && !normalized.is_empty()).then_some((index, normalized))
}

fn project_status_key(index: &str, normalized: &str) -> String {
    format!("{PROJECT_STATUS_PREFIX}{index}/{normalized}")
}

fn project_meta_key(index: &str, normalized: &str) -> String {
    format!("{PROJECT_META_PREFIX}{index}/{normalized}")
}

fn project_generation_prefix(index: &str, normalized: &str, generation: u64) -> String {
    format!("{PROJECT_FILE_PREFIX}{index}/{normalized}/{generation:020}/")
}

fn project_file_key(index: &str, normalized: &str, generation: u64, filename: &str) -> String {
    format!("{}{filename}", project_generation_prefix(index, normalized, generation))
}

pub(crate) fn upload_key(index: &str, normalized: &str, filename: &str) -> String {
    format!("{UPLOAD_PREFIX}{index}/{normalized}/{filename}")
}

fn override_key(index: &str, normalized: &str, filename: &str) -> String {
    format!("{OVERRIDE_PREFIX}{index}/{normalized}/{filename}")
}

fn file_source_value(url: &str, source: &str, size: Option<u64>, upstream: Option<&str>) -> String {
    upstream.map_or_else(
        || size.map_or_else(|| format!("{url}\n{source}"), |size| format!("{url}\n{source}\n{size}")),
        |upstream| {
            format!(
                "{url}\n{source}\n{}\n{upstream}",
                size.map_or_else(String::new, |size| size.to_string())
            )
        },
    )
}

/// A publication's sidecar claim, or the empty record standing for "this publication advertised no
/// sidecar". The empty record is what stops a virtual index from walking past the winning layer into
/// a shadowed layer's claim.
fn publication_value(metadata: Option<&(String, String)>, source: &str, upstream: Option<&str>) -> String {
    metadata.map_or_else(String::new, |(url, metadata_sha256)| {
        format!("{url}\n{metadata_sha256}\n{source}\n{}", upstream.unwrap_or_default())
    })
}

fn provenance_value(provenance_sha256: &str, size: u64) -> String {
    format!("{provenance_sha256}\n{size}")
}

/// Split a provenance record into `(bundle sha256, byte length)`, rejecting one missing either field.
fn split_provenance_value<'a>(key: &str, value: &'a str) -> Result<(&'a str, u64), peryx_storage::meta::MetaError> {
    let (sha256, size) = value
        .split_once('\n')
        .ok_or_else(|| peryx_storage::meta::MetaError::DriverRecordMissing {
            key: key.to_owned(),
            field: "size",
        })?;
    let size = size
        .parse()
        .map_err(|source| peryx_storage::meta::MetaError::DriverRecordInteger {
            key: key.to_owned(),
            field: "size",
            source,
        })?;
    Ok((sha256, size))
}

/// Decode a stored record that the namespace defines as UTF-8 text.
///
/// A present row that is not text is damage, not absence: mapping it to `None` would serve a stored
/// artifact as missing and let a purge or repair report success over a row it never read.
fn record_str(key: &str, raw: Vec<u8>) -> Result<String, peryx_storage::meta::MetaError> {
    String::from_utf8(raw).map_err(|source| peryx_storage::meta::MetaError::DriverRecordUtf8 {
        key: key.to_owned(),
        source,
    })
}

/// A `PyPI` driver namespace whose records are UTF-8 text, named so a repair pass can walk each one
/// in turn instead of reaching for a separate entry point per key prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PypiRecords {
    FileUrl,
    Metadata,
    Publication,
    Project,
    Override,
    Provenance,
}

impl PypiRecords {
    const fn prefix(self) -> &'static str {
        match self {
            Self::FileUrl => FILE_PREFIX,
            Self::Metadata => METADATA_PREFIX,
            Self::Publication => PUBLICATION_PREFIX,
            Self::Project => PROJECTS_PREFIX,
            Self::Override => OVERRIDE_PREFIX,
            Self::Provenance => PROVENANCE_PREFIX,
        }
    }

    /// The name a report prints for this namespace.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FileUrl => "file-url",
            Self::Metadata => "pep658",
            Self::Publication => "publication",
            Self::Project => "project",
            Self::Override => "override",
            Self::Provenance => "provenance",
        }
    }
}

/// Visit every record under `prefix`, keyed relative to it, stopping at the first one that is not
/// UTF-8.
fn scan_utf8_records<E>(
    meta: &peryx_storage::meta::MetaStore,
    prefix: &str,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
    meta.scan_driver_prefix(prefix, |key, raw| {
        let value = record_str(key, raw.to_vec())?;
        visit(&key[prefix.len()..], &value).map_err(peryx_storage::meta::MetaScanError::Visit)
    })
}

/// Visit every readable record in `namespace`, collecting the rows that do not decode rather than
/// stopping at the first.
///
/// This is the scan a repair pass wants and no other caller should: enumerating damage is the whole
/// point of `fsck`, whereas a read that continues past a row it could not decode is how a corrupt
/// project comes back as an empty one. The returned [`peryx_storage::meta::RepairScan`] names every
/// skipped key, so a report built from it cannot claim the namespace is clean.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_records_for_repair<E>(
    meta: &peryx_storage::meta::MetaStore,
    namespace: PypiRecords,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<peryx_storage::meta::RepairScan, peryx_storage::meta::MetaScanError<E>> {
    let prefix = namespace.prefix();
    let mut scan = peryx_storage::meta::RepairScan::default();
    meta.scan_driver_prefix(prefix, |key, raw| match record_str(key, raw.to_vec()) {
        Ok(value) => visit(&key[prefix.len()..], &value).map_err(peryx_storage::meta::MetaScanError::Visit),
        Err(source) => {
            scan.skip(&key[prefix.len()..], source);
            Ok(())
        }
    })?;
    Ok(scan)
}

/// The `PyPI` metadata surface as inherent-style methods on the neutral [`MetaStore`].
///
/// Every method delegates to the matching free function in this module. It exists so a call site
/// can keep writing `meta.put_index(..)` after the old `PyPI`-specific inherent methods leave
/// `peryx-storage`: bring the trait into scope with `use crate::store::PypiStore as _;` and the
/// receiver syntax resolves here instead.
///
/// [`MetaStore`]: peryx_storage::meta::MetaStore
#[cfg(feature = "serving")]
pub trait PypiStore {
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_index(&self, key: &str, record: &CachedIndex) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the transaction fails.
    fn retire_cached_project(
        &self,
        key: &str,
        index: &str,
        project: &str,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Advance a cached page's freshness after a `304 Not Modified`, writing only the small overlay
    /// row and leaving the page body untouched.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn touch_index_freshness(
        &self,
        key: &str,
        fetched_at_unix: i64,
        fresh_secs: Option<i64>,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails or the stored bytes cannot be decoded.
    fn get_index(&self, key: &str) -> Result<Option<CachedIndex>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails or a stored record cannot be decoded.
    fn list_index_pages(&self) -> Result<Vec<(String, i64, Option<i64>)>, peryx_storage::meta::MetaError>;

    /// Visit cached simple-index page summaries without collecting them.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails, a record cannot be decoded, or the visitor fails.
    fn scan_index_pages<E>(
        &self,
        visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_index_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a store error if the read fails or the stored record cannot be decoded.
    fn get_project_status(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Option<ProjectStatusRecord>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn put_cached_page(&self, write: CachedPageWrite<'_>) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn put_file_url(&self, sha256: &str, url: &str, source: &str) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_file_url(&self, sha256: &str) -> Result<Option<FileSource>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_file_urls<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Record the metadata derived from an artifact's own bytes.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_metadata(&self, artifact_sha256: &str, metadata_sha256: &str) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_metadata_digest(&self, artifact_sha256: &str) -> Result<Option<String>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<Option<Vec<u8>>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails or the stored record cannot be decoded.
    fn get_file_publication(
        &self,
        index: &str,
        normalized: &str,
        sha256: &str,
        filename: &str,
    ) -> Result<Option<FilePublication>, peryx_storage::meta::MetaError>;

    /// Visit raw publication records, keyed by `{index}/{normalized}/{sha256}/{filename}`.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_file_publications<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_metadata_digests<'a>(
        &self,
        artifact_sha256s: impl IntoIterator<Item = &'a str>,
    ) -> Result<std::collections::BTreeMap<String, String>, peryx_storage::meta::MetaError>;

    /// Visit raw PEP 658 metadata records, keyed by wheel digest.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_metadata_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Record one hosted publication's PEP 740 provenance bundle.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_provenance(
        &self,
        index: &str,
        normalized: &str,
        artifact_sha256: &str,
        filename: &str,
        bundle: ProvenanceSibling<'_>,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_provenance(
        &self,
        index: &str,
        normalized: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Option<(String, u64)>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store or decode error when the record cannot be read.
    fn list_upstream_attestations(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Vec<UpstreamAttestation>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store or decode error when the record cannot be read.
    fn get_upstream_attestation(
        &self,
        index: &str,
        project: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Option<UpstreamAttestation>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store or encode error when the record cannot be written.
    fn put_upstream_attestation(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
        record: &UpstreamAttestation,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Replace upstream provenance state only when it has not changed since it was read.
    ///
    /// # Errors
    /// Returns a store, decode, or encode error when the record cannot be compared or written.
    fn compare_exchange_upstream_attestation(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
        expected: &UpstreamAttestation,
        replacement: &UpstreamAttestation,
    ) -> Result<bool, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_provenance_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn put_project(&self, index: &str, normalized: &str, display: &str) -> Result<(), peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn get_project(&self, index: &str, normalized: &str) -> Result<Option<String>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn list_projects(&self, index: &str) -> Result<Vec<String>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_project_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn count_project_cache_purge(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn delete_project_cache(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError>;

    /// Publish a file - its sibling, record, project, and journal entry - only if `guard` accepts the
    /// publication as the store holds it, checked inside the same write transaction. Returns whether
    /// it wrote.
    ///
    /// # Errors
    /// Returns the guard's error, or a store error mapped into it, if the transaction fails.
    fn publish_file_if<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        file: &PublishedFile,
        guard: impl FnOnce(PublishedState<'_>) -> Result<Guard, E>,
    ) -> Result<bool, E>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn put_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
        record: &[u8],
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Promote a release onto `index` - its records, project, and journal entry - admitting each
    /// `(filename, token, bytes)` only when `guard` accepts the target's current record inside the
    /// write transaction. Tokens in `blob_sizes` are recorded as blob references. Returns how many
    /// files were written.
    ///
    /// # Errors
    /// Returns the guard's error, or a store error mapped into it, if the transaction fails.
    fn promote_files_checked<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        release: &PromotedRelease<'_>,
        guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<usize, E>;

    /// # Errors
    /// Returns the closure's error, or a store error mapped into it, if the transaction fails.
    fn mutate_uploads<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        action: &str,
        submitted_at_unix: i64,
        mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
    ) -> Result<usize, E>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn list_upload_entries(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the write fails.
    fn delete_upload(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        filename: &str,
        submitted_at_unix: i64,
    ) -> Result<bool, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_upload_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Apply one field change to the override record of a file served from a read-only layer.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn set_override(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        filename: &str,
        mutation: OverrideMutation<'_>,
        submitted_at_unix: i64,
    ) -> Result<bool, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a store error if the read fails or a stored record does not decode.
    fn list_overrides(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<std::collections::BTreeMap<String, FileOverride>, peryx_storage::meta::MetaError>;

    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_override_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Walk one namespace, collecting the rows that do not decode instead of stopping at the first.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_records_for_repair<E>(
        &self,
        namespace: PypiRecords,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<peryx_storage::meta::RepairScan, peryx_storage::meta::MetaScanError<E>>;

    /// # Errors
    /// Returns a store error if the read fails.
    fn summarize_indexes(
        &self,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, peryx_storage::meta::MetaError>;
}

#[cfg(feature = "serving")]
impl PypiStore for peryx_storage::meta::MetaStore {
    fn put_index(&self, key: &str, record: &CachedIndex) -> Result<(), peryx_storage::meta::MetaError> {
        index::put_index(self, key, record)
    }

    fn retire_cached_project(
        &self,
        key: &str,
        index: &str,
        project: &str,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        index::retire_cached_project(self, key, index, project)
    }

    fn touch_index_freshness(
        &self,
        key: &str,
        fetched_at_unix: i64,
        fresh_secs: Option<i64>,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        index::touch_index_freshness(self, key, fetched_at_unix, fresh_secs)
    }

    fn get_index(&self, key: &str) -> Result<Option<CachedIndex>, peryx_storage::meta::MetaError> {
        index::get_index(self, key)
    }

    fn list_index_pages(&self) -> Result<Vec<(String, i64, Option<i64>)>, peryx_storage::meta::MetaError> {
        index::list_index_pages(self)
    }

    fn scan_index_pages<E>(
        &self,
        visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        index::scan_index_pages(self, visit)
    }

    fn scan_index_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        index::scan_index_records(self, visit)
    }

    fn get_project_status(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Option<ProjectStatusRecord>, peryx_storage::meta::MetaError> {
        index::get_project_status(self, index, normalized)
    }

    fn put_cached_page(&self, write: CachedPageWrite<'_>) -> Result<(), peryx_storage::meta::MetaError> {
        index::put_cached_page(self, write)
    }

    fn put_file_url(&self, sha256: &str, url: &str, source: &str) -> Result<(), peryx_storage::meta::MetaError> {
        files::put_file_url(self, sha256, url, source)
    }

    fn get_file_url(&self, sha256: &str) -> Result<Option<FileSource>, peryx_storage::meta::MetaError> {
        files::get_file_url(self, sha256)
    }

    fn scan_file_urls<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        files::scan_file_urls(self, visit)
    }

    fn put_metadata(&self, artifact_sha256: &str, metadata_sha256: &str) -> Result<(), peryx_storage::meta::MetaError> {
        files::put_metadata(self, artifact_sha256, metadata_sha256)
    }

    fn get_metadata_digest(&self, artifact_sha256: &str) -> Result<Option<String>, peryx_storage::meta::MetaError> {
        files::get_metadata_digest(self, artifact_sha256)
    }

    fn get_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<Option<Vec<u8>>, peryx_storage::meta::MetaError> {
        uploads::get_upload(self, index, normalized, filename)
    }

    fn get_file_publication(
        &self,
        index: &str,
        normalized: &str,
        sha256: &str,
        filename: &str,
    ) -> Result<Option<FilePublication>, peryx_storage::meta::MetaError> {
        files::get_file_publication(self, index, normalized, sha256, filename)
    }

    fn scan_file_publications<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        files::scan_file_publications(self, visit)
    }

    fn get_metadata_digests<'a>(
        &self,
        artifact_sha256s: impl IntoIterator<Item = &'a str>,
    ) -> Result<std::collections::BTreeMap<String, String>, peryx_storage::meta::MetaError> {
        files::get_metadata_digests(self, artifact_sha256s)
    }

    fn scan_metadata_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        files::scan_metadata_records(self, visit)
    }

    fn put_provenance(
        &self,
        index: &str,
        normalized: &str,
        artifact_sha256: &str,
        filename: &str,
        bundle: ProvenanceSibling<'_>,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        files::put_provenance(self, index, normalized, artifact_sha256, filename, bundle)
    }

    fn get_provenance(
        &self,
        index: &str,
        normalized: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Option<(String, u64)>, peryx_storage::meta::MetaError> {
        files::get_provenance(self, index, normalized, artifact_sha256, filename)
    }

    fn list_upstream_attestations(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Vec<UpstreamAttestation>, peryx_storage::meta::MetaError> {
        attestations::list_upstream_attestations(self, index, artifact_sha256, filename)
    }

    fn get_upstream_attestation(
        &self,
        index: &str,
        project: &str,
        artifact_sha256: &str,
        filename: &str,
    ) -> Result<Option<UpstreamAttestation>, peryx_storage::meta::MetaError> {
        self.get_driver_value(&upstream_attestation_key(index, artifact_sha256, filename, project))
            .and_then(|raw| {
                raw.map(|raw| serde_json::from_slice(&raw).map_err(peryx_storage::meta::MetaError::from))
                    .transpose()
            })
    }

    fn put_upstream_attestation(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
        record: &UpstreamAttestation,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        let key = upstream_attestation_key(index, artifact_sha256, filename, &record.project);
        let owner_key = project_attestation_live_key(index, &record.project, artifact_sha256, filename);
        serde_json::to_vec(record)
            .map_err(peryx_storage::meta::MetaError::from)
            .and_then(|encoded| {
                self.commit_driver_cache_txn(|txn| {
                    txn.put_local(&key, &encoded).and_then(|()| {
                        serde_json::to_vec(&key)
                            .map_err(peryx_storage::meta::MetaError::from)
                            .and_then(|encoded_key| txn.put_local(&owner_key, &encoded_key))
                    })
                })
            })
    }

    fn compare_exchange_upstream_attestation(
        &self,
        index: &str,
        artifact_sha256: &str,
        filename: &str,
        expected: &UpstreamAttestation,
        replacement: &UpstreamAttestation,
    ) -> Result<bool, peryx_storage::meta::MetaError> {
        if (&expected.project, &expected.source, &expected.upstream, &expected.url)
            != (
                &replacement.project,
                &replacement.source,
                &replacement.upstream,
                &replacement.url,
            )
        {
            return Err(peryx_storage::meta::MetaError::DriverPrecondition(
                "attestation cache replacement changed its source identity".to_owned(),
            ));
        }
        let key = upstream_attestation_key(index, artifact_sha256, filename, &expected.project);
        serde_json::to_vec(replacement)
            .map_err(peryx_storage::meta::MetaError::from)
            .and_then(|replacement| {
                self.commit_driver_cache_txn(|txn| {
                    txn.get(&key)
                        .and_then(|raw| {
                            raw.map(|raw| serde_json::from_slice(&raw).map_err(peryx_storage::meta::MetaError::from))
                                .transpose()
                        })
                        .and_then(|current| {
                            if current.as_ref() != Some(expected) {
                                return Ok(false);
                            }
                            txn.put_local(&key, &replacement).map(|()| true)
                        })
                })
            })
    }

    fn scan_provenance_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        files::scan_provenance_records(self, visit)
    }

    fn put_project(&self, index: &str, normalized: &str, display: &str) -> Result<(), peryx_storage::meta::MetaError> {
        projects::put_project(self, index, normalized, display)
    }

    fn get_project(&self, index: &str, normalized: &str) -> Result<Option<String>, peryx_storage::meta::MetaError> {
        projects::get_project(self, index, normalized)
    }

    fn list_projects(&self, index: &str) -> Result<Vec<String>, peryx_storage::meta::MetaError> {
        projects::list_projects(self, index)
    }

    fn scan_project_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        projects::scan_project_records(self, visit)
    }

    fn count_project_cache_purge(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError> {
        projects::count_project_cache_purge(self, index, normalized, file_digests, metadata_digests)
    }

    fn delete_project_cache(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError> {
        projects::delete_project_cache(self, index, normalized, file_digests, metadata_digests)
    }

    fn publish_file_if<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        file: &PublishedFile,
        guard: impl FnOnce(PublishedState<'_>) -> Result<Guard, E>,
    ) -> Result<bool, E> {
        uploads::publish_file_if(self, outbox, file, guard)
    }

    fn put_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
        record: &[u8],
    ) -> Result<(), peryx_storage::meta::MetaError> {
        uploads::put_upload(self, index, normalized, filename, record)
    }

    fn promote_files_checked<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        release: &PromotedRelease<'_>,
        guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<usize, E> {
        uploads::promote_files_checked(self, outbox, release, guard)
    }

    fn mutate_uploads<E: From<peryx_storage::meta::MetaError>>(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        action: &str,
        submitted_at_unix: i64,
        mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
    ) -> Result<usize, E> {
        uploads::mutate_uploads(self, outbox, index, normalized, action, submitted_at_unix, mutate)
    }

    fn list_upload_entries(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, peryx_storage::meta::MetaError> {
        uploads::list_upload_entries(self, index, normalized)
    }

    fn delete_upload(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        filename: &str,
        submitted_at_unix: i64,
    ) -> Result<bool, peryx_storage::meta::MetaError> {
        uploads::delete_upload(self, outbox, index, normalized, filename, submitted_at_unix)
    }

    fn scan_upload_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        uploads::scan_upload_records(self, visit)
    }

    fn set_override(
        &self,
        outbox: bool,
        index: &str,
        normalized: &str,
        filename: &str,
        mutation: OverrideMutation<'_>,
        submitted_at_unix: i64,
    ) -> Result<bool, peryx_storage::meta::MetaError> {
        uploads::set_override(self, outbox, index, normalized, filename, mutation, submitted_at_unix)
    }

    fn list_overrides(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<std::collections::BTreeMap<String, FileOverride>, peryx_storage::meta::MetaError> {
        uploads::list_overrides(self, index, normalized)
    }

    fn scan_override_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        uploads::scan_override_records(self, visit)
    }

    fn scan_records_for_repair<E>(
        &self,
        namespace: PypiRecords,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<peryx_storage::meta::RepairScan, peryx_storage::meta::MetaScanError<E>> {
        scan_records_for_repair(self, namespace, visit)
    }

    fn summarize_indexes(
        &self,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, peryx_storage::meta::MetaError> {
        summary::summarize_indexes(self, index_names, recent_limit)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/store/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/store/corruption/tests.rs"]
mod corruption_tests;
