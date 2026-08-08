//! The OCI search-document mapping: how stored image repositories and their tags become the neutral
//! [`PackageDocument`]s the [`PackageSearch`](peryx_search::PackageSearch) index stores.
//!
//! The tantivy index, schema, and querying are ecosystem-neutral and live in `peryx-search`;
//! only the walk from an OCI index's stored tags to a repository's searchable text is format-specific,
//! so it sits behind the [`PackageIndexer`] seam here, as each ecosystem driver supplies its own.

use std::collections::BTreeSet;

use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_search::{
    IndexerCtx, PackageDocument, PackageIndexer, PackageSource, ProjectUpdate, SearchError, project_key,
};

use crate::store;

/// Produces OCI search documents (one per image repository) for the neutral search index.
#[derive(Debug, Clone, Copy, Default)]
pub struct OciIndexer;

impl PackageIndexer for OciIndexer {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for index in ctx.indexes {
            if index.ecosystem != crate::ECOSYSTEM {
                continue;
            }
            for repo in repositories(ctx, index)? {
                documents.push(document(ctx, index, &repo)?);
            }
        }
        Ok(documents)
    }

    /// Re-derive one repository across every OCI index that serves it. A repository the index does not
    /// serve contributes nothing, so a scoped refresh for another ecosystem's project touches no OCI
    /// document; membership is decided from the repository's own tag keys, not a full repository scan.
    fn project_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        let mut update = ProjectUpdate::default();
        for index in ctx.indexes {
            if index.ecosystem != crate::ECOSYSTEM || !serves_repository(ctx, index, name)? {
                continue;
            }
            update.keys.push(project_key(&index.route, name));
            update.documents.push(document(ctx, index, name)?);
        }
        Ok(update)
    }
}

/// Whether `index` serves `repo`, mirroring [`repositories`] for a single repository: a cached or hosted
/// index serves it when policy allows and it has at least one tag; a virtual index serves it when any
/// layer does. It reads only that repository's tag keys, so the check stays cheap on a large registry.
fn serves_repository(ctx: &IndexerCtx<'_>, index: &Index, repo: &str) -> Result<bool, SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            Ok(index.policy.check_project(PolicyAction::Serve, repo).is_ok()
                && !store::list_tags(ctx.meta, &index.name, repo)?.is_empty())
        }
        IndexKind::Virtual { layers, .. } => layers.iter().try_fold(false, |served, &position| {
            Ok(served || serves_repository(ctx, ctx.index_at(position), repo)?)
        }),
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
        available_locally: available_locally(ctx, index, repo)?,
        summary: Some(format!("{} tag{}", tags.len(), if tags.len() == 1 { "" } else { "s" })),
        text,
    })
}

/// Whether the repository can be pulled from local storage right now, decided from the #441 placement
/// of its tags' target manifests so search agrees with a by-digest read without probing the content
/// store. A repository is locally available when at least one tag targets a manifest whose bytes are
/// local: a pushed manifest, or a mirrored one already fetched. A tag whose manifest was discovered
/// but never fetched projects remote-only and does not count.
fn available_locally(ctx: &IndexerCtx<'_>, index: &Index, repo: &str) -> Result<bool, SearchError> {
    let mut targets = BTreeSet::new();
    collect_tag_targets(ctx, index, repo, &mut targets)?;
    for digest in targets {
        if ctx
            .meta
            .get_artifact_placement(&digest)?
            .is_some_and(|placement| placement.availability.is_local())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The distinct manifest digests the repository's tags target across an index's stores, unioned over a
/// virtual index's layers the same way its tags and repositories are.
fn collect_tag_targets(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    repo: &str,
    targets: &mut BTreeSet<String>,
) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            for (_tag, digest) in store::list_tag_targets(ctx.meta, &index.name, repo)? {
                targets.insert(digest);
            }
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_tag_targets(ctx, ctx.index_at(position), repo, targets)?;
            }
        }
    }
    Ok(())
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
