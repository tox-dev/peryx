use crate::{NamedUpstream, RouteError, UpstreamClient, UpstreamHealth, UpstreamRouter};

fn upstream(name: &str) -> NamedUpstream {
    NamedUpstream::new(
        name,
        UpstreamClient::new(&format!("https://{name}.example/simple/")).unwrap(),
    )
}

fn router() -> UpstreamRouter {
    UpstreamRouter::new(vec![upstream("internal"), upstream("mirror"), upstream("pypi")]).unwrap()
}

fn names(router: &UpstreamRouter, project: &str) -> Vec<String> {
    router
        .candidates(project)
        .map(|upstream| upstream.name().to_owned())
        .collect()
}

#[test]
fn test_upstream_router_preserves_fallback_order() {
    assert_eq!(names(&router(), "demo"), ["internal", "mirror", "pypi"]);
}

#[test]
fn test_upstream_router_disables_repository_fallback() {
    assert_eq!(names(&router().with_fallback(false), "demo"), ["internal"]);
}

#[test]
fn test_upstream_router_protects_one_project_from_fallback() {
    let router = router().protect("private").unwrap();
    assert_eq!(names(&router, "private"), ["internal"]);
    assert_eq!(names(&router, "public"), ["internal", "mirror", "pypi"]);
}

#[test]
fn test_upstream_router_pin_is_strict() {
    let router = router().pin("torch", "mirror").unwrap();
    assert_eq!(names(&router, "torch"), ["mirror"]);
    assert_eq!(names(&router, "numpy"), ["internal", "mirror", "pypi"]);
}

#[test]
fn test_upstream_router_exposes_the_selected_client() {
    let router = router().pin("demo", "mirror").unwrap();
    let selected = router.candidates("demo").next().unwrap();
    assert_eq!(selected.client().redacted_base_url(), "https://mirror.example/simple/");
}

#[test]
fn test_upstream_router_finds_a_source_by_name() {
    let router = router();
    assert_eq!(router.source("mirror").unwrap().name(), "mirror");
    assert!(router.source("missing").is_none());
}

#[test]
fn test_named_upstream_health_is_shared_between_router_clones() {
    let router = router();
    let cloned = router.clone();
    assert_eq!(
        router.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Configured; 3]
    );

    router.sources().next().unwrap().mark_unhealthy();
    cloned.sources().nth(1).unwrap().mark_healthy();

    assert_eq!(
        router.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [
            UpstreamHealth::Unhealthy,
            UpstreamHealth::Healthy,
            UpstreamHealth::Configured
        ]
    );
    assert_eq!(UpstreamHealth::Unhealthy.as_str(), "unhealthy");
}

#[test]
fn test_upstream_router_rejects_no_sources() {
    assert_eq!(UpstreamRouter::new(Vec::new()).unwrap_err(), RouteError::Empty);
}

#[test]
fn test_upstream_router_rejects_an_empty_source_name() {
    let client = UpstreamClient::new("https://example.invalid/simple/").unwrap();
    assert_eq!(
        UpstreamRouter::new(vec![NamedUpstream::new("", client)]).unwrap_err(),
        RouteError::EmptyName
    );
}

#[test]
fn test_upstream_router_rejects_duplicate_source_names() {
    assert_eq!(
        UpstreamRouter::new(vec![upstream("pypi"), upstream("pypi")]).unwrap_err(),
        RouteError::DuplicateName("pypi".to_owned())
    );
}

#[test]
fn test_upstream_router_rejects_an_empty_project_pin() {
    assert_eq!(router().pin("", "pypi").unwrap_err(), RouteError::EmptyProject);
}

#[test]
fn test_upstream_router_rejects_an_empty_protected_project() {
    assert_eq!(router().protect("").unwrap_err(), RouteError::EmptyProject);
}

#[test]
fn test_upstream_router_rejects_an_unknown_pin_source() {
    assert_eq!(
        router().pin("demo", "missing").unwrap_err(),
        RouteError::UnknownPin {
            project: "demo".to_owned(),
            upstream: "missing".to_owned(),
        }
    );
}
