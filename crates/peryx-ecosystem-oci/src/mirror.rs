//! Mirroring OCI images: pull a list of image references (each manifest and every blob it names)
//! into the store so a cached index can serve them with no upstream, the container analogue of
//! `peryx mirror sync`. A manifest list is followed into its per-platform manifests.

use std::sync::Arc;

use peryx_driver::ServingState;
use peryx_index::Index;
use peryx_storage::blob::Digest;
use peryx_upstream::Auth;
use serde::Serialize;

use crate::registry::{download_blob, serving_members};
use crate::settings::{IndexSettings, upstream_repo};
use crate::store::{self, Manifest};
use crate::upstream::Upstream;

/// The media type recorded for a manifest whose upstream response omits one.
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// One line of a mirror run: a manifest or blob that was synced, already cached, or failed, plus a
/// closing summary. The verb `kind` and `status` keep the report machine-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MirrorRow {
    pub kind: &'static str,
    pub repo: String,
    pub reference: String,
    pub digest: String,
    pub status: &'static str,
    pub bytes: u64,
    pub reason: String,
}

impl MirrorRow {
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

/// A parsed image reference: `repo[:tag]` or `repo@sha256:...`. Tags never contain `/`, so the split
/// point is the last `:` after the final `/`; a bare name defaults to the `latest` tag.
struct ImageRef {
    repo: String,
    reference: String,
    by_digest: bool,
}

fn parse_ref(raw: &str) -> Option<ImageRef> {
    if let Some((repo, digest)) = raw.split_once('@') {
        return (!repo.is_empty() && !digest.is_empty()).then(|| ImageRef {
            repo: repo.to_owned(),
            reference: digest.to_owned(),
            by_digest: true,
        });
    }
    let last_slash = raw.rfind('/').map_or(0, |index| index + 1);
    let Some(colon) = raw[last_slash..].rfind(':') else {
        return Some(ImageRef {
            repo: raw.to_owned(),
            reference: "latest".to_owned(),
            by_digest: false,
        });
    };
    let split = last_slash + colon;
    Some(ImageRef {
        repo: raw[..split].to_owned(),
        reference: raw[split + 1..].to_owned(),
        by_digest: false,
    })
}

/// The read-only context for one mirror run: the stores, the upstream client, and where to pull from.
struct Mirror<'a> {
    state: &'a Arc<ServingState>,
    upstream: &'a Upstream,
    base: String,
    auth: Auth,
    index: &'a str,
    settings: IndexSettings,
    mode: MirrorMode,
}

/// Mirror every reference in `refs` through `index` in `mode`, under that index's `settings`.
///
/// `index` is a cached index, or a virtual index with a cached member. Returns one report row per
/// manifest, per blob, and a trailing summary.
///
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
    let Some((base, auth)) = serving_members(state, index).into_iter().find_map(|member| {
        member
            .proxy_client()
            .map(|client| (client.base_url().to_owned(), client.auth().clone()))
    }) else {
        rows.push(MirrorRow::error(
            "summary",
            &index.name,
            "",
            "",
            "index has no cached upstream".to_owned(),
        ));
        return Ok(rows);
    };
    let upstream = Upstream::new();
    let context = Mirror {
        state,
        upstream: &upstream,
        base,
        auth,
        index: &index.name,
        settings,
        mode,
    };
    for raw in refs {
        match parse_ref(raw) {
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
    let (synced, errors) = rows
        .iter()
        .fold((0u64, 0u64), |(synced, errors), row| match row.status {
            "synced" => (synced + 1, errors),
            "error" => (synced, errors + 1),
            _ => (synced, errors),
        });
    rows.push(MirrorRow::row(
        "summary",
        &index.name,
        "",
        "",
        if errors == 0 { "synced" } else { "error" },
        synced,
        format!("{synced} synced, {errors} errors"),
    ));
    Ok(rows)
}

impl Mirror<'_> {
    /// The name `repo` is spelled with upstream. What lands in the store keeps the operator's spelling,
    /// so a mirrored image serves under the name it was asked for.
    fn upstream_repo<'a>(&self, repo: &'a str) -> std::borrow::Cow<'a, str> {
        upstream_repo(self.settings.library_prefix, &self.base, repo)
    }

    /// Mirror one image reference: its manifest, any per-platform child manifests, and every blob.
    async fn one_ref(&self, image: &ImageRef, rows: &mut Vec<MirrorRow>) -> anyhow::Result<()> {
        let tag = (!image.by_digest).then_some(image.reference.as_str());
        if let Some(manifest) = self.manifest_of(&image.repo, &image.reference, tag, rows).await? {
            self.walk_manifest(&image.repo, &manifest, rows).await?;
        }
        Ok(())
    }

    /// Fetch or read one manifest, recording its row, and hand back its stored bytes for walking.
    async fn manifest_of(
        &self,
        repo: &str,
        reference: &str,
        tag: Option<&str>,
        rows: &mut Vec<MirrorRow>,
    ) -> anyhow::Result<Option<Manifest>> {
        if self.mode == MirrorMode::Verify {
            return self.verify_manifest(repo, reference, tag, rows);
        }
        let response = match self
            .upstream
            .manifest(&self.base, &self.auth, &self.upstream_repo(repo), reference)
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
        let bytes = response.bytes().await?;
        let digest = format!("sha256:{}", Digest::of(&bytes).as_str());
        // A reference that is itself a digest (no tag) pins the exact bytes; if the upstream, or a proxy
        // between, returns something else, storing it under the computed digest would report `synced`
        // while the requested manifest was never mirrored. The serving path makes the same check.
        if tag.is_none() && reference != digest {
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
        store::put_manifest(&self.state.meta, &digest, &manifest)?;
        if let Some(tag) = tag {
            store::put_tag(&self.state.meta, self.index, repo, tag, &digest)?;
            store::set_tag_freshness(&self.state.meta, self.index, repo, tag, &digest, (self.state.clock)())?;
        }
        rows.push(MirrorRow::synced(
            "manifest",
            repo,
            reference,
            &digest,
            manifest.bytes.len() as u64,
        ));
        Ok(Some(manifest))
    }

    /// Read a mirrored manifest from the store for verification, resolving a tag through its mapping.
    fn verify_manifest(
        &self,
        repo: &str,
        reference: &str,
        tag: Option<&str>,
        rows: &mut Vec<MirrorRow>,
    ) -> anyhow::Result<Option<Manifest>> {
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
        rows.push(MirrorRow::cached("manifest", repo, reference, &digest));
        Ok(Some(manifest))
    }

    /// Follow a manifest to the blobs it needs, over a work queue rather than recursion: an image
    /// index enqueues its per-platform manifests; an image manifest names a config blob and layers.
    async fn walk_manifest(&self, repo: &str, manifest: &Manifest, rows: &mut Vec<MirrorRow>) -> anyhow::Result<()> {
        let mut pending = vec![manifest.bytes.clone()];
        while let Some(bytes) = pending.pop() {
            let (children, blobs) = store::manifest_descriptors(&bytes);
            for child in children {
                if let Some(child_manifest) = self.manifest_of(repo, &child, None, rows).await? {
                    pending.push(child_manifest.bytes);
                }
            }
            for digest in blobs {
                self.blob(repo, &digest, rows).await;
            }
        }
        Ok(())
    }

    /// Sync or verify one blob by digest.
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
        if self.state.blobs.exists(&storage) {
            rows.push(MirrorRow::cached("blob", repo, digest, digest));
            return;
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
        match self
            .upstream
            .blob(&self.base, &self.auth, &self.upstream_repo(repo), digest)
            .await
        {
            Ok(response) => {
                if download_blob(&self.state.blobs, &storage, response).await.is_ok() {
                    rows.push(MirrorRow::synced(
                        "blob",
                        repo,
                        digest,
                        digest,
                        blob_size(self.state, &storage),
                    ));
                } else {
                    rows.push(MirrorRow::error(
                        "blob",
                        repo,
                        digest,
                        digest,
                        "digest verification failed".to_owned(),
                    ));
                }
            }
            Err(err) => rows.push(MirrorRow::error("blob", repo, digest, digest, err.to_string())),
        }
    }
}

/// The on-disk size of a stored blob, or `0` when its file cannot be stat'd.
fn blob_size(state: &ServingState, storage: &Digest) -> u64 {
    state
        .blobs
        .path_for(storage)
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}
