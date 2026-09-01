use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode, header};
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
use tokio::sync::watch;

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
            .with_extension(Arc::new(TopologySampler::default()))
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
    let view = match availability_audience(state.serving.clone(), &headers).await {
        Ok(audience) => topology_view(audience),
        Err(_) => return AvailabilityRejection::response(),
    };
    let local = local_status(&state.serving).await;
    let snapshot = state
        .serving
        .availability_topology()
        .snapshot(view, local, (state.serving.clock)());
    let mut response = axum::Json(snapshot).into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn availability_topology_stream(
    State(state): State<Arc<AppState>>,
    Extension(sampler): Extension<Arc<TopologySampler>>,
    headers: HeaderMap,
) -> Response {
    let view = match availability_audience(state.serving.clone(), &headers).await {
        Ok(audience) => topology_view(audience),
        Err(_) => return AvailabilityRejection::response(),
    };
    let sse = Sse::new(topology_events(sampler.subscribe(state.serving.clone(), view)))
        .keep_alive(KeepAlive::new().interval(TOPOLOGY_HEARTBEAT_INTERVAL).text("heartbeat"));
    let mut response = sse.into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn topology_events(subscription: TopologySubscription) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold((subscription, 0_u64), |(mut subscription, mut sequence)| async move {
        let snapshot = subscription.next_snapshot().await;
        sequence += 1;
        let event = Event::default()
            .id(sequence.to_string())
            .event("topology")
            .data(serde_json::to_string(snapshot.as_ref()).unwrap_or_default());
        Some((Ok(event), (subscription, sequence)))
    })
}

#[derive(Default)]
struct TopologySampler {
    shared: Mutex<SamplerState>,
}

impl TopologySampler {
    fn subscribe(self: &Arc<Self>, state: Arc<ServingState>, view: TopologyView) -> TopologySubscription {
        let receiver = {
            let mut shared = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.subscribers += 1;
            if let Some(running) = &shared.running {
                running.samples.subscribe()
            } else {
                let (samples, receiver) = watch::channel(None);
                let task = tokio::spawn(sample_topology(state, samples.clone()));
                shared.running = Some(RunningSampler { samples, task });
                receiver
            }
        };
        TopologySubscription {
            sampler: self.clone(),
            view,
            receiver,
            initial: true,
            last_version: None,
        }
    }
}

struct TopologySubscription {
    sampler: Arc<TopologySampler>,
    view: TopologyView,
    receiver: watch::Receiver<Option<TopologySample>>,
    initial: bool,
    last_version: Option<u64>,
}

impl TopologySubscription {
    async fn next_snapshot(&mut self) -> Arc<TopologySnapshot> {
        loop {
            let projection = self.next_projection().await;
            if self.last_version != Some(projection.version) {
                self.last_version = Some(projection.version);
                return projection.snapshot;
            }
        }
    }

    async fn next_projection(&mut self) -> TopologyProjection {
        if self.initial {
            self.initial = false;
            if let Some(projection) = self.current_projection() {
                return projection;
            }
        }
        self.receiver
            .changed()
            .await
            .expect("a subscription retains its sampler");
        self.current_projection()
            .expect("the sampler publishes before notifying subscribers")
    }

    fn current_projection(&mut self) -> Option<TopologyProjection> {
        self.receiver
            .borrow_and_update()
            .as_ref()
            .map(|sample| sample.projection(self.view).clone())
    }
}

impl Drop for TopologySubscription {
    fn drop(&mut self) {
        let task = {
            let mut shared = self
                .sampler
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.subscribers -= 1;
            if shared.subscribers == 0 {
                Some(shared.running.take().expect("a subscriber has a running sampler").task)
            } else {
                None
            }
        };
        if let Some(task) = task {
            task.abort();
        }
    }
}

#[derive(Default)]
struct SamplerState {
    subscribers: usize,
    running: Option<RunningSampler>,
}

struct RunningSampler {
    samples: watch::Sender<Option<TopologySample>>,
    task: tokio::task::JoinHandle<()>,
}

struct TopologySample {
    projections: [TopologyProjection; 3],
}

impl TopologySample {
    const fn projection(&self, view: TopologyView) -> &TopologyProjection {
        &self.projections[match view {
            TopologyView::Public => 0,
            TopologyView::Operator => 1,
            TopologyView::Administrator => 2,
        }]
    }
}

#[derive(Clone)]
struct TopologyProjection {
    snapshot: Arc<TopologySnapshot>,
    version: u64,
}

async fn sample_topology(state: Arc<ServingState>, samples: watch::Sender<Option<TopologySample>>) {
    let mut interval = tokio::time::interval(TOPOLOGY_SAMPLE_INTERVAL);
    loop {
        interval.tick().await;
        let local = local_status(&state).await;
        let captured_at = (state.clock)();
        let topology = state.availability_topology();
        samples.send_replace(Some(TopologySample {
            projections: {
                let previous = samples.borrow();
                [
                    TopologyView::Public,
                    TopologyView::Operator,
                    TopologyView::Administrator,
                ]
                .map(|view| {
                    TopologyProjection::new(
                        topology.snapshot(view, local, captured_at),
                        previous.as_ref().map(|sample| sample.projection(view)),
                    )
                })
            },
        }));
    }
}

impl TopologyProjection {
    fn new(snapshot: TopologySnapshot, previous: Option<&Self>) -> Self {
        let version = match previous {
            Some(previous) if same_topology_state(&previous.snapshot, &snapshot) => previous.version,
            Some(previous) => previous.version + 1,
            None => 1,
        };
        Self {
            snapshot: Arc::new(snapshot),
            version,
        }
    }
}

fn same_topology_state(left: &TopologySnapshot, right: &TopologySnapshot) -> bool {
    left.mode == right.mode
        && left.group == right.group
        && left.node_count == right.node_count
        && left.local == right.local
        && left.nodes == right.nodes
}

const fn topology_view(audience: AvailabilityAudience) -> TopologyView {
    match audience {
        AvailabilityAudience::Administrator => TopologyView::Administrator,
        AvailabilityAudience::Operator => TopologyView::Operator,
        AvailabilityAudience::Public => TopologyView::Public,
    }
}

pub async fn availability_audience(
    state: Arc<ServingState>,
    headers: &HeaderMap,
) -> Result<AvailabilityAudience, AvailabilityRejection> {
    ServingStateAvailabilityAuthorizer::new(state)
        .authorize(headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()))
        .await
        .map_err(|_| AvailabilityRejection)
}

pub struct AvailabilityRejection;

impl AvailabilityRejection {
    pub fn response() -> Response {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "identity service unavailable"})),
        )
            .into_response();
        ProtectedCachePolicy::NoStore.apply(response.headers_mut());
        response
    }
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
