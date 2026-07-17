use std::alloc::System;

use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind, RouteResolver};
use peryx_policy::Policy;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_route_resolver_lookup_allocates_nothing() {
    let routes = RouteResolver::new(&[Index {
        name: "pypi".to_owned(),
        route: "root/pypi".to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }]);
    let region = Region::new(ALLOCATOR);
    let result = routes.resolve("root/pypi/simple/project");
    let stats = region.change();

    assert_eq!(
        (result, stats.allocations, stats.bytes_allocated),
        (Some((0, "simple/project")), 0, 0)
    );
}
