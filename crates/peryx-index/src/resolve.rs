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

/// Hosted layers pass their journal frontier through virtual indexes.
#[must_use]
pub fn layers_include_hosted(indexes: &[Index], layers: &[usize]) -> bool {
    layers_reach(indexes, layers, |kind| matches!(kind, IndexKind::Hosted { .. }))
}

/// Follow nested layers because source policy applies to indirect cached sources too.
#[must_use]
pub fn reaches_cached(indexes: &[Index], position: usize) -> bool {
    layers_reach(indexes, &[position], |kind| matches!(kind, IndexKind::Cached { .. }))
}

fn layers_reach(indexes: &[Index], layers: &[usize], target: fn(&IndexKind) -> bool) -> bool {
    fn walk(indexes: &[Index], position: usize, path: &mut Vec<usize>, target: fn(&IndexKind) -> bool) -> bool {
        if path.contains(&position) {
            return false;
        }
        let kind = &indexes[position].kind;
        if target(kind) {
            return true;
        }
        match kind {
            IndexKind::Virtual { layers, .. } => {
                path.push(position);
                let found = layers.iter().any(|&member| walk(indexes, member, path, target));
                path.pop();
                found
            }
            IndexKind::Cached { .. } | IndexKind::Hosted { .. } => false,
        }
    }
    layers
        .iter()
        .any(|&position| walk(indexes, position, &mut Vec::new(), target))
}

#[cfg(test)]
#[path = "../tests/unit/resolve/tests.rs"]
mod tests;
