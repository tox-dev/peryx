//! Mirroring OCI images: pull a list of image references (each manifest and every blob it names)
//! into the store so a cached index can serve them with no upstream, the container analogue of
//! `peryx mirror sync`. A manifest list is followed into its per-platform manifests.

use std::collections::HashSet;
use std::sync::Arc;

use peryx_driver::ServingState;
use peryx_index::Index;
use peryx_storage::blob::Digest;
use peryx_upstream::UpstreamClient;
use serde::Serialize;

use crate::name::{ImageReference, Reference, parse_image_reference};
use crate::registry::{MAX_MANIFEST_BYTES, bounded_body, download_blob, serving_members};
use crate::settings::{IndexSettings, upstream_repo};
use crate::store::{self, Descriptors, Manifest};
use crate::upstream::Upstream;

/// The media type recorded for a manifest whose upstream response omits one.
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// How many distinct manifests one mirror run fetches before it gives up. A cyclic or explosively
/// wide graph, which a non-SHA-256 descriptor lets an upstream build without a fixed point, would
/// otherwise never drain the queue; the cap bounds the work well above any real multi-arch index.
const MAX_GRAPH_NODES: usize = 1024;

/// How deep a mirror run descends into nested manifest lists. Real images nest an index over a
/// handful of per-platform manifests; this leaves ample headroom while still ending a chain a hostile
/// upstream keeps extending.
const MAX_GRAPH_DEPTH: usize = 32;

/// One line of a mirror run: a manifest or blob that was synced, already cached, or failed, plus a
/// closing summary. The verb `kind` and `status` keep the report machine-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MirrorRow {
    pub kind: &'static str,
    pub index: String,
    pub repo: String,
    pub reference: String,
    pub digest: String,
    pub status: &'static str,
    pub bytes: u64,
    pub reason: String,
}

impl MirrorRow {
    pub(super) fn selected(index: &str, raw: &str) -> Self {
        parse_image_reference(raw).map_or_else(
            || Self::row("manifest", raw, "", "", "selected", 0, String::new()).with_index(index),
            |image| {
                let (Reference::Tag(reference) | Reference::Digest(reference)) = &image.reference;
                Self::row(
                    "manifest",
                    &image.repository,
                    reference,
                    "",
                    "selected",
                    0,
                    String::new(),
                )
                .with_index(index)
            },
        )
    }

    pub(super) fn count(index: &str, name: &'static str, value: u64) -> Self {
        Self::row("summary", "", name, "", name, value, String::new()).with_index(index)
    }

    fn with_index(mut self, index: &str) -> Self {
        index.clone_into(&mut self.index);
        self
    }

    fn synced(kind: &'static str, repo: &str, reference: &str, digest: &str, bytes: u64) -> Self {
        Self::row(kind, repo, reference, digest, "synced", bytes, String::new())
    }

    fn cached(kind: &'static str, repo: &str, reference: &str, digest: &str) -> Self {
        Self::row(kind, repo, reference, digest, "cached", 0, String::new())
    }

    fn error(kind: &'static str, repo: &str, reference: &str, digest: &str, reason: String) -> Self {
        Self::row(kind, repo, reference, digest, "error", 0, reason)
    }

    fn row(
        kind: &'static str,
        repo: &str,
        reference: &str,
        digest: &str,
        status: &'static str,
        bytes: u64,
        reason: String,
    ) -> Self {
        Self {
            kind,
            index: String::new(),
            repo: repo.to_owned(),
            reference: reference.to_owned(),
            digest: digest.to_owned(),
            status,
            bytes,
            reason,
        }
    }
}

/// What a mirror run does with each reference. `Sync` pulls anything missing; `Verify` only reports
/// whether the store already holds the manifest and every blob it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorMode {
    Sync,
    Verify,
}

/// The read-only context for one mirror run: the stores, the upstream client, and where to pull from.
struct Mirror<'a> {
    state: &'a Arc<ServingState>,
    upstream: &'a Upstream,
    client: &'a UpstreamClient,
    index: &'a str,
    settings: IndexSettings,
    mode: MirrorMode,
}

/// # Errors
/// Returns an error only on a store fault (metadata or blob io); a missing image, unreachable
/// upstream, or bad blob is a reported row, not an error, so one bad reference never aborts the run.
pub async fn mirror(
    state: &Arc<ServingState>,
    index: &Index,
    settings: IndexSettings,
    refs: &[String],
    mode: MirrorMode,
) -> anyhow::Result<Vec<MirrorRow>> {
    let mut rows = Vec::new();
    let Some(client) = serving_members(state, index).into_iter().find_map(Index::proxy_client) else {
        rows.push(
            MirrorRow::error("summary", "", "", "", "index has no cached upstream".to_owned()).with_index(&index.name),
        );
        return Ok(rows);
    };
    let upstream = Upstream::new();
    let context = Mirror {
        state,
        upstream: &upstream,
        client,
        index: &index.name,
        settings,
        mode,
    };
    for raw in refs {
        match parse_image_reference(raw) {
            Some(image) => context.one_ref(&image, &mut rows).await?,
            None => rows.push(MirrorRow::error(
                "manifest",
                raw,
                "",
                "",
                "not a valid image reference".to_owned(),
            )),
        }
    }
    let (synced, cached, errors, bytes) =
        rows.iter()
            .fold((0u64, 0u64, 0u64, 0u64), |(synced, cached, errors, bytes), row| {
                (
                    synced + u64::from(row.status == "synced"),
                    cached + u64::from(row.status == "cached"),
                    errors + u64::from(row.status == "error"),
                    bytes.saturating_add(row.bytes),
                )
            });
    rows.push(MirrorRow::row(
        "summary",
        "",
        "",
        "",
        if errors == 0 { "synced" } else { "error" },
        bytes,
        format!("{synced} synced, {cached} cached, {errors} errors"),
    ));
    for row in &mut rows {
        row.index.clone_from(&index.name);
    }
    Ok(rows)
}

/// Enqueue a manifest's child descriptors for the walk, deduplicating by digest and holding the graph
/// within `MAX_GRAPH_NODES` and `MAX_GRAPH_DEPTH`. A child already scheduled this run is skipped, so a
/// cycle or diamond fetches each digest once. Returns `true` when a bound is hit, after recording the
/// error row, so the caller stops the walk.
fn schedule_children(
    repo: &str,
    children: Vec<String>,
    depth: usize,
    visited: &mut HashSet<String>,
    pending: &mut Vec<(String, usize)>,
    rows: &mut Vec<MirrorRow>,
) -> bool {
    for child in children {
        if !visited.insert(child.clone()) {
            continue;
        }
        if depth > MAX_GRAPH_DEPTH {
            let reason = format!("manifest graph exceeds depth {MAX_GRAPH_DEPTH}");
            rows.push(MirrorRow::error("manifest", repo, &child, "", reason));
            return true;
        }
        if visited.len() > MAX_GRAPH_NODES {
            let reason = format!("manifest graph exceeds {MAX_GRAPH_NODES} nodes");
            rows.push(MirrorRow::error("manifest", repo, &child, "", reason));
            return true;
        }
        pending.push((child, depth));
    }
    false
}

/// What a manifest depends on, or `None` after reporting the schema rule its body breaks.
///
/// An upstream is not a push: peryx never accepted these bytes over the registry API, so nothing has
/// checked that they are a manifest at all. A body the media type's schema rejects names no
/// dependencies, which the walk would otherwise read as a graph that is already complete.
fn descriptors_of(
    manifest: &Manifest,
    repo: &str,
    reference: &str,
    digest: &str,
    rows: &mut Vec<MirrorRow>,
) -> Option<Descriptors> {
    match store::validated_descriptors(&manifest.media_type, &manifest.bytes) {
        Ok(descriptors) => Some(descriptors),
        Err(fault) => {
            rows.push(MirrorRow::error("manifest", repo, reference, digest, fault.to_string()));
            None
        }
    }
}

impl Mirror<'_> {
    /// The name `repo` is spelled with upstream. What lands in the store keeps the operator's spelling,
    /// so a mirrored image serves under the name it was asked for.
    fn upstream_repo<'a>(&self, repo: &'a str) -> std::borrow::Cow<'a, str> {
        upstream_repo(self.settings.library_prefix, self.client.base_url(), repo)
    }

    async fn one_ref(&self, image: &ImageReference, rows: &mut Vec<MirrorRow>) -> anyhow::Result<()> {
        let (reference, tag) = match &image.reference {
            Reference::Tag(tag) => (tag.as_str(), Some(tag.as_str())),
            Reference::Digest(digest) => (digest.as_str(), None),
        };
        if let Some(descriptors) = self.manifest_of(&image.repository, reference, tag, rows).await? {
            self.walk_manifest(&image.repository, descriptors, rows).await?;
        }
        Ok(())
    }

    /// Pull one manifest and hand back what it depends on. `None` is a reference this run reported on
    /// and will not walk.
    async fn manifest_of(
        &self,
        repo: &str,
        reference: &str,
        tag: Option<&str>,
        rows: &mut Vec<MirrorRow>,
    ) -> anyhow::Result<Option<Descriptors>> {
        if self.mode == MirrorMode::Verify {
            return self.verify_manifest(repo, reference, tag, rows);
        }
        let response = match self
            .upstream
            .manifest(self.client, &self.upstream_repo(repo), reference)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                rows.push(MirrorRow::error("manifest", repo, reference, "", err.to_string()));
                return Ok(None);
            }
        };
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or(DEFAULT_MANIFEST_TYPE)
            .to_owned();
        let bytes = bounded_body(response, MAX_MANIFEST_BYTES)
            .await
            .map_err(|err| anyhow::anyhow!(String::from(err)))?;
        let digest = format!("sha256:{}", Digest::of(&bytes).as_str());
        // A by-sha256-digest reference (no tag) pins the exact bytes; if the upstream, or a proxy
        // between, returns something else, storing it under the computed digest would report `synced`
        // while the requested manifest was never mirrored. A digest in another algorithm the spec
        // permits, peryx cannot recompute, so it trusts the upstream bytes rather than reject a valid
        // pull, the same guard the serving path keeps.
        let sha256_mismatch = reference.starts_with("sha256:") && reference != digest;
        if tag.is_none() && sha256_mismatch {
            rows.push(MirrorRow::error(
                "manifest",
                repo,
                reference,
                "",
                format!("upstream digest {digest} does not match requested {reference}"),
            ));
            return Ok(None);
        }
        let manifest = Manifest {
            media_type,
            bytes: bytes.to_vec(),
        };
        // Storing first and walking after would cache bytes no client can use under a digest the run
        // then reports complete, because a document that does not parse names no dependencies.
        let Some(descriptors) = descriptors_of(&manifest, repo, reference, &digest, rows) else {
            return Ok(None);
        };
        store::record_manifest(&self.state.meta, self.index, repo, &digest, &manifest)?;
        store::record_content_placement(&self.state.meta, &digest, store::OciArtifactOrigin::Mirrored, true)?;
        let search_invalidation = crate::search_oci::SearchInvalidationGuard::arm(self.state, repo);
        if let Some(tag) = tag {
            store::put_tag(&self.state.meta, self.index, repo, tag, &digest)?;
            store::set_tag_freshness(&self.state.meta, self.index, repo, tag, &digest, (self.state.clock)())?;
        }
        drop(search_invalidation);
        rows.push(MirrorRow::synced(
            "manifest",
            repo,
            reference,
            &digest,
            manifest.bytes.len() as u64,
        ));
        Ok(Some(descriptors))
    }

    fn verify_manifest(
        &self,
        repo: &str,
        reference: &str,
        tag: Option<&str>,
        rows: &mut Vec<MirrorRow>,
    ) -> anyhow::Result<Option<Descriptors>> {
        let digest = match tag {
            Some(tag) => {
                let Some(digest) = store::get_tag(&self.state.meta, self.index, repo, tag)? else {
                    rows.push(MirrorRow::error(
                        "manifest",
                        repo,
                        reference,
                        "",
                        "tag not mirrored".to_owned(),
                    ));
                    return Ok(None);
                };
                digest
            }
            None => reference.to_owned(),
        };
        let Some(manifest) = store::get_manifest(&self.state.meta, &digest)? else {
            rows.push(MirrorRow::error(
                "manifest",
                repo,
                reference,
                &digest,
                "manifest missing".to_owned(),
            ));
            return Ok(None);
        };
        // A stored manifest that no longer parses cannot be reported cached: its empty descriptor list
        // would pass verification for an image whose layers were never mirrored.
        let Some(descriptors) = descriptors_of(&manifest, repo, reference, &digest, rows) else {
            return Ok(None);
        };
        rows.push(MirrorRow::cached("manifest", repo, reference, &digest));
        Ok(Some(descriptors))
    }

    /// Follow a manifest to the blobs it needs, over a work queue rather than recursion: an image
    /// index enqueues its per-platform manifests; an image manifest names a config blob and layers.
    ///
    /// A descriptor digest is scheduled at most once per run, so a self-referential or cyclic graph
    /// terminates and a diamond fetches each shared descendant a single time. Bounds are enforced when
    /// a child is scheduled, before it is fetched, so a graph a hostile upstream keeps growing stops on
    /// a stable error row without the fetch that would follow.
    async fn walk_manifest(
        &self,
        repo: &str,
        descriptors: Descriptors,
        rows: &mut Vec<MirrorRow>,
    ) -> anyhow::Result<()> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut pending: Vec<(String, usize)> = Vec::new();
        let (children, blobs) = descriptors;
        for digest in blobs {
            self.blob(repo, &digest, rows).await;
        }
        if schedule_children(repo, children, 1, &mut visited, &mut pending, rows) {
            return Ok(());
        }
        while let Some((digest, depth)) = pending.pop() {
            let Some((children, blobs)) = self.manifest_of(repo, &digest, None, rows).await? else {
                continue;
            };
            for digest in blobs {
                self.blob(repo, &digest, rows).await;
            }
            if schedule_children(repo, children, depth + 1, &mut visited, &mut pending, rows) {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn blob(&self, repo: &str, digest: &str, rows: &mut Vec<MirrorRow>) {
        let Some(storage) = store::blob_digest(digest) else {
            rows.push(MirrorRow::error(
                "blob",
                repo,
                digest,
                digest,
                "unsupported digest".to_owned(),
            ));
            return;
        };
        match self.state.blobs.head(&storage).await {
            Ok(Some(_)) => {
                rows.push(MirrorRow::cached("blob", repo, digest, digest));
                return;
            }
            Ok(None) => {}
            Err(err) => {
                rows.push(MirrorRow::error("blob", repo, digest, digest, err.to_string()));
                return;
            }
        }
        if self.mode == MirrorMode::Verify {
            rows.push(MirrorRow::error(
                "blob",
                repo,
                digest,
                digest,
                "blob missing".to_owned(),
            ));
            return;
        }
        match self.upstream.blob(self.client, &self.upstream_repo(repo), digest).await {
            Ok(response) => match download_blob(&self.state.blobs, &storage, response).await {
                Ok(bytes) => rows.push(MirrorRow::synced("blob", repo, digest, digest, bytes)),
                Err(err) => {
                    rows.push(MirrorRow::error("blob", repo, digest, digest, err.to_string()));
                }
            },
            Err(err) => rows.push(MirrorRow::error("blob", repo, digest, digest, err.to_string())),
        }
    }
}
