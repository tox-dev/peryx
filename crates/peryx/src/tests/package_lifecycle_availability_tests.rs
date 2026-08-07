//! Package lifecycle operations exercised across the `none`, `dc`, and `ha` availability modes.
//!
//! Publish, promote, yank, delete, and restore are the durable lifecycle a hosted index exposes. These
//! probes hold that each operation reaches the same documented outcome whether the node runs as a
//! single-node `none` writer or a `dc`/`ha` writer that fronts a consensus group: replication changes
//! how a mutation becomes durable, never what the client-visible lifecycle is. A read-only replica
//! originates none of it and refuses every mutation at its gate.
//!
//! Authorization is decided once, at the request boundary, before any bytes are staged. An upload with
//! a credential the index does not accept is denied at ingress in every mode, so the same deny-by-default
//! contract holds under replication.
//!
//! Every arm is deterministic: requests run through the in-process router with [`ServiceExt::oneshot`],
//! and a replica advances one [`sync_cycle`](crate::replication::ReplicationRuntime::sync_cycle) at a
//! time. No test sleeps, binds a fixed port, or reads a real clock. Following the [PyPA specifications]
//! for the `PyPI` surface and the [OCI Distribution Specification] for the registry surface.
//!
//! [PyPA specifications]: https://packaging.python.org/en/latest/specifications/
//! [OCI Distribution Specification]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::state::AppState;
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::Value;
use tower::ServiceExt as _;

use crate::config::{
    AvailabilityConfig, Config, DcMember, DcMembership, DcRole, IndexConfig, IndexKind, ReplicationConfig, SecretSource,
};
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

/// The shared secret a replica presents to its primary; irrelevant to the assertions but required to
/// build the runtime.
const TOKEN: &str = "replica-secret";
/// The secret every hosted index accepts for writes, presented as the upload credential.
const UPLOAD: &str = "s3cret";
/// The writer identity a `dc`/`ha` store is claimed under, so a replica agrees with what it follows.
const WRITER_IDENTITY: &str = "writer-a";
/// A real wheel with parseable metadata, so the publish path admits it. Its name and version are fixed
/// by the archive, so the form fields must agree.
const WHEEL: &[u8] = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
/// The project and version the fixture wheel carries; the upload form must present these.
const PROJECT: &str = "veloxdemo";
const VERSION: &str = "1.0.0";

/// A hosted index at `route == name` that accepts `UPLOAD` for writes and deletes.
fn hosted(name: &str, ecosystem: Ecosystem) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted { volatile: true },
        anonymous_read: None,
        tokens: vec![crate::tests::writer_token(SecretSource::Literal(UPLOAD.to_owned()))],
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
    }
}

/// A `staging` and `prod` hosted `PyPI` pair (so a promote has a source and a target) and an `images`
/// hosted `OCI` store.
fn lifecycle_indexes() -> Vec<IndexConfig> {
    vec![
        hosted("staging", peryx_ecosystem_pypi::ECOSYSTEM),
        hosted("prod", peryx_ecosystem_pypi::ECOSYSTEM),
        hosted("images", peryx_ecosystem_oci::ECOSYSTEM),
    ]
}

/// A single-node `none` writer: the default posture, with no availability resource.
fn none_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: lifecycle_indexes(),
        ..Config::default()
    }
}

/// A `dc` writer that fronts a two-member group and journals its mutations for replication.
fn dc_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_replication()),
        dc_membership: Some(group()),
        indexes: lifecycle_indexes(),
        ..Config::default()
    }
}

/// An `ha` writer: the same primary posture reached through an `ha` roster, so the mode label differs
/// and the durability requirement is replicated.
/// A single-datacenter `ha` roster: one writer, no remote datacenter to wait on. A single-process `ha`
/// behavior test proves its writes locally, since cross-datacenter write completion needs a reachable
/// remote and is exercised by the multi-process availability harness rather than faked here.
fn solo_group() -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: vec![DcMember {
            node: WRITER_IDENTITY.to_owned(),
            dc: "east-1".to_owned(),
            address: "10.0.0.1:8080".to_owned(),
            role: DcRole::Writer,
        }],
    }
}

fn ha_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(primary_replication()),
        dc_membership: Some(solo_group()),
        indexes: lifecycle_indexes(),
        ..Config::default()
    }
}

/// A `dc` replica that follows `upstream`. A configured replica is read-only, so its router refuses
/// mutations.
fn dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(replica_replication(upstream)),
        indexes: lifecycle_indexes(),
        ..Config::default()
    }
}

/// An `ha` replica: the same read-only follower reached through an `ha` roster.
fn ha_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(replica_replication(upstream)),
        indexes: lifecycle_indexes(),
        ..Config::default()
    }
}

fn primary_replication() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: WRITER_IDENTITY.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
    }
}

fn replica_replication(upstream: &str) -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: upstream.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
        poll_interval: Duration::from_millis(1),
        page_size: NonZeroUsize::MIN,
    }
}

fn group() -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: vec![
            DcMember {
                node: WRITER_IDENTITY.to_owned(),
                dc: "east-1".to_owned(),
                address: "10.0.0.1:8080".to_owned(),
                role: DcRole::Writer,
            },
            DcMember {
                node: "replica-b".to_owned(),
                dc: "east-2".to_owned(),
                address: "10.0.0.2:8080".to_owned(),
                role: DcRole::Replica,
            },
        ],
    }
}

/// Seed the replica store's writer identity before it is opened read-only, the way a provisioned
/// replica is handed the writer it will follow.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

/// Build a writer node's state and neutral router. A writer accepts mutations, so its router carries no
/// read-only gate.
fn writer_node(config: &Config) -> (Arc<AppState>, Router) {
    let state = build_state(config).unwrap();
    let router = router_for(state.clone());
    (state, router)
}

/// A writer whose `ha` metadata dimension waits on `remotes`, so the ingress POST resolves its metadata
/// acknowledgement from a remote frontier rather than the local journal alone. A single-datacenter roster
/// configures no remote to wait on, so the sources are injected directly into the freshly built state,
/// which is still uniquely owned before the router clones it.
fn writer_node_with_remotes(
    config: &Config,
    remotes: Vec<Arc<dyn peryx_ha_distributed::RemoteFrontierSource + Send + Sync>>,
) -> (Arc<AppState>, Router) {
    let mut state = build_state(config).unwrap();
    Arc::get_mut(&mut state)
        .expect("the freshly built state is uniquely owned before the router clones it")
        .set_remote_frontier_sources(remotes);
    let router = router_for(state.clone());
    (state, router)
}

/// Build a read-only replica's state and full router (with its mutation gate) plus the runtime that
/// drives its sync.
fn replica_node(config: &Config) -> (Arc<AppState>, Router, ReplicationRuntime) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::new(config, &state).unwrap();
    let router = runtime.mount(router_for(state.clone()));
    (state, router, runtime)
}

/// The Basic header value carrying `secret` under `user`.
fn basic(user: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{secret}")))
}

/// Publish the wheel to `route` as `project` `version`, authenticated by `secret`, and return the
/// response status. `PyPI` uploads use the `__token__` Basic convention over a legacy multipart form.
async fn upload_as(router: &Router, route: &str, project: &str, version: &str, secret: &str) -> StatusCode {
    let boundary = "peryxlifecycleboundary";
    let digest = Digest::of(WHEEL);
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", project),
        ("version", version),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", digest.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    let filename = format!("{project}-{version}-py3-none-any.whl");
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{filename}\"\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(WHEEL);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/{route}/"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::AUTHORIZATION, basic("__token__", secret))
        .body(Body::from(body))
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// An authenticated `PyPI` mutation (yank, unyank, delete, restore, promote) with no body.
async fn pypi_mutate(router: &Router, method: &str, path: &str) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, basic("__token__", UPLOAD))
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// Whether `route` currently serves a Simple detail page for `project`: `200` when a version is
/// visible, `404` when none is.
async fn pypi_present(router: &Router, route: &str, project: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/{route}/simple/{project}/"))
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// Push a blob to an `OCI` repository monolithically and return its digest.
async fn oci_push_blob(router: &Router, repo: &str, bytes: &[u8]) -> String {
    let digest = format!("sha256:{}", Digest::of(bytes).as_str());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v2/{repo}/blobs/uploads/?digest={digest}"))
        .header(header::AUTHORIZATION, basic("_", UPLOAD))
        .body(Body::from(bytes.to_vec()))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::CREATED,
        "the config blob uploads",
    );
    digest
}

/// Push a one-config, zero-layer manifest to `repo` under `reference` and return the response status.
async fn oci_push_manifest(router: &Router, repo: &str, reference: &str, config: &str) -> StatusCode {
    let manifest = format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":2}},"#,
            r#""layers":[]}}"#,
        ),
        config = config,
    );
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v2/{repo}/manifests/{reference}"))
        .header(header::AUTHORIZATION, basic("_", UPLOAD))
        .header(header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
        .body(Body::from(manifest))
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// An authenticated `OCI` manifest mutation (delete, restore) with no body.
async fn oci_mutate(router: &Router, method: &str, path: &str) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, basic("_", UPLOAD))
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// The status of a manifest read by reference.
async fn oci_present(router: &Router, repo: &str, reference: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v2/{repo}/manifests/{reference}"))
        .header(header::AUTHORIZATION, basic("_", UPLOAD))
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// The availability mode a node reports on its public topology surface.
async fn topology_mode(router: &Router) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri("/+availability/topology")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["mode"].clone()
}

/// The full `PyPI` lifecycle reaches its documented outcome in each mode: a publish becomes visible, a
/// promote copies it to another index, a yank hides then a restore returns it, and a delete removes then
/// a restore brings it back. The node reports the mode it ran under, so each arm is tied to its mode.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config, "none")]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config, "dc")]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config, "ha")]
#[tokio::test]
async fn test_pypi_lifecycle_completes_in_every_availability_mode(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] mode: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router) = writer_node(&build(&dir));
    assert_eq!(
        topology_mode(&router).await,
        mode,
        "the node runs under the {mode} mode"
    );

    // Publish into staging.
    assert_eq!(
        upload_as(&router, "staging", PROJECT, VERSION, UPLOAD).await,
        StatusCode::OK
    );
    assert_eq!(pypi_present(&router, "staging", PROJECT).await, StatusCode::OK);

    // Promote the release from staging into prod.
    assert_eq!(
        pypi_mutate(
            &router,
            "PUT",
            &format!("/prod/{PROJECT}/{VERSION}/promote?from=staging")
        )
        .await,
        StatusCode::OK,
    );
    assert_eq!(pypi_present(&router, "prod", PROJECT).await, StatusCode::OK);

    // Yank hides the release from resolution, and an unyank reverses it; the page stays served either way.
    assert_eq!(
        pypi_mutate(&router, "PUT", &format!("/prod/{PROJECT}/{VERSION}/yank")).await,
        StatusCode::OK,
    );
    assert_eq!(
        pypi_mutate(&router, "DELETE", &format!("/prod/{PROJECT}/{VERSION}/yank")).await,
        StatusCode::OK,
    );
    assert_eq!(pypi_present(&router, "prod", PROJECT).await, StatusCode::OK);

    // Delete removes the release, and restore brings it back from trash.
    assert_eq!(
        pypi_mutate(&router, "DELETE", &format!("/prod/{PROJECT}/{VERSION}/")).await,
        StatusCode::OK,
    );
    assert_eq!(pypi_present(&router, "prod", PROJECT).await, StatusCode::NOT_FOUND);
    assert_eq!(
        pypi_mutate(&router, "PUT", &format!("/prod/{PROJECT}/{VERSION}/restore")).await,
        StatusCode::OK,
    );
    assert_eq!(pypi_present(&router, "prod", PROJECT).await, StatusCode::OK);
}

/// An `ha` publish waits on a remote datacenter's metadata frontier: with an eligible remote already
/// holding the operation, the write proves durable and returns `200`, the same client-visible outcome as
/// a single-datacenter write. This drives the ingress POST's remote-metadata arm, which a single-DC `ha`
/// roster (no remote to wait on) never reaches.
#[tokio::test]
async fn test_ha_publish_waits_on_a_covering_remote_metadata_frontier() {
    let dir = tempfile::tempdir().unwrap();
    // With no ownership group, the committed authority epoch is 0; a frontier at the ceiling covers any
    // serial the store commits, so this remote holds every operation the write can produce.
    let remote = peryx_ha_distributed::LoopbackRemoteFrontierSource::reporting("west-1", 0, u64::MAX);
    let (_state, router) = writer_node_with_remotes(&ha_writer_config(&dir), vec![Arc::new(remote)]);

    assert_eq!(
        upload_as(&router, "staging", PROJECT, VERSION, UPLOAD).await,
        StatusCode::OK,
        "an eligible remote holding the operation proves the write remote-durable"
    );
    assert_eq!(pypi_present(&router, "staging", PROJECT).await, StatusCode::OK);
}

/// The `OCI` manifest lifecycle reaches its documented outcome in each mode: a push publishes a tag, a
/// delete trashes it, and a restore returns it.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config, "none")]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config, "dc")]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config, "ha")]
#[tokio::test]
async fn test_oci_lifecycle_completes_in_every_availability_mode(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] mode: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router) = writer_node(&build(&dir));
    assert_eq!(topology_mode(&router).await, mode);

    let config = oci_push_blob(&router, "images/app", b"{}").await;
    assert_eq!(
        oci_push_manifest(&router, "images/app", "1.0", &config).await,
        StatusCode::CREATED
    );
    assert_eq!(oci_present(&router, "images/app", "1.0").await, StatusCode::OK);

    // Delete the tag, then restore it.
    assert_eq!(
        oci_mutate(&router, "DELETE", "/v2/images/app/manifests/1.0").await,
        StatusCode::ACCEPTED,
    );
    assert_eq!(oci_present(&router, "images/app", "1.0").await, StatusCode::NOT_FOUND);
    assert_eq!(
        oci_mutate(&router, "PUT", "/v2/images/app/manifests/1.0/restore").await,
        StatusCode::ACCEPTED,
    );
    assert_eq!(oci_present(&router, "images/app", "1.0").await, StatusCode::OK);
}

/// A read-only replica originates no lifecycle: it refuses every mutation at its gate with
/// `503 read_only_replica`, ahead of any handler, on both the `PyPI` and `OCI` surfaces. Holds for a `dc`
/// and an `ha` follower.
#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_read_only_replica_refuses_every_lifecycle_mutation(
    #[case] build: fn(&tempfile::TempDir, &str) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/"));
    assert!(state.read_only, "a configured replica is read-only");

    assert_eq!(
        upload_as(&router, "staging", "flask", "1.0", UPLOAD).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a replica refuses a PyPI publish",
    );
    assert_eq!(
        pypi_mutate(&router, "DELETE", "/prod/flask/1.0/").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a replica refuses a PyPI delete",
    );
    assert_eq!(
        oci_mutate(&router, "PUT", "/v2/images/app/manifests/1.0").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a replica refuses an OCI push",
    );
}

/// A publish is authorized once, at ingress: a credential the index does not accept is denied before any
/// bytes are staged, and nothing becomes visible. An accepted credential publishes. The contract holds
/// identically in each mode, so replication never widens who may write.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_unauthorized_publish_is_denied_at_ingress_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router) = writer_node(&build(&dir));

    // A wrong secret is refused at the boundary, and the release never becomes visible.
    assert_eq!(
        upload_as(&router, "staging", PROJECT, VERSION, "wrong-secret").await,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(pypi_present(&router, "staging", PROJECT).await, StatusCode::NOT_FOUND);

    // The accepted credential publishes.
    assert_eq!(
        upload_as(&router, "staging", PROJECT, VERSION, UPLOAD).await,
        StatusCode::OK
    );
    assert_eq!(pypi_present(&router, "staging", PROJECT).await, StatusCode::OK);
}
