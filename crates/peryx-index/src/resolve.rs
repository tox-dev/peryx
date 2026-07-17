//! Resolving a request against the configured indexes, and the order a virtual index merges them in.

use std::collections::HashMap;

use crate::index::{Index, IndexKind};

/// Immutable repository-route positions for request dispatch.
pub struct RouteResolver {
    positions: HashMap<Box<str>, usize>,
}

impl RouteResolver {
    /// Copy validated routes once so request lookup can borrow path slices.
    #[must_use]
    pub fn new(indexes: &[Index]) -> Self {
        Self {
            positions: indexes
                .iter()
                .enumerate()
                .map(|(position, index)| (Box::from(index.route.as_str()), position))
                .collect(),
        }
    }

    /// Resolve the longest segment-aligned route prefix without allocating.
    #[must_use]
    pub fn resolve<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        let mut end = path.len();
        loop {
            if let Some(&position) = self.positions.get(&path[..end]) {
                return Some((position, if end == path.len() { "" } else { &path[end + 1..] }));
            }
            end = path[..end].rfind('/')?;
        }
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

#[cfg(test)]
mod tests {
    use super::{RouteResolver, remainder, shadow_order};
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

    fn index(name: &str, route: &str, kind: IndexKind) -> Index {
        Index {
            name: name.to_owned(),
            route: route.to_owned(),
            ecosystem: Ecosystem::Pypi,
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
}
