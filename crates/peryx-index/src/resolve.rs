//! Resolving a request against the configured indexes, and the order a virtual index merges them in.

use std::collections::HashMap;

use crate::index::{Index, IndexKind};

/// Immutable repository-route positions for request dispatch.
pub struct RouteResolver {
    root: Node,
}

/// One segment of the route trie: the index position registered at this exact prefix, if any, and the
/// children reached by the next path segment.
#[derive(Default)]
struct Node {
    position: Option<usize>,
    children: HashMap<Box<str>, Self>,
}

impl RouteResolver {
    /// Index every route as a chain of segment nodes so a later lookup hashes each request segment once
    /// instead of re-hashing the whole prefix at every depth.
    #[must_use]
    pub fn new(indexes: &[Index]) -> Self {
        let mut root = Node::default();
        for (position, index) in indexes.iter().enumerate() {
            let mut node = &mut root;
            for segment in index.route.split('/') {
                node = node.children.entry(Box::from(segment)).or_default();
            }
            node.position = Some(position);
        }
        Self { root }
    }

    /// Resolve the longest segment-aligned route prefix without allocating, in one linear forward pass:
    /// walk the trie segment by segment, remembering the deepest node that is itself a route.
    #[must_use]
    pub fn resolve<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        let mut node = &self.root;
        let mut best: Option<(usize, usize)> = None;
        let mut start = 0;
        loop {
            let end = path[start..].find('/').map_or(path.len(), |offset| start + offset);
            let Some(child) = node.children.get(&path[start..end]) else {
                break;
            };
            node = child;
            if let Some(position) = node.position {
                best = Some((position, end));
            }
            if end == path.len() {
                break;
            }
            start = end + 1;
        }
        best.map(|(position, end)| (position, if end == path.len() { "" } else { &path[end + 1..] }))
    }
}

/// The part of `path` after `route`, requiring a segment boundary so `team/dev` does not match
/// `team/development`. `""` means the index route itself.
#[must_use]
pub fn remainder<'a>(path: &'a str, route: &str) -> Option<&'a str> {
    if path == route {
        return Some("");
    }
    path.strip_prefix(route)?.strip_prefix('/')
}

/// A virtual index's members in shadowing order: every non-cached member first, then the cached ones.
///
/// Within each group the configured order decides precedence, but a cached member always resolves
/// last. That is the dependency-confusion defense - a name a hosted member serves is never answered
/// from upstream - and making it structural means no `layers` ordering an operator writes can lose it.
/// The sort is stable, so `["hosted-a", "cached", "hosted-b"]` merges as `["hosted-a", "hosted-b",
/// "cached"]`.
#[must_use]
pub fn shadow_order(indexes: &[Index], layers: &[usize]) -> Vec<usize> {
    let mut ordered = layers.to_vec();
    ordered.sort_by_key(|&position| matches!(indexes[position].kind, IndexKind::Cached { .. }));
    ordered
}

/// Whether a virtual index's `layers` reach a hosted index, directly or through a nested virtual member.
///
/// A hosted member is the only kind that carries a local journal serial, so a replica's readable-frontier
/// gate uses this to tell whether a virtual index inherits one and must be held until its members catch
/// up. A cached member reports upstream state the frontier does not govern, so it does not count.
#[must_use]
pub fn layers_include_hosted(indexes: &[Index], layers: &[usize]) -> bool {
    layers.iter().any(|&position| match &indexes[position].kind {
        IndexKind::Hosted { .. } => true,
        IndexKind::Cached { .. } => false,
        IndexKind::Virtual { layers, .. } => layers_include_hosted(indexes, layers),
    })
}

/// Whether the index at `position` resolves through any cached source: a cached index itself, or a
/// virtual index that reaches one through its members at any depth.
///
/// Source policy (protected names, no-fallback, private-first) restricts what a *cached* member may
/// answer, and must recognise a cache reached through a nested virtual layer just as it does a direct
/// one. A shallow check of the direct member's kind would let a single extra virtual wrapper reopen the
/// upstream path a policy closed, which is a dependency-confusion foothold.
///
/// The walk is cycle-safe: a virtual index whose members loop back to it is visited once on a path, so a
/// self-referential configuration terminates instead of recurring forever.
#[must_use]
pub fn reaches_cached(indexes: &[Index], position: usize) -> bool {
    fn walk(indexes: &[Index], position: usize, path: &mut Vec<usize>) -> bool {
        if path.contains(&position) {
            return false;
        }
        match &indexes[position].kind {
            IndexKind::Cached { .. } => true,
            IndexKind::Hosted { .. } => false,
            IndexKind::Virtual { layers, .. } => {
                path.push(position);
                let found = layers.iter().any(|&member| walk(indexes, member, path));
                path.pop();
                found
            }
        }
    }
    walk(indexes, position, &mut Vec::new())
}

#[cfg(test)]
#[path = "../tests/unit/resolve/tests.rs"]
mod tests;
