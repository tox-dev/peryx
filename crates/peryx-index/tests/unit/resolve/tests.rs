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
            remainder("team/dev/items", "team/dev"),
            remainder("team/development", "team/dev"),
        ],
        [Some(""), Some("items"), None]
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
            resolver.resolve("team/dev/items/naïve"),
            resolver.resolve("team/other"),
            resolver.resolve("team/development"),
            resolver.resolve("elsewhere"),
        ],
        [
            Some((1, "")),
            Some((1, "")),
            Some((1, "items/naïve")),
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
    let indexes = vec![index("alpha", "alpha", cached()), index("hosted", "hosted", hosted())];
    assert_eq!(shadow_order(&indexes, &[0, 1]), vec![1, 0]);
    assert_eq!(shadow_order(&indexes, &[1, 0]), vec![1, 0]);
}

#[test]
fn test_shadow_order_keeps_configured_order_within_a_group() {
    let indexes = vec![
        index("hosted-a", "a", hosted()),
        index("alpha", "alpha", cached()),
        index("hosted-b", "b", hosted()),
    ];
    assert_eq!(shadow_order(&indexes, &[0, 1, 2]), vec![0, 2, 1]);
}

#[test]
fn test_layers_include_hosted_reaches_a_hosted_member_through_a_nested_virtual() {
    let indexes = vec![
        index("hosted", "h", hosted()),
        index("alpha", "c", cached()),
        index("inner", "inner", virtual_layers(&[0])),
    ];
    assert!(layers_include_hosted(&indexes, &[1, 2]));
}

#[test]
fn test_layers_include_hosted_is_false_when_no_member_reaches_a_hosted_index() {
    let indexes = vec![
        index("alpha", "c", cached()),
        index("proxy-only", "p", virtual_layers(&[0])),
    ];
    assert!(!layers_include_hosted(&indexes, &[0, 1]));
}

#[test]
fn test_layers_include_hosted_terminates_on_a_virtual_cycle() {
    let indexes = vec![
        index("a", "a", virtual_layers(&[1])),
        index("b", "b", virtual_layers(&[0])),
    ];
    assert!(!layers_include_hosted(&indexes, &[0]));
}

#[test]
fn test_reaches_cached_reads_a_direct_member_kind() {
    let indexes = vec![index("alpha", "c", cached()), index("hosted", "h", hosted())];
    assert!(reaches_cached(&indexes, 0));
    assert!(!reaches_cached(&indexes, 1));
}

#[test]
fn test_reaches_cached_finds_a_cache_one_virtual_layer_down() {
    let indexes = vec![
        index("alpha", "c", cached()),
        index("hosted", "h", hosted()),
        index("inner", "inner", virtual_layers(&[1, 0])),
    ];
    assert!(reaches_cached(&indexes, 2));
}

#[test]
fn test_reaches_cached_finds_a_cache_several_virtual_layers_down() {
    let indexes = vec![
        index("alpha", "c", cached()),
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
        client: UpstreamClient::new("http://example.invalid/artifacts/").unwrap(),
        offline: false,
    }
}

const fn hosted() -> IndexKind {
    IndexKind::Hosted { volatile: false }
}

fn virtual_layers(layers: &[usize]) -> IndexKind {
    IndexKind::Virtual {
        layers: layers.to_vec(),
        write_target: None,
    }
}
