use utoipa::openapi::PathsBuilder;

#[test]
fn availability_paths_register_the_distributed_surface() {
    let paths = super::availability_paths(PathsBuilder::new()).build().paths;
    assert_eq!(
        paths.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "/+analytics/completeness",
            "/+availability/operations",
            "/+availability/placements",
            "/+availability/placements/{digest}",
            "/+availability/topology",
            "/+availability/topology/stream",
        ]
    );
}
