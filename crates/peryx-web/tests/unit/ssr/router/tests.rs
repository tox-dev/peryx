use rstest::rstest;

use super::*;

#[test]
fn ui_state_projects_leptos_options() {
    let directory = tempfile::tempdir().unwrap();
    let state = UiState {
        options: leptos_options(),
        app: Arc::new(AppState::new(
            peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
            peryx_storage::blob::BlobStore::new(directory.path().join("blobs")),
            60,
            Vec::new(),
        )),
    };

    assert_eq!(LeptosOptions::from_ref(&state).site_root, state.options.site_root);
}

#[test]
fn ui_pages_declare_a_descriptor_for_every_route_path() {
    assert_eq!(
        route_descriptors()
            .iter()
            .copied()
            .map(RouteDescriptor::path)
            .collect::<Vec<_>>(),
        crate::ROUTE_PATHS
    );
}

#[test]
fn ui_pages_are_read_only_get_routes() {
    let postures = route_descriptors()
        .iter()
        .map(|descriptor| (descriptor.method(), descriptor.posture()))
        .collect::<Vec<_>>();

    assert_eq!(
        postures,
        vec![(RouteMethod::Get, RoutePosture::Read); crate::ROUTE_PATHS.len()]
    );
}

#[rstest]
#[case::dashboard("/", RouteClass::Admin)]
#[case::admin_status("/admin/status", RouteClass::Admin)]
#[case::admin_topology("/admin/topology", RouteClass::Admin)]
#[case::admin_placements("/admin/placements", RouteClass::Admin)]
#[case::admin_operations("/admin/operations", RouteClass::Admin)]
#[case::admin_policy_decisions("/admin/policy-decisions", RouteClass::Admin)]
#[case::admin_trash("/admin/trash", RouteClass::Admin)]
#[case::admin_analytics("/admin/analytics", RouteClass::Admin)]
#[case::browse("/browse", RouteClass::Listing)]
#[case::search("/search", RouteClass::Listing)]
#[case::stats("/stats", RouteClass::Admin)]
#[case::login("/login", RouteClass::Authentication)]
fn ui_page_shares_the_budget_of_the_work_it_renders(#[case] path: &str, #[case] class: RouteClass) {
    let descriptor = route_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.path() == path)
        .unwrap();

    assert_eq!(descriptor.rate_limit(), RouteRateLimit::Class(class));
}
