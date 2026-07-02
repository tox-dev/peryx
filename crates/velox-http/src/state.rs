//! Shared application state and index routing.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use velox_storage::blob::BlobStore;
use velox_storage::meta::MetaStore;
use velox_upstream::UpstreamClient;

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// One resolved index. `layers`/`upload` in an overlay are indices into [`AppState::indexes`], so
/// resolution is a plain vector walk with no name lookups at request time.
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub kind: IndexKind,
}

/// The runtime shape of an index: a mirror owns its upstream client, a local store its upload
/// policy, an overlay the resolved positions of its layers and upload target.
#[derive(Debug)]
pub enum IndexKind {
    Mirror(UpstreamClient),
    Local {
        upload_token: Option<String>,
        volatile: bool,
    },
    Overlay {
        layers: Vec<usize>,
        upload: Option<usize>,
    },
}

/// Everything a request handler needs. Shared as `Arc<AppState>`.
pub struct AppState {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    /// How long a cached simple page is served before revalidating, in seconds.
    pub ttl_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
    /// PEP 658/714 `.metadata` sibling requests served, exposed via `/metrics`. Downstream clients
    /// only hit this when they take the metadata-only resolution fast path, so it is the server-side
    /// proof that pip and uv resolve through velox without downloading whole wheels.
    pub metadata_requests: AtomicU64,
    pub indexes: Vec<Index>,
}

impl AppState {
    /// Build the state with a system clock.
    #[must_use]
    pub fn new(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>) -> Self {
        Self::with_clock(meta, blobs, ttl_secs, indexes, Arc::new(system_now))
    }

    /// Build the state with an injected clock.
    #[must_use]
    pub fn with_clock(meta: MetaStore, blobs: BlobStore, ttl_secs: i64, indexes: Vec<Index>, clock: Clock) -> Self {
        Self {
            meta,
            blobs,
            ttl_secs,
            clock,
            requests: AtomicU64::new(0),
            metadata_requests: AtomicU64::new(0),
            indexes,
        }
    }

    /// Find the index whose route is the longest segment-aligned prefix of `path` (which has no
    /// leading slash), and the path remainder after `route/`. Returns `None` if no route matches.
    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a Index, &'a str)> {
        let mut best: Option<(&Index, &str)> = None;
        for index in &self.indexes {
            let Some(rest) = remainder(path, &index.route) else {
                continue;
            };
            if best.is_none_or(|(current, _)| index.route.len() > current.route.len()) {
                best = Some((index, rest));
            }
        }
        best
    }

    /// The index at position `pos` (an overlay layer or upload target).
    #[must_use]
    pub fn index_at(&self, pos: usize) -> &Index {
        &self.indexes[pos]
    }

    /// Describe every configured index for presentation: kind name, overlay layer names, and
    /// whether uploads are enabled. Shared by `/+status` and the web UI.
    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        self.indexes
            .iter()
            .map(|index| {
                let (kind, layers, uploads) = match &index.kind {
                    IndexKind::Mirror(_) => ("mirror", Vec::new(), false),
                    IndexKind::Local { upload_token, .. } => ("local", Vec::new(), upload_token.is_some()),
                    IndexKind::Overlay { layers, upload } => {
                        let names = layers.iter().map(|&pos| self.index_at(pos).name.clone()).collect();
                        let uploads = upload.is_some_and(|pos| {
                            matches!(
                                &self.index_at(pos).kind,
                                IndexKind::Local {
                                    upload_token: Some(_),
                                    ..
                                }
                            )
                        });
                        ("overlay", names, uploads)
                    }
                };
                IndexDescription {
                    name: index.name.clone(),
                    route: index.route.clone(),
                    kind,
                    layers,
                    uploads,
                }
            })
            .collect()
    }
}

/// A configured index as presented to humans: on the dashboard and in `/+status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub kind: &'static str,
    pub layers: Vec<String>,
    pub uploads: bool,
}

/// The part of `path` after `route`, requiring a segment boundary so `team/dev` does not match
/// `team/development`. `""` means the index root itself.
fn remainder<'a>(path: &'a str, route: &str) -> Option<&'a str> {
    if path == route {
        return Some("");
    }
    path.strip_prefix(route)?.strip_prefix('/')
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
