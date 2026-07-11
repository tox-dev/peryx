//! The OCI search-document mapping: how stored image repositories and their tags become the neutral
//! [`PackageDocument`]s the [`PackageSearch`](peryx_search::PackageSearch) index stores.
//!
//! The tantivy index, schema, and querying are ecosystem-neutral and live in `peryx-search`;
//! only the walk from an OCI index's stored tags to a repository's searchable text is format-specific,
//! so it sits behind the [`PackageIndexer`] seam here, as each ecosystem driver supplies its own.

use std::collections::BTreeSet;

use peryx_core::Ecosystem;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_search::{IndexerCtx, PackageDocument, PackageIndexer, PackageSource, SearchError};

use crate::store;

/// Produces OCI search documents (one per image repository) for the neutral search index.
#[derive(Debug, Clone, Copy, Default)]
pub struct OciIndexer;

impl PackageIndexer for OciIndexer {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for index in ctx.indexes {
            if index.ecosystem != Ecosystem::Oci {
                continue;
            }
            for repo in repositories(ctx, index)? {
                documents.push(document(ctx, index, &repo)?);
            }
        }
        Ok(documents)
    }
}

/// The distinct repositories an index serves: a cached or hosted index reads its own store; a virtual
/// index unions its members' repositories.
fn repositories(ctx: &IndexerCtx<'_>, index: &Index) -> Result<BTreeSet<String>, SearchError> {
    let mut repos = BTreeSet::new();
    collect(ctx, index, &mut repos)?;
    Ok(repos)
}

fn collect(ctx: &IndexerCtx<'_>, index: &Index, repos: &mut BTreeSet<String>) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            // A policy-blocked repository is hidden on reads (`policy_blocks` in the serving path), so
            // it must not surface through search either; the PyPI indexer filters the same way.
            for repo in store::list_repositories(ctx.meta, &index.name)? {
                if index.policy.check_project(PolicyAction::Serve, &repo).is_ok() {
                    repos.insert(repo);
                }
            }
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect(ctx, ctx.index_at(position), repos)?;
            }
        }
    }
    Ok(())
}

/// One repository's search document: its name is the display and search text, its tags widen the text,
/// and its source follows the index role.
fn document(ctx: &IndexerCtx<'_>, index: &Index, repo: &str) -> Result<PackageDocument, SearchError> {
    let tags = tags(ctx, index, repo)?;
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
fn tags(ctx: &IndexerCtx<'_>, index: &Index, repo: &str) -> Result<Vec<String>, SearchError> {
    let mut tags = BTreeSet::new();
    collect_tags(ctx, index, repo, &mut tags)?;
    Ok(tags.into_iter().collect())
}

fn collect_tags(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    repo: &str,
    tags: &mut BTreeSet<String>,
) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            tags.extend(store::list_tags(ctx.meta, &index.name, repo)?);
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_tags(ctx, ctx.index_at(position), repo, tags)?;
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
