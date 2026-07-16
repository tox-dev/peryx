//! How the `PyPI` driver lays its metadata into the neutral [`MetaStore`] key-value table.
//!
//! Every record that once lived in a `PyPI`-specific redb table now serializes into the neutral
//! `driver_kv` table under a null-delimited namespace prefix, so the store never grows a table per
//! format and can drop the `PyPI` tables. The value encodings are byte-identical to the old typed
//! tables: the on-disk format and the warm-read cost both depend on it, so nothing here re-serializes
//! a record differently than the table it replaces.
//!
//! [`MetaStore`]: peryx_storage::meta::MetaStore

mod files;
mod index;
mod journal;
mod projects;
mod record;
mod summary;
mod uploads;

pub use files::{
    FileSource, get_file_url, get_metadata, get_metadata_digests, put_file_url, put_metadata, scan_file_urls,
    scan_metadata_records,
};
pub use index::{
    get_index, get_project_status, list_index_pages, put_cached_page, put_index, scan_index_pages, scan_index_records,
    touch_index_freshness,
};
pub use journal::JournalEntry;
pub use peryx_driver::serving::{IndexSummary, RecentUpload};
pub use projects::{
    ProjectCachePurgeCounts, count_project_cache_purge, delete_project_cache, get_project, list_projects, put_project,
    scan_project_records,
};
pub use record::{CachedIndex, CachedIndexPage, CachedIndexSummary, FreshnessOverlay, ProjectStatusRecord};
pub use summary::summarize_indexes;
pub use uploads::{
    Guard, MetadataSibling, PublishedFile, UploadMutation, delete_override, delete_upload, list_overrides,
    list_upload_entries, mutate_uploads, promote_files_checked, publish_file_if, put_override, put_upload,
    scan_override_records, scan_upload_records,
};

/// The former `index_document` table: cached simple-index pages, keyed by the caller's route key.
const INDEX_PREFIX: &str = "pypi\u{0}i\u{0}";
/// The freshness overlay a `304 Not Modified` writes: the fetch time and lifetime for a page whose
/// body did not change, keyed by the same route key so a revalidation rewrites a header, not a body.
const FRESHNESS_PREFIX: &str = "pypi\u{0}h\u{0}";
/// The former `artifact_source` table: upstream source URLs, keyed by artifact digest.
const FILE_PREFIX: &str = "pypi\u{0}f\u{0}";
/// The former `metadata_sidecar` table: PEP 658 siblings, keyed by artifact digest.
const METADATA_PREFIX: &str = "pypi\u{0}d\u{0}";
/// The former `projects` table: observed display names, keyed by `{index}/{normalized}`.
const PROJECTS_PREFIX: &str = "pypi\u{0}p\u{0}";
/// The former `project_status` table: explicit status markers, keyed by `{index}/{normalized}`.
const PROJECT_STATUS_PREFIX: &str = "pypi\u{0}s\u{0}";
/// The former `uploads` table: hosted file records, keyed by `{index}/{normalized}/{filename}`.
const UPLOAD_PREFIX: &str = "pypi\u{0}u\u{0}";
/// The former `overrides` table: yanked/hidden markers, keyed by `{index}/{normalized}/{filename}`.
const OVERRIDE_PREFIX: &str = "pypi\u{0}o\u{0}";

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

fn project_key(index: &str, normalized: &str) -> String {
    format!("{PROJECTS_PREFIX}{index}/{normalized}")
}

fn project_status_key(index: &str, normalized: &str) -> String {
    format!("{PROJECT_STATUS_PREFIX}{index}/{normalized}")
}

fn upload_key(index: &str, normalized: &str, filename: &str) -> String {
    format!("{UPLOAD_PREFIX}{index}/{normalized}/{filename}")
}

fn override_key(index: &str, normalized: &str, filename: &str) -> String {
    format!("{OVERRIDE_PREFIX}{index}/{normalized}/{filename}")
}

/// The `artifact_source` value: URL, source index, optional size, and optional routed upstream.
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

/// The `metadata_sidecar` value: URL, the sibling's own sha256, and the source index, newline-joined.
fn metadata_value(url: &str, metadata_sha256: &str, source: &str) -> String {
    format!("{url}\n{metadata_sha256}\n{source}")
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
    /// Store a cached index record under `key`.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_index(&self, key: &str, record: &CachedIndex) -> Result<(), peryx_storage::meta::MetaError>;

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

    /// Fetch a cached index record.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the stored bytes cannot be decoded.
    fn get_index(&self, key: &str) -> Result<Option<CachedIndex>, peryx_storage::meta::MetaError>;

    /// Every cached page's key, fetch timestamp, and upstream freshness lifetime.
    ///
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

    /// Visit raw cached simple-index records, keyed by route.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_index_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Fetch one project's explicit status marker.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the stored record cannot be decoded.
    fn get_project_status(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Option<ProjectStatusRecord>, peryx_storage::meta::MetaError>;

    /// Store everything a freshly fetched cached page produces in one transaction.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "one transaction needs every namespace's rows together"
    )]
    fn put_cached_page(
        &self,
        key: &str,
        record: &CachedIndex,
        index: &str,
        normalized: &str,
        display: &str,
        source: &str,
        upstream: Option<&str>,
        project_status: Option<&str>,
        project_status_reason: Option<&str>,
        files: &[(String, String, Option<u64>)],
        metadata: &[(String, String, String)],
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Record the upstream URL a blob digest can be fetched from and its source index.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_file_url(&self, sha256: &str, url: &str, source: &str) -> Result<(), peryx_storage::meta::MetaError>;

    /// Look up the source for a blob digest.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn get_file_url(&self, sha256: &str) -> Result<Option<FileSource>, peryx_storage::meta::MetaError>;

    /// Visit raw file URL records, keyed by artifact digest.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_file_urls<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Record the PEP 658 metadata sibling for an artifact.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_metadata(
        &self,
        artifact_sha256: &str,
        url: &str,
        metadata_sha256: &str,
        source: &str,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Look up an artifact's metadata sibling.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn get_metadata(
        &self,
        artifact_sha256: &str,
    ) -> Result<Option<(String, String, String)>, peryx_storage::meta::MetaError>;

    /// Look up metadata sha256 values for many artifact digests.
    ///
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

    /// Record that a project's display name has been observed on `index`.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_project(&self, index: &str, normalized: &str, display: &str) -> Result<(), peryx_storage::meta::MetaError>;

    /// Fetch a project's display name on one index.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn get_project(&self, index: &str, normalized: &str) -> Result<Option<String>, peryx_storage::meta::MetaError>;

    /// List the display names of projects observed on `index`, sorted.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn list_projects(&self, index: &str) -> Result<Vec<String>, peryx_storage::meta::MetaError>;

    /// Visit raw project-display records, keyed by `{index}/{normalized}`.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_project_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Count the rows a project-cache purge would remove.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn count_project_cache_purge(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError>;

    /// Delete cached metadata rows for one project, reporting what was removed.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn delete_project_cache(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, peryx_storage::meta::MetaError>;

    /// Publish a file — its sibling, record, project, and journal entry — only if `guard` accepts the
    /// filename's current record, checked inside the same write transaction. Returns whether it wrote.
    ///
    /// # Errors
    /// Returns the guard's error, or a store error mapped into it, if the transaction fails.
    fn publish_file_if<E: From<peryx_storage::meta::MetaError>>(
        &self,
        file: &PublishedFile,
        guard: impl FnOnce(Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<bool, E>;

    /// Store an uploaded file's serialized record on a private index.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
        record: &[u8],
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Promote a release onto `index` — its records, project, and journal entry — admitting each
    /// `(filename, token, bytes)` only when `guard` accepts the target's current record inside the
    /// write transaction. Tokens in `blob_sizes` are recorded as blob references. Returns how many
    /// files were written.
    ///
    /// # Errors
    /// Returns the guard's error, or a store error mapped into it, if the transaction fails.
    fn promote_files_checked<E: From<peryx_storage::meta::MetaError>>(
        &self,
        index: &str,
        normalized: &str,
        display: &str,
        records: &[(String, String, Vec<u8>)],
        blob_sizes: &std::collections::BTreeMap<String, u64>,
        guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<usize, E>;

    /// Apply a per-file mutation to every uploaded record of `normalized` on `index`, journaling
    /// `action` for each record it changes, all in one transaction. Returns how many records changed.
    ///
    /// # Errors
    /// Returns the closure's error, or a store error mapped into it, if the transaction fails.
    fn mutate_uploads<E: From<peryx_storage::meta::MetaError>>(
        &self,
        index: &str,
        normalized: &str,
        action: &str,
        mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
    ) -> Result<usize, E>;

    /// List the `(filename, record)` pairs uploaded for `normalized` on `index`, sorted by filename.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn list_upload_entries(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, peryx_storage::meta::MetaError>;

    /// Delete one uploaded file record, returning whether it existed.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn delete_upload(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<bool, peryx_storage::meta::MetaError>;

    /// Visit raw upload records, keyed by `{index}/{normalized}/{filename}`.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_upload_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Record a yanked/hidden override for a file served from a read-only layer.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn put_override(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
        kind: &str,
    ) -> Result<(), peryx_storage::meta::MetaError>;

    /// Remove a file's override, returning whether one existed.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    fn delete_override(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<bool, peryx_storage::meta::MetaError>;

    /// List the `(filename, kind)` overrides recorded for `normalized` on `index`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    fn list_overrides(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Vec<(String, String)>, peryx_storage::meta::MetaError>;

    /// Visit raw override records, keyed by `{index}/{normalized}/{filename}`.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor fails.
    fn scan_override_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>>;

    /// Summarize observed projects and uploads for configured indexes.
    ///
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

    #[allow(
        clippy::too_many_arguments,
        reason = "one transaction needs every namespace's rows together"
    )]
    fn put_cached_page(
        &self,
        key: &str,
        record: &CachedIndex,
        index: &str,
        normalized: &str,
        display: &str,
        source: &str,
        upstream: Option<&str>,
        project_status: Option<&str>,
        project_status_reason: Option<&str>,
        files: &[(String, String, Option<u64>)],
        metadata: &[(String, String, String)],
    ) -> Result<(), peryx_storage::meta::MetaError> {
        index::put_cached_page(
            self,
            key,
            record,
            index,
            normalized,
            display,
            source,
            upstream,
            project_status,
            project_status_reason,
            files,
            metadata,
        )
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

    fn put_metadata(
        &self,
        artifact_sha256: &str,
        url: &str,
        metadata_sha256: &str,
        source: &str,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        files::put_metadata(self, artifact_sha256, url, metadata_sha256, source)
    }

    fn get_metadata(
        &self,
        artifact_sha256: &str,
    ) -> Result<Option<(String, String, String)>, peryx_storage::meta::MetaError> {
        files::get_metadata(self, artifact_sha256)
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
        file: &PublishedFile,
        guard: impl FnOnce(Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<bool, E> {
        uploads::publish_file_if(self, file, guard)
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
        index: &str,
        normalized: &str,
        display: &str,
        records: &[(String, String, Vec<u8>)],
        blob_sizes: &std::collections::BTreeMap<String, u64>,
        guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
    ) -> Result<usize, E> {
        uploads::promote_files_checked(self, index, normalized, display, records, blob_sizes, guard)
    }

    fn mutate_uploads<E: From<peryx_storage::meta::MetaError>>(
        &self,
        index: &str,
        normalized: &str,
        action: &str,
        mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
    ) -> Result<usize, E> {
        uploads::mutate_uploads(self, index, normalized, action, mutate)
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
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<bool, peryx_storage::meta::MetaError> {
        uploads::delete_upload(self, index, normalized, filename)
    }

    fn scan_upload_records<E>(
        &self,
        visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        uploads::scan_upload_records(self, visit)
    }

    fn put_override(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
        kind: &str,
    ) -> Result<(), peryx_storage::meta::MetaError> {
        uploads::put_override(self, index, normalized, filename, kind)
    }

    fn delete_override(
        &self,
        index: &str,
        normalized: &str,
        filename: &str,
    ) -> Result<bool, peryx_storage::meta::MetaError> {
        uploads::delete_override(self, index, normalized, filename)
    }

    fn list_overrides(
        &self,
        index: &str,
        normalized: &str,
    ) -> Result<Vec<(String, String)>, peryx_storage::meta::MetaError> {
        uploads::list_overrides(self, index, normalized)
    }

    fn scan_override_records<E>(
        &self,
        visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), peryx_storage::meta::MetaScanError<E>> {
        uploads::scan_override_records(self, visit)
    }

    fn summarize_indexes(
        &self,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, peryx_storage::meta::MetaError> {
        summary::summarize_indexes(self, index_names, recent_limit)
    }
}
