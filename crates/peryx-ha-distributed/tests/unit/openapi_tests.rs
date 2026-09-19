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

#[test]
fn availability_paths_document_each_operation_rather_than_a_blank_default() {
    let paths = super::availability_paths(PathsBuilder::new()).build().paths;
    let summary = |path: &str| paths[path].get.as_ref().unwrap().summary.clone();

    assert_eq!(
        summary("/+availability/topology"),
        Some("Availability topology snapshot".to_owned())
    );
    assert_eq!(
        summary("/+availability/topology/stream"),
        Some("Availability topology stream".to_owned())
    );
    assert_eq!(
        summary("/+availability/placements/{digest}"),
        Some("Blob placement across datacenters".to_owned())
    );
    assert_eq!(
        summary("/+availability/operations"),
        Some("Pending operations health".to_owned())
    );
    assert_eq!(
        summary("/+availability/placements"),
        Some("Artifact placement health".to_owned())
    );
}
