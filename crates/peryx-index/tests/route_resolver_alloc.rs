use std::alloc::System;

use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind, RouteResolver};
use peryx_policy::Policy;
#[cfg(not(coverage))]
use stats_alloc::Region;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_route_resolver_lookup_allocates_nothing() {
    let routes = RouteResolver::new(&[Index {
        name: "pypi".to_owned(),
        route: "root/pypi".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }]);
    // `-C instrument-coverage` perturbs the process allocation count, so the zero-allocation
    // guarantee is only measurable off the instrumented run. cargo-llvm-cov sets `--cfg coverage`:
    // the normal test matrix still verifies the guarantee, while the coverage run keeps exercising
    // `resolve` for its line coverage without the unmeasurable assertion.
    #[cfg(not(coverage))]
    let region = Region::new(ALLOCATOR);
    let result = routes.resolve("root/pypi/simple/project");

    assert_eq!(result, Some((0, "simple/project")));
    #[cfg(not(coverage))]
    {
        let stats = region.change();
        assert_eq!((stats.allocations, stats.bytes_allocated), (0, 0));
    }
}
