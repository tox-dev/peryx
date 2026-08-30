#![cfg(feature = "ssr")]

use crate as peryx_web;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use leptos::prelude::*;
use peryx_core::{
    BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection, Ecosystem, Role as IndexRole,
};
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, BrowseDriver, BrowseRequest, EcosystemDriver, IndexSummary, IndexSummaryDriver,
    IndexSummaryError, MetricsDriver, RecentWrite,
};
use peryx_driver::state::{AppState, Index, IndexDescription, IndexKind, describe_index};
use peryx_events::metrics::{MetricFamily, MetricKind};
use peryx_identity::{
    Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, Role, SESSION_COOKIE, ServerUser, SessionSealer,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_test_support::EcosystemDriverFixture;
use tower::ServiceExt as _;

const FIXTURE_ECOSYSTEM: Ecosystem = Ecosystem::new("fixture");
const QUERY: &str = "index=fixture&opaque=segment%2Fvalue%3Adetail";
const PASSWORD: &str = "Correct-Horse-Battery-Staple-42!";
const ADMIN_AUTHORIZATION: &str = "Basic QWxpY2U6Q29ycmVjdC1Ib3JzZS1CYXR0ZXJ5LVN0YXBsZS00MiE=";
const OPERATOR_AUTHORIZATION: &str = "Basic T2xpdmlhOkNvcnJlY3QtSG9yc2UtQmF0dGVyeS1TdGFwbGUtNDIh";
const INDEX_AUTHORIZATION: &str = "Basic cHVibGlzaGVyOnVwbG9hZC1zZWNyZXQ=";
const SESSION_KEY: &[u8] = b"a-token-realm-signing-secret-here";
const FIXTURE_METRIC: MetricFamily = MetricFamily {
    key: "fixture",
    prom_name: "peryx_fixture_total",
    help: "Fixture events.",
    ui_label: "Fixture events",
    roles: &[IndexRole::Hosted],
    json_name: None,
    kind: MetricKind::Counter,
};
static BARE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(FIXTURE_ECOSYSTEM, RouteClass::Listing);

#[tokio::test]
async fn server_render_reports_password_overload() {
    let (_directory, mut app) = state(Vec::new());
    let meta = app.serving.meta.clone();
    Arc::get_mut(&mut app.serving).unwrap().users = peryx_driver::users::UserService::with_password_settings(
        meta,
        peryx_identity::PasswordPolicy::new(8, 1, 1).unwrap(),
        0,
    );

    let (status, headers, _) = render(
        Arc::new(app),
        "/",
        &[(header::AUTHORIZATION.as_str(), "Basic QWxpY2U6cGFzc3dvcmQ=")],
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn neutral_browse_contract_forwards_raw_query() {
    let body = render_browse(Some(fixture_page())).await;

    assert!(body.contains("Fixture object"), "{body}");
    assert!(body.contains("Neutral browse response"), "{body}");
}

#[tokio::test]
async fn neutral_browse_contract_renders_no_match() {
    let body = render_browse(None).await;

    assert!(body.contains("Nothing matched this browse query."), "{body}");
    assert!(!body.contains("Fixture object"), "{body}");
}

#[tokio::test]
async fn browse_contract_renders_unsafe_link_destinations_as_text() {
    let body = render_browse(Some(BrowsePage {
        title: "Unsafe links".to_owned(),
        breadcrumbs: vec![BrowseLink {
            label: "Breadcrumb".to_owned(),
            href: "javascript:alert(1)".to_owned(),
        }],
        sections: vec![
            BrowseSection::Properties {
                heading: "Properties".to_owned(),
                entries: vec![BrowseProperty {
                    label: "Homepage".to_owned(),
                    value: "Property".to_owned(),
                    href: Some("data:text/html,<script>".to_owned()),
                }],
            },
            BrowseSection::Links {
                heading: "Links".to_owned(),
                entries: vec![
                    BrowseLink {
                        label: "Related".to_owned(),
                        href: "javascript:alert(2)".to_owned(),
                    },
                    BrowseLink {
                        label: "Whitespace".to_owned(),
                        href: " javascript:alert(4)".to_owned(),
                    },
                    BrowseLink {
                        label: "Control".to_owned(),
                        href: "relative\u{7f}path".to_owned(),
                    },
                    BrowseLink {
                        label: "File".to_owned(),
                        href: "file:///tmp/unsafe".to_owned(),
                    },
                ],
                empty: String::new(),
            },
            BrowseSection::Table {
                heading: "Table".to_owned(),
                columns: vec!["Value".to_owned()],
                rows: vec![BrowseRow {
                    cells: vec![BrowseCell {
                        text: "Cell".to_owned(),
                        href: Some("data:text/plain,unsafe".to_owned()),
                        code: false,
                    }],
                    badges: Vec::new(),
                    actions: Vec::new(),
                }],
                empty: String::new(),
            },
            BrowseSection::Content {
                heading: "Content".to_owned(),
                text: String::new(),
                size: None,
                offset: 0,
                next: Some(BrowseLink {
                    label: "Next".to_owned(),
                    href: "javascript:alert(3)".to_owned(),
                }),
            },
        ],
        ..BrowsePage::default()
    }))
    .await;
    let body = rendered_main(&body);

    for text in [
        "Breadcrumb",
        "Property",
        "Related",
        "Whitespace",
        "Control",
        "File",
        "Cell",
        "Next",
    ] {
        assert!(body.contains(text), "missing {text:?} in {body}");
    }
    for destination in ["javascript:", "data:", "file:"] {
        assert!(!body.contains(destination), "unexpected {destination:?} in {body}");
    }
    assert!(body.contains("<span>Control</span>"), "{body}");
}

#[tokio::test]
async fn browse_contract_preserves_allowed_link_destinations() {
    let body = render_browse(Some(BrowsePage {
        title: "Allowed links".to_owned(),
        breadcrumbs: [
            ("HTTP", "http://example.test/x"),
            ("HTTPS", "https://example.test/x"),
            ("Mail", "mailto:owner@example.test"),
            ("Path", "/browse?index=fixture"),
            ("Query", "?index=fixture"),
            ("Fragment", "#files"),
            ("Relative", "artifact/file"),
            ("Network", "//example.test/x"),
        ]
        .into_iter()
        .map(|(label, href)| BrowseLink {
            label: label.to_owned(),
            href: href.to_owned(),
        })
        .collect(),
        ..BrowsePage::default()
    }))
    .await;
    let body = rendered_main(&body);

    for href in [
        "mailto:owner@example.test",
        "/browse?index=fixture",
        "?index=fixture",
        "#files",
        "artifact/file",
    ] {
        assert!(
            body.contains(&format!(r#"href="{href}""#)),
            "missing {href:?} in {body}"
        );
    }
    for href in ["http://example.test/x", "https://example.test/x", "//example.test/x"] {
        assert!(
            body.contains(&format!(r#"href="{href}" rel="external nofollow noopener noreferrer""#)),
            "missing external {href:?} in {body}"
        );
    }
}

#[tokio::test]
async fn browse_contract_reports_resolution_and_capability_errors() {
    for (indexes, driver, expected) in [
        (Vec::new(), false, "not configured"),
        (vec![fixture_index()], false, "no ecosystem driver"),
        (vec![fixture_index()], true, "does not support browsing"),
    ] {
        let (_directory, mut app) = state(indexes);
        if driver {
            app.register_driver(Arc::new(EcosystemDriverFixture::new(
                FIXTURE_ECOSYSTEM,
                RouteClass::Listing,
            )));
        }
        let (_, _, body) = render(Arc::new(app), &format!("/browse?{QUERY}"), &[]).await;
        assert!(body.contains(expected), "expected {expected:?} in {body}");
    }

    let (_directory, mut app) = state(vec![private_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: Some(fixture_page()),
            summary_error: None,
        },
    );
    let app = Arc::new(app);
    let (_, _, denied) = render(app.clone(), &format!("/browse?{QUERY}"), &[]).await;
    assert!(denied.contains("read access denied"), "{denied}");
    let (_, _, allowed) = render(
        app,
        &format!("/browse?{QUERY}"),
        &[(header::AUTHORIZATION.as_str(), INDEX_AUTHORIZATION)],
    )
    .await;
    assert!(allowed.contains("Fixture object"), "{allowed}");
}

#[tokio::test]
async fn private_search_without_request_context_reports_header_extraction() {
    let (_directory, mut app) = state(vec![private_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: None,
            summary_error: None,
        },
    );
    let owner = Owner::new();
    owner.set();
    provide_context(Arc::new(app));

    let error = peryx_web::ssr::browse(QUERY).await.unwrap_err();
    assert!(error.starts_with("request headers: "), "{error}");
    let error = peryx_web::ssr::search("", "all", "all", 1, 25).await.unwrap_err();
    assert!(error.starts_with("request headers: "), "{error}");
}

#[tokio::test]
async fn search_contract_normalizes_options_and_reports_invalid_regex() {
    let (_directory, app) = state(Vec::new());
    let owner = Owner::new();
    owner.set();
    provide_context(Arc::new(app));

    assert_eq!(
        peryx_web::ssr::search("", "invalid", "invalid", 0, 7).await,
        Ok(peryx_web::model::UiSearchPage {
            source_type: "all".to_owned(),
            availability: "all".to_owned(),
            page: 1,
            page_size: 25,
            ..Default::default()
        })
    );
    let error = peryx_web::ssr::search("re:[", "all", "all", 1, 25).await.unwrap_err();
    assert!(error.starts_with("artifact search: "), "{error}");
}

#[tokio::test]
async fn search_contract_applies_private_index_access() {
    let (_directory, app) = state(vec![private_index()]);
    let (_, _, body) = render(
        Arc::new(app),
        "/search?q=&page_size=25",
        &[(header::AUTHORIZATION.as_str(), INDEX_AUTHORIZATION)],
    )
    .await;

    assert!(body.contains("Nothing indexed yet."), "{body}");
}

#[tokio::test]
async fn status_contract_applies_public_operator_and_administrator_views() {
    let (_directory_without_driver, app_without_driver) = state(vec![fixture_index()]);
    let (_, _, body) = render(Arc::new(app_without_driver), "/", &[]).await;
    assert!(body.contains("/fixture/"), "{body}");

    let (_directory, mut app) = state(vec![fixture_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: None,
            summary_error: None,
        },
    );
    app.register_client_discovery(FIXTURE_ECOSYSTEM, &BARE_DRIVER);
    app.serving.requests.store(7, Ordering::Relaxed);
    add_user(&app, "Alice", Role::Administrator).await;
    add_user(&app, "Olivia", Role::Operator).await;
    let app = Arc::new(app);

    for (uri, authorization, expected) in [
        ("/", None, "<strong>0</strong><span>accepted requests</span>"),
        (
            "/",
            Some(OPERATOR_AUTHORIZATION),
            "<strong>7</strong><span>accepted requests</span>",
        ),
        ("/admin/status", Some(ADMIN_AUTHORIZATION), "fixture-1.bin"),
    ] {
        let headers = authorization.map_or_else(Vec::new, |value| vec![(header::AUTHORIZATION.as_str(), value)]);
        let (_, _, body) = render(app.clone(), uri, &headers).await;
        assert!(body.contains(expected), "expected {expected:?} in {body}");
    }

    let (_, _, body) = render(
        app,
        "/admin/status",
        &[(header::AUTHORIZATION.as_str(), ADMIN_AUTHORIZATION)],
    )
    .await;
    for expected in ["fixture-1.bin", "token configured", "&lt;redacted&gt;"] {
        assert!(body.contains(expected), "expected {expected:?} in {body}");
    }

    let (_directory, mut app) = state(vec![fixture_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: None,
            summary_error: Some(IndexSummaryError::Storage),
        },
    );
    add_user(&app, "Alice", Role::Administrator).await;
    let (_, _, body) = render(
        Arc::new(app),
        "/admin/status",
        &[(header::AUTHORIZATION.as_str(), ADMIN_AUTHORIZATION)],
    )
    .await;
    assert_eq!(body.matches(">unavailable<").count(), 4, "{body}");
}

#[tokio::test]
async fn login_contract_reads_signed_session_cookie() {
    let (_directory, mut app) = state(Vec::new());
    let user = app.serving.users.create("Ada Lovelace").unwrap();
    app.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    let cookie = format!(
        "{SESSION_COOKIE}={}",
        SessionSealer::new(SESSION_KEY).seal_session(&user, 4_102_444_800)
    );
    let app = Arc::new(app);
    let (_, _, anonymous) = render(app.clone(), "/login", &[]).await;
    assert!(anonymous.contains("No login providers are configured."), "{anonymous}");
    let (_, _, body) = render(app, "/login", &[(header::COOKIE.as_str(), cookie.as_str())]).await;

    assert!(body.contains("Signed in as"), "{body}");
    assert!(body.contains("Ada Lovelace"), "{body}");
}

#[tokio::test]
async fn router_contract_serves_favicon() {
    let (_directory, app) = state(Vec::new());
    let (status, headers, body) = render(Arc::new(app), "/favicon.svg", &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml");
    assert_eq!(body, include_str!("../../../../site/static/icon.svg"));
}

#[tokio::test]
async fn router_contract_serves_brand_mark() {
    let (_directory, app) = state(Vec::new());
    let (status, headers, body) = render(Arc::new(app), "/mark.svg", &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml");
    assert_eq!(body, include_str!("../../../../site/static/mark.svg"));
}

#[tokio::test]
async fn router_contract_rejects_unknown_paths() {
    let (_directory, app) = state(Vec::new());
    let (status, _, _) = render(Arc::new(app), "/missing", &[]).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn contract_driver_returns_declared_values() {
    let expected_page = Some(fixture_page());
    let driver = ContractDriver {
        browse_response: expected_page.clone(),
        summary_error: None,
    };

    assert_eq!(driver.ecosystem(), FIXTURE_ECOSYSTEM);
    assert_eq!(
        AbsoluteProtocolDriver::classify_route(&BARE_DRIVER, "/fixture/object"),
        RouteClass::Listing
    );
    let index = fixture_index();
    assert_eq!(
        peryx_driver::serving::ClientDiscovery::discover_index(
            &BARE_DRIVER,
            describe_index(std::slice::from_ref(&index), 0),
            None,
        ),
        serde_json::json!({
            "name": "fixture",
            "route": "fixture",
            "kind": "hosted",
            "ecosystem": "fixture",
        })
    );
    assert_eq!(
        (
            peryx_driver::serving::ClientDiscovery::discover_index(
                &driver,
                describe_index(std::slice::from_ref(&index), 0),
                None,
            ),
            peryx_driver::serving::ClientDiscovery::client_endpoint(&driver, "fixture"),
        ),
        (
            serde_json::json!({
                "name": "fixture",
                "route": "fixture",
                "kind": "hosted",
                "ecosystem": "fixture",
            }),
            "/fixture/".to_owned(),
        )
    );
    let (_directory, app) = state(vec![index]);
    let access = peryx_driver::access::ReadAccess::from_headers(&app.serving, &HeaderMap::new());
    assert_eq!(
        BrowseDriver::browse(
            &driver,
            BrowseRequest {
                state: app.serving.clone(),
                position: 0,
                raw_query: QUERY.to_owned(),
                access: &access,
                base: None,
            },
        )
        .await,
        Ok(expected_page)
    );
}

#[tokio::test]
async fn availability_views_apply_operator_and_administrator_access() {
    let (_directory, app) = state(Vec::new());
    add_user(&app, "Alice", Role::Administrator).await;
    add_user(&app, "Olivia", Role::Operator).await;
    let app = Arc::new(app);

    for (authorization, rows) in [(OPERATOR_AUTHORIZATION, false), (ADMIN_AUTHORIZATION, true)] {
        let owner = Owner::new();
        owner.set();
        provide_context(app.clone());
        let (parts, ()) = Request::builder()
            .header(header::AUTHORIZATION, authorization)
            .body(())
            .unwrap()
            .into_parts();
        provide_context(parts);

        assert_eq!(peryx_web::ssr::operations().await.unwrap().rows.is_some(), rows);
        assert_eq!(peryx_web::ssr::placements().await.unwrap().rows.is_some(), rows);
    }

    for uri in ["/admin/operations", "/admin/placements"] {
        let (_, _, body) = render(
            app.clone(),
            uri,
            &[(header::AUTHORIZATION.as_str(), ADMIN_AUTHORIZATION)],
        )
        .await;
        assert!(!body.contains("You do not have access"), "{body}");
    }

    let owner = Owner::new();
    owner.set();
    provide_context(app.clone());
    let (parts, ()) = Request::builder()
        .header(header::AUTHORIZATION, ADMIN_AUTHORIZATION)
        .body(())
        .unwrap()
        .into_parts();
    provide_context(parts);
    let digest = "sha256:abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
    assert_eq!(
        peryx_web::data::load_blob_placement(digest.to_owned())
            .await
            .unwrap()
            .digest,
        digest
    );
}

async fn render_browse(browse_response: Option<BrowsePage>) -> String {
    let (_directory, mut app) = state(vec![fixture_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response,
            summary_error: None,
        },
    );
    let (status, _, body) = render(Arc::new(app), &format!("/browse?{QUERY}"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    body
}

fn rendered_main(body: &str) -> &str {
    body.split_once("<main>").unwrap().1.split_once("</main>").unwrap().0
}

async fn render(app: Arc<AppState>, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = peryx_web::ssr::ui_router(app)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    (status, headers, body)
}

async fn add_user(app: &AppState, name: &str, role: Role) {
    let user = app.serving.users.create(name).unwrap();
    app.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    app.serving
        .authorization
        .grant(&user.id, role, GrantScope::Server)
        .unwrap();
}

fn state(indexes: Vec<Index>) -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    (directory, AppState::new(meta, blobs, 60, indexes))
}

fn fixture_page() -> BrowsePage {
    BrowsePage {
        title: "Fixture object".to_owned(),
        summary: Some("Neutral browse response".to_owned()),
        ..BrowsePage::default()
    }
}

fn fixture_index() -> Index {
    Index {
        name: "fixture".to_owned(),
        route: "fixture".to_owned(),
        ecosystem: FIXTURE_ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "publisher".to_owned(),
                secret: "upload-secret".to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Read, Action::Write]),
                }],
                expires_at: None,
            }],
        },
    }
}

fn private_index() -> Index {
    let mut index = fixture_index();
    index.acl.anonymous_read = false;
    index
}

struct ContractDriver {
    browse_response: Option<BrowsePage>,
    summary_error: Option<IndexSummaryError>,
}

#[async_trait]
impl BrowseDriver for ContractDriver {
    async fn browse(
        &self,
        request: BrowseRequest<'_>,
    ) -> Result<Option<BrowsePage>, peryx_driver::serving::BrowseError> {
        let BrowseRequest {
            state,
            position,
            raw_query,
            access,
            ..
        } = request;
        assert_eq!((position, raw_query.as_str()), (0, QUERY));
        access.for_index(state.index_at(position)).authorize_any_resource()?;
        Ok(self.browse_response.clone())
    }
}

impl EcosystemDriver for ContractDriver {
    fn ecosystem(&self) -> Ecosystem {
        FIXTURE_ECOSYSTEM
    }
}

fn register_contract_driver(app: &mut AppState, driver: ContractDriver) {
    let driver = Arc::new(driver);
    app.register_driver(driver.clone());
    app.register_capabilities(|capabilities| {
        capabilities.register_browse(FIXTURE_ECOSYSTEM, driver.clone());
        capabilities.register_index_summary(FIXTURE_ECOSYSTEM, driver.clone());
        capabilities.register_metrics(FIXTURE_ECOSYSTEM, driver);
    });
}

impl MetricsDriver for ContractDriver {
    fn metric_families(&self) -> &'static [MetricFamily] {
        &[FIXTURE_METRIC]
    }
}

impl peryx_driver::serving::ClientDiscovery for ContractDriver {
    fn discover_index(
        &self,
        index: IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        peryx_driver::discovery::minimal_entry(&index)
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/{route}/")
    }
}

impl IndexSummaryDriver for ContractDriver {
    fn summarize_indexes(
        &self,
        _meta: &MetaStore,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<HashMap<String, IndexSummary>, IndexSummaryError> {
        if let Some(error) = self.summary_error {
            return Err(error);
        }
        Ok(index_names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    IndexSummary {
                        resource_count: 3,
                        write_count: 2,
                        recent_writes: vec![RecentWrite {
                            resource: "fixture".to_owned(),
                            artifact: "fixture-1.bin".to_owned(),
                            group: "release".to_owned(),
                            written_at: Some("2026-08-10T00:00:00Z".to_owned()),
                            size: Some(41),
                        }]
                        .into_iter()
                        .take(recent_limit)
                        .collect(),
                    },
                )
            })
            .collect())
    }
}

#[tokio::test]
async fn browse_contract_authorizes_a_private_index_through_a_browser_session() {
    let (_directory, mut app) = state(vec![private_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: Some(fixture_page()),
            summary_error: None,
        },
    );
    app.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    let reader = app.serving.users.create("Rita").unwrap();
    app.serving
        .authorization
        .grant(
            &reader.id,
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "fixture".to_owned(),
            },
        )
        .unwrap();
    let stranger = app.serving.users.create("Sam").unwrap();
    let app = Arc::new(app);

    let (_, _, allowed) = render(
        app.clone(),
        &format!("/browse?{QUERY}"),
        &[(header::COOKIE.as_str(), session_cookie(&reader).as_str())],
    )
    .await;
    let (_, _, denied) = render(
        app,
        &format!("/browse?{QUERY}"),
        &[(header::COOKIE.as_str(), session_cookie(&stranger).as_str())],
    )
    .await;

    assert!(allowed.contains("Fixture object"), "{allowed}");
    assert!(denied.contains("read access denied"), "{denied}");
}

#[tokio::test]
async fn browse_contract_drops_a_browser_session_once_the_grant_is_revoked() {
    let (_directory, mut app) = state(vec![private_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: Some(fixture_page()),
            summary_error: None,
        },
    );
    app.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    let reader = app.serving.users.create("Rita").unwrap();
    let scope = GrantScope::Repository {
        name: "fixture".to_owned(),
    };
    app.serving
        .authorization
        .grant(&reader.id, Role::RepositoryReader, scope.clone())
        .unwrap();
    let app = Arc::new(app);
    let (_, _, allowed) = render(
        app.clone(),
        &format!("/browse?{QUERY}"),
        &[(header::COOKIE.as_str(), session_cookie(&reader).as_str())],
    )
    .await;

    app.serving
        .authorization
        .revoke(&reader.id, Role::RepositoryReader, &scope)
        .unwrap();
    let (_, _, denied) = render(
        app,
        &format!("/browse?{QUERY}"),
        &[(header::COOKIE.as_str(), session_cookie(&reader).as_str())],
    )
    .await;

    assert!(allowed.contains("Fixture object"), "{allowed}");
    assert!(denied.contains("read access denied"), "{denied}");
}

#[tokio::test]
async fn status_contract_applies_the_operator_view_to_a_browser_session() {
    let (_directory, mut app) = state(vec![fixture_index()]);
    register_contract_driver(
        &mut app,
        ContractDriver {
            browse_response: None,
            summary_error: None,
        },
    );
    app.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    app.serving.requests.store(7, Ordering::Relaxed);
    add_user(&app, "Olivia", Role::Operator).await;
    let operator = app.serving.users.identify("Olivia").unwrap().unwrap();
    let visitor = app.serving.users.create("Sam").unwrap();
    let app = Arc::new(app);

    let (_, _, signed_in) = render(
        app.clone(),
        "/",
        &[(header::COOKIE.as_str(), session_cookie(&operator).as_str())],
    )
    .await;
    let (_, _, visiting) = render(
        app,
        "/",
        &[(header::COOKIE.as_str(), session_cookie(&visitor).as_str())],
    )
    .await;

    assert!(
        signed_in.contains("<strong>7</strong><span>accepted requests</span>"),
        "{signed_in}"
    );
    assert!(
        visiting.contains("<strong>0</strong><span>accepted requests</span>"),
        "{visiting}"
    );
}

fn session_cookie(user: &ServerUser) -> String {
    format!(
        "{SESSION_COOKIE}={}",
        SessionSealer::new(SESSION_KEY).seal_session(user, 4_102_444_800)
    )
}
