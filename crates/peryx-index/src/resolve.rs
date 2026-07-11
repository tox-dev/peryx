//! Resolving a request against the configured indexes, and the order a virtual index merges them in.

use crate::index::{Index, IndexKind};

/// The part of `path` after `route`, requiring a segment boundary so `team/dev` does not match
/// `team/development`. `""` means the index route itself.
#[must_use]
pub fn remainder<'a>(path: &'a str, route: &str) -> Option<&'a str> {
    if path == route {
        return Some("");
    }
    path.strip_prefix(route)?.strip_prefix('/')
}

/// The position of the index whose route is the longest segment-aligned prefix of `path` (which has
/// no leading slash), and the path remainder after `route/`. `None` when no route matches.
#[must_use]
pub fn resolve_position<'a>(indexes: &[Index], path: &'a str) -> Option<(usize, &'a str)> {
    let mut best: Option<(usize, &str)> = None;
    for (position, index) in indexes.iter().enumerate() {
        let Some(rest) = remainder(path, &index.route) else {
            continue;
        };
        if best.is_none_or(|(current, _)| index.route.len() > indexes[current].route.len()) {
            best = Some((position, rest));
        }
    }
    best
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
    use super::{remainder, resolve_position, shadow_order};
    use crate::index::{Index, IndexKind};
    use peryx_core::Ecosystem;
    use peryx_identity::IndexAcl;
    use peryx_policy::Policy;
    use peryx_upstream::UpstreamClient;

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

    #[test]
    fn test_remainder_requires_a_segment_boundary() {
        assert_eq!(remainder("team/dev", "team/dev"), Some(""));
        assert_eq!(remainder("team/dev/simple", "team/dev"), Some("simple"));
        assert_eq!(remainder("team/development", "team/dev"), None);
    }

    #[test]
    fn test_resolve_position_prefers_the_longest_route() {
        let indexes = vec![index("short", "team", hosted()), index("long", "team/dev", hosted())];
        assert_eq!(resolve_position(&indexes, "team/dev/simple"), Some((1, "simple")));
        assert_eq!(resolve_position(&indexes, "team/other"), Some((0, "other")));
        assert_eq!(resolve_position(&indexes, "elsewhere"), None);
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
}
