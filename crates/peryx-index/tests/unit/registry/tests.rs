use super::{IndexRegistry, IndexSet};
use crate::index::{Index, IndexKind};
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_policy::Policy;

fn index(name: &str, route: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

#[test]
fn test_index_set_resolves_against_its_own_indexes() {
    let set = IndexSet::new(vec![index("a", "team"), index("b", "team/dev")]);
    assert_eq!(set.indexes().len(), 2);
    assert_eq!(set.index_at(1).name, "b");
    assert_eq!(set.resolve_position("team/dev/simple"), Some((1, "simple")));
    let (resolved, rest) = set.resolve("team/other").unwrap();
    assert_eq!((resolved.name.as_str(), rest), ("a", "other"));
    assert!(set.resolve("elsewhere").is_none());
}

#[test]
fn test_registry_swaps_the_serving_set_without_disturbing_a_held_snapshot() {
    let registry = IndexRegistry::new(vec![index("a", "old")]);
    let before = registry.snapshot();
    assert_eq!(before.resolve_position("old"), Some((0, "")));
    assert_eq!(before.resolve_position("new"), None);

    let replaced = registry.replace(vec![index("b", "new")]);
    assert_eq!(replaced.indexes()[0].route, "old");

    // The snapshot taken before the swap still resolves the old route.
    assert_eq!(before.resolve_position("old"), Some((0, "")));
    assert_eq!(before.resolve_position("new"), None);

    // A fresh snapshot resolves the new route and no longer the old one.
    let after = registry.snapshot();
    assert_eq!(after.resolve_position("new"), Some((0, "")));
    assert_eq!(after.resolve_position("old"), None);
}
