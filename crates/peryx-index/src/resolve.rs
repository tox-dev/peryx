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
/// last. That is the dependency-confusion defense — a name a hosted member serves is never answered
/// from upstream — and making it structural means no `layers` ordering an operator writes can lose it.
/// The sort is stable, so `["hosted-a", "pypi", "hosted-b"]` merges as `["hosted-a", "hosted-b",
/// "pypi"]`.
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
mod tests {
    use super::{RouteResolver, layers_include_hosted, reaches_cached, remainder, shadow_order};
    use crate::index::{Index, IndexKind};
    use peryx_core::Ecosystem;
    use peryx_identity::IndexAcl;
    use peryx_policy::Policy;
    use peryx_upstream::UpstreamClient;

    #[test]
    fn test_remainder_requires_a_segment_boundary() {
        assert_eq!(
            [
                remainder("team/dev", "team/dev"),
                remainder("team/dev/simple", "team/dev"),
                remainder("team/development", "team/dev"),
            ],
            [Some(""), Some("simple"), None]
        );
    }

    #[test]
    fn test_route_resolver_prefers_the_longest_segment_aligned_route() {
        let indexes = vec![index("short", "team", hosted()), index("long", "team/dev", hosted())];
        let resolver = RouteResolver::new(&indexes);
        assert_eq!(
            [
                resolver.resolve("team/dev"),
                resolver.resolve("team/dev/"),
                resolver.resolve("team/dev/simple/naïve"),
                resolver.resolve("team/other"),
                resolver.resolve("team/development"),
                resolver.resolve("elsewhere"),
            ],
            [
                Some((1, "")),
                Some((1, "")),
                Some((1, "simple/naïve")),
                Some((0, "other")),
                Some((0, "development")),
                None,
            ]
        );
    }

    #[test]
    fn test_route_resolver_walks_through_a_non_terminal_prefix() {
        let indexes = vec![index("deep", "a/b/c", hosted())];
        let resolver = RouteResolver::new(&indexes);
        assert_eq!(
            [
                resolver.resolve("a"),
                resolver.resolve("a/b"),
                resolver.resolve("a/b/c"),
                resolver.resolve("a/b/c/d"),
                resolver.resolve("a/x/c"),
            ],
            [None, None, Some((0, "")), Some((0, "d")), None]
        );
    }

    #[test]
    fn test_route_resolver_matches_the_naive_reference_on_arbitrary_paths() {
        let indexes = vec![
            index("root", "team", hosted()),
            index("dev", "team/dev", hosted()),
            index("deep", "team/dev/tools", hosted()),
        ];
        let resolver = RouteResolver::new(&indexes);
        for path in [
            "team",
            "team/",
            "team/dev",
            "team/dev/tools",
            "team/dev/tools/extra",
            "team/development",
            "team//dev",
            "other",
            "",
        ] {
            assert_eq!(resolver.resolve(path), naive_resolve(&indexes, path), "path {path:?}");
        }
    }

    fn naive_resolve<'a>(indexes: &[Index], path: &'a str) -> Option<(usize, &'a str)> {
        let mut best: Option<(usize, &str)> = None;
        for (position, index) in indexes.iter().enumerate() {
            if let Some(rest) = remainder(path, &index.route)
                && best.is_none_or(|(current, _)| index.route.len() > indexes[current].route.len())
            {
                best = Some((position, rest));
            }
        }
        best
    }

    #[test]
    fn test_shadow_order_puts_cached_members_last_whatever_the_configured_order() {
        let indexes = vec![index("pypi", "pypi", cached()), index("hosted", "hosted", hosted())];
        assert_eq!(shadow_order(&indexes, &[0, 1]), vec![1, 0]);
        assert_eq!(shadow_order(&indexes, &[1, 0]), vec![1, 0]);
    }

    #[test]
    fn test_shadow_order_keeps_configured_order_within_a_group() {
        let indexes = vec![
            index("hosted-a", "a", hosted()),
            index("pypi", "pypi", cached()),
            index("hosted-b", "b", hosted()),
        ];
        assert_eq!(shadow_order(&indexes, &[0, 1, 2]), vec![0, 2, 1]);
    }

    #[test]
    fn test_layers_include_hosted_reaches_a_hosted_member_through_a_nested_virtual() {
        let indexes = vec![
            index("hosted", "h", hosted()),
            index("pypi", "c", cached()),
            index("inner", "inner", virtual_layers(&[0])),
        ];
        // The cached member (position 1) is inspected first and does not count; the nested virtual
        // (position 2) reaches the hosted member, so the layer set inherits a journal serial.
        assert!(layers_include_hosted(&indexes, &[1, 2]));
    }

    #[test]
    fn test_layers_include_hosted_is_false_when_no_member_reaches_a_hosted_index() {
        let indexes = vec![
            index("pypi", "c", cached()),
            index("proxy-only", "p", virtual_layers(&[0])),
        ];
        assert!(!layers_include_hosted(&indexes, &[0, 1]));
    }

    #[test]
    fn test_reaches_cached_reads_a_direct_member_kind() {
        let indexes = vec![index("pypi", "c", cached()), index("hosted", "h", hosted())];
        assert!(reaches_cached(&indexes, 0));
        assert!(!reaches_cached(&indexes, 1));
    }

    #[test]
    fn test_reaches_cached_finds_a_cache_one_virtual_layer_down() {
        let indexes = vec![
            index("pypi", "c", cached()),
            index("hosted", "h", hosted()),
            index("inner", "inner", virtual_layers(&[1, 0])),
        ];
        assert!(reaches_cached(&indexes, 2));
    }

    #[test]
    fn test_reaches_cached_finds_a_cache_several_virtual_layers_down() {
        let indexes = vec![
            index("pypi", "c", cached()),
            index("inner", "inner", virtual_layers(&[0])),
            index("middle", "middle", virtual_layers(&[1])),
            index("outer", "outer", virtual_layers(&[2])),
        ];
        assert!(reaches_cached(&indexes, 3));
    }

    #[test]
    fn test_reaches_cached_is_false_for_a_hosted_only_virtual_tree() {
        let indexes = vec![
            index("hosted", "h", hosted()),
            index("inner", "inner", virtual_layers(&[0])),
            index("outer", "outer", virtual_layers(&[1])),
        ];
        assert!(!reaches_cached(&indexes, 2));
    }

    #[test]
    fn test_reaches_cached_terminates_on_a_virtual_cycle() {
        // A configuration a build never emits, but the walk must not recur forever if one arises: two
        // virtual indexes reference each other and reach no cache, so the answer is a terminating false.
        let indexes = vec![
            index("a", "a", virtual_layers(&[1])),
            index("b", "b", virtual_layers(&[0])),
        ];
        assert!(!reaches_cached(&indexes, 0));
    }

    fn index(name: &str, route: &str, kind: IndexKind) -> Index {
        Index {
            name: name.to_owned(),
            route: route.to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind,
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }
    }

    fn cached() -> IndexKind {
        IndexKind::Cached {
            client: UpstreamClient::new("http://example.invalid/simple/").unwrap(),
            offline: false,
        }
    }

    const fn hosted() -> IndexKind {
        IndexKind::Hosted { volatile: false }
    }

    fn virtual_layers(layers: &[usize]) -> IndexKind {
        IndexKind::Virtual {
            layers: layers.to_vec(),
            upload: None,
        }
    }
}
