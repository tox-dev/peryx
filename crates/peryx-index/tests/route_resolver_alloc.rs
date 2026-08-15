use std::alloc::System;

use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind, RouteResolver};
use peryx_policy::Policy;
use stats_alloc::Region;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_route_resolver_lookup_allocates_nothing() {
    let routes = RouteResolver::new(&[Index {
        name: "alpha".to_owned(),
        route: "root/alpha".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }]);
    let region = Region::new(ALLOCATOR);
    let result = routes.resolve("root/alpha/items/artifact");

    assert_eq!(result, Some((0, "items/artifact")));
    let stats = region.change();
    assert_eq!((stats.allocations, stats.bytes_allocated), (0, 0));
}
