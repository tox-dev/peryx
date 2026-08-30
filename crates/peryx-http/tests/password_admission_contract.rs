use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier as ThreadBarrier};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::state::AppState;
use peryx_driver::users::{AuthenticationError, EnrollError, PasswordDerivationError, UserService};
use peryx_identity::{PasswordPolicy, UserId};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tokio::sync::Notify;
use tower::ServiceExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

const ACTIVE_DERIVATIONS: usize = 4;
const PASSWORD: &str = "correct horse";

#[test]
fn password_admission_bounds_running_and_queued_derivations() {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let weak = UserService::with_password_settings(store.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 4);
    let legacy = weak.create("Stale").unwrap();
    runtime.block_on(weak.set_password(&legacy.id, PASSWORD)).unwrap();
    let service = UserService::with_password_settings(store.clone(), PasswordPolicy::new(16, 1, 1).unwrap(), 4);
    let real = service.create("Real").unwrap();
    runtime.block_on(service.set_password(&real.id, PASSWORD)).unwrap();
    let enrollment = service.create("Enrollment").unwrap();
    let active = (0..ACTIVE_DERIVATIONS)
        .map(|index| service.create(&format!("Active {index}")))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut state = AppState::new(store, BlobStore::new(directory.path().join("blobs")), 60, Vec::new());
    Arc::get_mut(&mut state.serving).unwrap().users = service.clone();
    let router = peryx_http::router(Arc::new(state));
    let gate = DerivationGate::new();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(gate.clone())).unwrap();

    runtime.block_on(async move {
        let mut active = active
            .into_iter()
            .map(|user| {
                let service = service.clone();
                tokio::spawn(async move { service.set_password(&user.id, PASSWORD).await })
            })
            .collect::<Vec<_>>();
        gate.wait_started(4).await;

        let real_check = spawn_authentication(&service, "Real");
        let decoy_check = spawn_authentication(&service, "Unknown");
        let queued_enrollment = {
            let service = service.clone();
            let id = enrollment.id.clone();
            tokio::spawn(async move { service.set_password(&id, PASSWORD).await })
        };
        let legacy_check = spawn_authentication(&service, "Stale");
        gate.wait_admitted(8).await;
        gate.wait_verifier_reads(2).await;

        let canceled_active = active.pop().unwrap();
        canceled_active.abort();
        assert!(canceled_active.await.unwrap_err().is_cancelled());

        assert_eq!(status(&router, Some("Real")).await, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status(&router, Some("Unknown")).await, StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(
            service.set_password(&enrollment.id, PASSWORD).await,
            Err(EnrollError::Derivation(PasswordDerivationError::Overloaded))
        ));
        assert_eq!(status(&router, None).await, StatusCode::OK);
        assert_eq!(gate.counts(), (8, 4, 2));

        queued_enrollment.abort();
        assert!(queued_enrollment.await.unwrap_err().is_cancelled());
        let replacement = {
            let service = service.clone();
            let id = enrollment.id;
            tokio::spawn(async move { service.set_password(&id, PASSWORD).await })
        };
        gate.wait_admitted(9).await;
        assert_eq!(gate.counts(), (9, 4, 2));
        assert!(matches!(
            service.authenticate("Unknown", PASSWORD).await,
            Err(AuthenticationError::Derivation(PasswordDerivationError::Overloaded))
        ));

        gate.release();
        for derivation in active {
            derivation.await.unwrap().unwrap();
        }
        assert_eq!(real_check.await.unwrap(), Some(real.id));
        assert_eq!(decoy_check.await.unwrap(), None);
        replacement.await.unwrap().unwrap();
        assert_eq!(legacy_check.await.unwrap(), Some(legacy.id));
        gate.wait_started(9).await;
        assert_eq!(gate.counts(), (9, 9, 2));
    });
}

async fn status(router: &Router, user: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri("/+status");
    if let Some(user) = user {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{PASSWORD}"))),
        );
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

fn spawn_authentication(service: &UserService, name: &'static str) -> tokio::task::JoinHandle<Option<UserId>> {
    let service = service.clone();
    tokio::spawn(async move { service.authenticate(name, PASSWORD).await.unwrap() })
}

#[derive(Clone)]
struct DerivationGate {
    state: Arc<DerivationGateState>,
}

impl DerivationGate {
    fn new() -> Self {
        Self {
            state: Arc::new(DerivationGateState {
                admitted: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                verifier_reads: AtomicUsize::new(0),
                changed: Notify::new(),
                release: ThreadBarrier::new(ACTIVE_DERIVATIONS + 1),
            }),
        }
    }

    async fn wait_admitted(&self, expected: usize) {
        self.wait_count(&self.state.admitted, expected).await;
    }

    async fn wait_started(&self, expected: usize) {
        self.wait_count(&self.state.started, expected).await;
    }

    async fn wait_verifier_reads(&self, expected: usize) {
        self.wait_count(&self.state.verifier_reads, expected).await;
    }

    fn release(&self) {
        self.state.release.wait();
    }

    fn counts(&self) -> (usize, usize, usize) {
        (
            self.state.admitted.load(Ordering::SeqCst),
            self.state.started.load(Ordering::SeqCst),
            self.state.verifier_reads.load(Ordering::SeqCst),
        )
    }

    async fn wait_count(&self, counter: &AtomicUsize, expected: usize) {
        while counter.load(Ordering::SeqCst) < expected {
            self.state.changed.notified().await;
        }
    }
}

struct DerivationGateState {
    admitted: AtomicUsize,
    started: AtomicUsize,
    verifier_reads: AtomicUsize,
    changed: Notify,
    release: ThreadBarrier,
}

impl<Subscriber> tracing_subscriber::Layer<Subscriber> for DerivationGate
where
    Subscriber: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        match event.metadata().target() {
            "peryx_driver::users::password_derivation_admitted" => {
                self.state.admitted.fetch_add(1, Ordering::SeqCst);
                self.state.changed.notify_one();
            }
            "peryx_driver::users::password_derivation_started" => {
                let started = self.state.started.fetch_add(1, Ordering::SeqCst);
                self.state.changed.notify_one();
                if started < ACTIVE_DERIVATIONS {
                    self.state.release.wait();
                }
            }
            "peryx_driver::users::password_verifier_read" => {
                self.state.verifier_reads.fetch_add(1, Ordering::SeqCst);
                self.state.changed.notify_one();
            }
            _ => {}
        }
    }
}
