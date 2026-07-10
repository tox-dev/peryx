//! The OCI search-document mapping: how stored image repositories and their tags become the neutral
//! [`PackageDocument`]s the [`PackageSearch`](peryx_http::search::PackageSearch) index stores.
//!
//! The tantivy index, schema, and querying are ecosystem-neutral and live in [`peryx_http::search`];
//! only the walk from an OCI index's stored tags to a repository's searchable text is format-specific,
//! so it sits behind the [`PackageIndexer`] seam here, as each ecosystem driver supplies its own.

use std::collections::BTreeSet;

use peryx_core::Ecosystem;
use peryx_http::search::{PackageDocument, PackageIndexer, PackageSource, SearchError};
use peryx_http::state::AppState;
use peryx_index::{Index, IndexKind};

use crate::store;

/// Produces OCI search documents (one per image repository) for the neutral search index.
#[derive(Debug, Clone, Copy, Default)]
pub struct OciIndexer;

impl PackageIndexer for OciIndexer {
    fn documents(&self, state: &AppState) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for index in &state.indexes {
            if index.ecosystem != Ecosystem::Oci {
                continue;
            }
            for repo in repositories(state, index)? {
                documents.push(document(state, index, &repo)?);
            }
        }
        Ok(documents)
    }
}

/// The distinct repositories an index serves: a cached or hosted index reads its own store; a virtual
/// index unions its members' repositories.
fn repositories(state: &AppState, index: &Index) -> Result<BTreeSet<String>, SearchError> {
    let mut repos = BTreeSet::new();
    collect(state, index, &mut repos)?;
    Ok(repos)
}

fn collect(state: &AppState, index: &Index, repos: &mut BTreeSet<String>) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            repos.extend(store::list_repositories(&state.meta, &index.name)?);
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect(state, state.index_at(position), repos)?;
            }
        }
    }
    Ok(())
}

/// One repository's search document: its name is the display and search text, its tags widen the text,
/// and its source follows the index role.
fn document(state: &AppState, index: &Index, repo: &str) -> Result<PackageDocument, SearchError> {
    let tags = tags(state, index, repo)?;
    let mut text = repo.to_owned();
    for tag in &tags {
        text.push(' ');
        text.push_str(tag);
    }
    Ok(PackageDocument {
        display_name: repo.to_owned(),
        normalized_name: repo.to_owned(),
        route: index.route.clone(),
        index: index.name.clone(),
        ecosystem: index.ecosystem.as_str().to_owned(),
        source: source(&index.kind),
        summary: Some(format!("{} tag{}", tags.len(), if tags.len() == 1 { "" } else { "s" })),
        text,
    })
}

/// Every tag a repository has across an index's stores, sorted and distinct.
fn tags(state: &AppState, index: &Index, repo: &str) -> Result<Vec<String>, SearchError> {
    let mut tags = BTreeSet::new();
    collect_tags(state, index, repo, &mut tags)?;
    Ok(tags.into_iter().collect())
}

fn collect_tags(state: &AppState, index: &Index, repo: &str, tags: &mut BTreeSet<String>) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            tags.extend(store::list_tags(&state.meta, &index.name, repo)?);
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_tags(state, state.index_at(position), repo, tags)?;
            }
        }
    }
    Ok(())
}

const fn source(kind: &IndexKind) -> PackageSource {
    match kind {
        IndexKind::Hosted { .. } => PackageSource::Uploaded,
        _ => PackageSource::Cached,
    }
}
