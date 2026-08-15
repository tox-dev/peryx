use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroU16;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use axum::body::Body;
use axum::http::Request;
use peryx_driver::serving::AbsoluteProtocolDriver as _;
use peryx_driver::{AppState, ServingState};
use peryx_ecosystem_oci::OciRegistry;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_plugin_registry::PluginRegistry;
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tempfile::TempDir;
use tokio::runtime::{Builder, Runtime};

pub const MAX_MANIFEST_BYTES: usize = 4096;

static MANIFEST_ADAPTER: LazyLock<Mutex<ManifestAdapter>> = LazyLock::new(Mutex::default);

pub struct ManifestAdapter {
    harness: Harness,
    reset_after: NonZeroU16,
}

impl ManifestAdapter {
    #[must_use]
    pub fn new(reset_after: NonZeroU16) -> Self {
        Self {
            harness: Harness::new(),
            reset_after,
        }
    }

    pub fn run(&mut self, data: &[u8]) -> Option<u16> {
        if data.len() > MAX_MANIFEST_BYTES {
            return None;
        }
        let runs = self.harness.parse(data);
        if runs == self.reset_after.get() {
            self.harness = Harness::new();
            Some(0)
        } else {
            Some(runs)
        }
    }
}

impl Default for ManifestAdapter {
    fn default() -> Self {
        Self::new(NonZeroU16::new(1024).expect("the reset interval is nonzero"))
    }
}

struct Harness {
    _directory: TempDir,
    registry: OciRegistry,
    runtime: Runtime,
    state: Arc<ServingState>,
    runs: u16,
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create fuzz data directory");
        let runtime = Builder::new_current_thread().build().expect("create fuzz runtime");
        let runtime_guard = runtime.enter();
        let index = Index {
            name: "fuzz".to_owned(),
            route: "fuzz".to_owned(),
            ecosystem: peryx_ecosystem_oci::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl {
                anonymous_read: true,
                tokens: vec![NamedToken {
                    name: "fuzzer".to_owned(),
                    secret: "fuzz".to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Write]),
                    }],
                    expires_at: None,
                }],
            },
        };
        let mut state = AppState::new(
            MetaStore::open(directory.path().join("peryx.redb")).expect("open fuzz metadata"),
            BlobStore::new(directory.path().join("blobs")),
            60,
            vec![index],
        );
        let registry = PluginRegistry::new(vec![peryx_ecosystem_oci::registration()])
            .expect("register OCI fuzz plugin")
            .activate([peryx_ecosystem_oci::ECOSYSTEM])
            .expect("activate OCI fuzz plugin");
        registry.register_activated_capabilities(&mut state.capability_install_context());
        registry
            .install_drivers(
                &mut state.runtime_install_context().expect("create fuzz install context"),
                &HashMap::new(),
            )
            .expect("install OCI fuzz driver");
        drop(runtime_guard);
        Self {
            _directory: directory,
            registry: OciRegistry::default(),
            runtime,
            state: state.serving.clone(),
            runs: 0,
        }
    }

    fn parse(&mut self, data: &[u8]) -> u16 {
        let request = Request::builder()
            .method("PUT")
            .uri("/v2/fuzz/package/manifests/latest")
            .header("authorization", "Basic XzpmdXp6")
            .header("content-type", "application/vnd.oci.image.manifest.v1+json")
            .body(Body::from(data.to_vec()))
            .expect("build fuzz request");
        let _ = self.runtime.block_on(self.registry.serve(self.state.clone(), request));
        self.runs += 1;
        self.runs
    }
}

#[must_use]
pub fn fuzz_manifest(data: &[u8]) -> Option<u16> {
    MANIFEST_ADAPTER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .run(data)
}

#[must_use]
pub fn fuzz_reference(data: &[u8]) -> bool {
    let Ok(reference) = std::str::from_utf8(data) else {
        return false;
    };
    let _ = OciRegistry::default().classify_route(&format!("/v2/fuzz/manifests/{reference}"));
    true
}
