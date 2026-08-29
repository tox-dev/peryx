use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash as _, Hasher as _};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use futures_util::stream::{self, Stream};
use peryx_core::{LocalStatus, NodeLiveness, TopologySnapshot, TopologyView};
use peryx_driver::HttpRoutes;
use peryx_driver::ServingStateAvailabilityAuthorizer;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::state::{AppState, ServingState};
use peryx_driver::{RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet};
use peryx_ha::{AvailabilityAudience, AvailabilityAuthorizer as _};

use peryx_http::response_security::ProtectedCachePolicy;

const TOPOLOGY_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

const TOPOLOGY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub struct DistributedHttpRoutes;

impl HttpRoutes for DistributedHttpRoutes {
    fn routes(&self) -> RouteSet {
        RouteSet::new()
            .route(
                read("/+analytics/completeness"),
                get(crate::completeness_http::analytics_completeness),
            )
            .route(read("/+availability/topology"), get(availability_topology))
            .route(
                read("/+availability/topology/stream"),
                get(availability_topology_stream),
            )
            .route(
                read("/+availability/operations"),
                get(crate::operations_http::operations),
            )
            .route(
                read("/+availability/placements"),
                get(crate::placements_http::placements),
            )
            .route(
                read("/+availability/placements/{digest}"),
                get(crate::placements_http::blob_placements),
            )
    }
}

const fn read(path: &'static str) -> RouteDescriptor {
    RouteDescriptor::new(
        RouteMethod::Get,
        path,
        RoutePosture::Read,
        RouteRateLimit::Class(RouteClass::Admin),
    )
}

async fn availability_topology(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let view = topology_view(availability_audience(state.serving.clone(), &headers).await);
    let local = local_status(&state.serving).await;
    let snapshot = state
        .serving
        .availability_topology()
        .snapshot(view, local, (state.serving.clock)());
    let mut response = axum::Json(snapshot).into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn availability_topology_stream(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let view = topology_view(availability_audience(state.serving.clone(), &headers).await);
    let sse = Sse::new(topology_events(state.serving.clone(), view))
        .keep_alive(KeepAlive::new().interval(TOPOLOGY_HEARTBEAT_INTERVAL).text("heartbeat"));
    let mut response = sse.into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn topology_events(state: Arc<ServingState>, view: TopologyView) -> impl Stream<Item = Result<Event, Infallible>> {
    let interval = tokio::time::interval(TOPOLOGY_SAMPLE_INTERVAL);
    stream::unfold(
        (state, view, interval, None::<u64>, 0_u64),
        |(state, view, mut interval, mut last, mut sequence)| async move {
            let event = loop {
                interval.tick().await;
                let local = local_status(&state).await;
                let snapshot = state.availability_topology().snapshot(view, local, (state.clock)());
                let version = state_version(&snapshot);
                if last != Some(version) {
                    last = Some(version);
                    sequence += 1;
                    break Event::default()
                        .id(sequence.to_string())
                        .event("topology")
                        .data(serde_json::to_string(&snapshot).unwrap_or_default());
                }
            };
            Some((Ok(event), (state, view, interval, last, sequence)))
        },
    )
}

fn state_version(snapshot: &TopologySnapshot) -> u64 {
    let mut probe = snapshot.clone();
    probe.captured_at = 0;
    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(&probe).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

const fn topology_view(audience: AvailabilityAudience) -> TopologyView {
    match audience {
        AvailabilityAudience::Administrator => TopologyView::Administrator,
        AvailabilityAudience::Operator => TopologyView::Operator,
        AvailabilityAudience::Public => TopologyView::Public,
    }
}

pub async fn availability_audience(state: Arc<ServingState>, headers: &HeaderMap) -> AvailabilityAudience {
    ServingStateAvailabilityAuthorizer::new(state)
        .authorize(headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()))
        .await
}

async fn local_status(state: &ServingState) -> LocalStatus {
    let serial = state.meta.current_serial();
    let serving = serial.is_ok() && state.blobs.health().await.is_ok();
    LocalStatus {
        role: state.availability_role(),
        liveness: if serving {
            NodeLiveness::Live
        } else {
            NodeLiveness::Unready
        },
        frontier: serial.unwrap_or(0),
    }
}
