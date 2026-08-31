use std::collections::HashMap;

use crate::index::{Index, IndexKind};

pub struct RouteResolver {
    root: Node,
}

#[derive(Default)]
struct Node {
    position: Option<usize>,
    children: HashMap<Box<str>, Self>,
}

impl RouteResolver {
    /// Hash each request segment once instead of each complete prefix.
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

/// Put cached layers last so an upstream cannot shadow a hosted resource.
#[must_use]
pub fn shadow_order(indexes: &[Index], layers: &[usize]) -> Vec<usize> {
    let mut ordered = layers.to_vec();
    ordered.sort_by_key(|&position| matches!(indexes[position].kind, IndexKind::Cached { .. }));
    ordered
}

/// The leaf indexes `layers` serves from, hosted before cached, keeping configured order within
/// each group and listing a shared descendant once.
///
/// A virtual member never appears: it stands for the leaves beneath it, and a container has no
/// source class of its own, so ordering it as hosted lets a nested cache shadow a hosted sibling.
/// Detection is path-local, so a diamond still contributes its shared leaf while a cyclic branch
/// fails closed and contributes nothing.
#[must_use]
pub fn leaf_order(indexes: &[Index], layers: &[usize]) -> Vec<usize> {
    let mut leaves = leaves(indexes, layers);
    leaves.sort_by_key(|&position| matches!(indexes[position].kind, IndexKind::Cached { .. }));
    leaves
}

/// Hosted layers pass their journal frontier through virtual indexes.
#[must_use]
pub fn layers_include_hosted(indexes: &[Index], layers: &[usize]) -> bool {
    leaves(indexes, layers)
        .into_iter()
        .any(|position| matches!(indexes[position].kind, IndexKind::Hosted { .. }))
}

/// Follow nested layers because source policy applies to indirect cached sources too.
#[must_use]
pub fn reaches_cached(indexes: &[Index], position: usize) -> bool {
    leaves(indexes, &[position])
        .into_iter()
        .any(|position| matches!(indexes[position].kind, IndexKind::Cached { .. }))
}

/// The index at `position` followed by every index it composes, nested layers included.
///
/// A virtual index answers with whatever its layers hold, so a decision that must hold for the
/// content a route can serve - an access check, say - has to consider all of them, not just the
/// route the request named. A layer already visited is not revisited, so a mis-declared cycle
/// yields a finite set instead of looping.
#[must_use]
pub fn composed_indexes(indexes: &[Index], position: usize) -> Vec<usize> {
    let mut composed = vec![position];
    let mut pending = 0;
    while let Some(&position) = composed.get(pending) {
        pending += 1;
        let IndexKind::Virtual { layers, .. } = &indexes[position].kind else {
            continue;
        };
        for &layer in layers {
            if !composed.contains(&layer) {
                composed.push(layer);
            }
        }
    }
    composed
}

fn leaves(indexes: &[Index], layers: &[usize]) -> Vec<usize> {
    fn walk(indexes: &[Index], position: usize, path: &mut Vec<usize>, found: &mut Vec<usize>) {
        match &indexes[position].kind {
            IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
                if !found.contains(&position) {
                    found.push(position);
                }
            }
            IndexKind::Virtual { layers, .. } => {
                if path.contains(&position) {
                    return;
                }
                path.push(position);
                for &member in layers {
                    walk(indexes, member, path, found);
                }
                path.pop();
            }
        }
    }
    let mut found = Vec::new();
    for &position in layers {
        walk(indexes, position, &mut Vec::new(), &mut found);
    }
    found
}

#[cfg(test)]
#[path = "../tests/unit/resolve/tests.rs"]
mod tests;
