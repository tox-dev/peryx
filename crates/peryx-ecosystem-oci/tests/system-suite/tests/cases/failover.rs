//! Hosted writes require home durability, not cross-DC acknowledgement.

use std::{path::PathBuf, time::Duration};

use peryx::{config, config::Config, operator};
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use serde_json::Value;

use crate::harness::{Cluster, MemberSpec, Node, Role, Topology};

const OCI_ROUTE: &str = "oci";
const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const UPLOAD_TOKEN: &str = "harness-upload-secret";

const READ_ONLY_BODY: &str = r#"{"error":"read_only_replica","message":"this replica does not accept mutations"}"#;

const REPO: &str = "app";

const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;

const LAYER: &[u8] = b"a-single-image-layer-of-bytes";

fn home_oci_group() -> Cluster {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
            MemberSpec::new("replica-south", "south", Role::Replica),
        ],
    )
    .with_admin()
    .with_index_config(oci_config())
    .with_write_ack_deadline(1)
    .start()
    .expect("the ha group starts")
}

const fn oci_config() -> &'static str {
    "[[index]]\nname = \"oci\"\necosystem = \"oci\"\nhosted = true\nvolatile = true\n\n\
     [[index.access_token]]\nname = \"uploader\"\nsecret = \"harness-upload-secret\"\n\
     resources = [\"*\"]\nactions = [\"write\", \"delete\"]\n"
}

fn oci_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Digest::of(bytes).as_str())
}

trait OciNodeExt {
    fn oci_v2(&self) -> Option<(u16, String)>;
    fn oci_push_blob(&self, repo: &str, blob: &[u8]) -> Result<(u16, String), reqwest::Error>;
    fn oci_pull_blob(&self, repo: &str, digest: &str) -> Option<(u16, Vec<u8>)>;
    fn oci_put_manifest(
        &self,
        repo: &str,
        reference: &str,
        manifest: &[u8],
        media_type: &str,
    ) -> Result<(u16, String), reqwest::Error>;
    fn oci_get_manifest(&self, repo: &str, reference: &str) -> Option<(u16, Option<String>, Vec<u8>)>;
    fn oci_tags(&self, repo: &str) -> Vec<String>;
    fn oci_mutate(&self, method: reqwest::Method, path: &str) -> Option<(u16, Option<String>, String)>;
}

impl OciNodeExt for Node {
    fn oci_v2(&self) -> Option<(u16, String)> {
        self.http_get("/v2/")
    }

    fn oci_push_blob(&self, repo: &str, blob: &[u8]) -> Result<(u16, String), reqwest::Error> {
        let digest = oci_digest(blob);
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v2/{OCI_ROUTE}/{repo}/blobs/uploads/?digest={digest}"),
            )
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .body(blob.to_vec())
            .send()?;
        Ok((response.status().as_u16(), digest))
    }

    fn oci_pull_blob(&self, repo: &str, digest: &str) -> Option<(u16, Vec<u8>)> {
        self.download(&format!("/v2/{OCI_ROUTE}/{repo}/blobs/{digest}"))
    }

    fn oci_put_manifest(
        &self,
        repo: &str,
        reference: &str,
        manifest: &[u8],
        media_type: &str,
    ) -> Result<(u16, String), reqwest::Error> {
        let response = self
            .request(
                reqwest::Method::PUT,
                &format!("/v2/{OCI_ROUTE}/{repo}/manifests/{reference}"),
            )
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(manifest.to_vec())
            .send()?;
        Ok((response.status().as_u16(), oci_digest(manifest)))
    }

    fn oci_get_manifest(&self, repo: &str, reference: &str) -> Option<(u16, Option<String>, Vec<u8>)> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/v2/{OCI_ROUTE}/{repo}/manifests/{reference}"),
            )
            .header(reqwest::header::ACCEPT, OCI_MANIFEST_TYPE)
            .send()
            .ok()?;
        let code = response.status().as_u16();
        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Some((code, digest, response.bytes().ok()?.to_vec()))
    }

    fn oci_tags(&self, repo: &str) -> Vec<String> {
        let Some((200, body)) = self.http_get(&format!("/v2/{OCI_ROUTE}/{repo}/tags/list")) else {
            return Vec::new();
        };
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.get("tags")?.as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.as_str().map(str::to_owned))
            .collect()
    }

    fn oci_mutate(&self, method: reqwest::Method, path: &str) -> Option<(u16, Option<String>, String)> {
        let response = self
            .request(method, path)
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Some((code, retry_after, response.text().unwrap_or_default()))
    }
}

fn image_manifest() -> Vec<u8> {
    format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"{manifest}","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":{config_size}}},"#,
            r#""layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer}","size":{layer_size}}}]}}"#,
        ),
        manifest = OCI_MANIFEST_TYPE,
        config = oci_digest(CONFIG),
        config_size = CONFIG.len(),
        layer = oci_digest(LAYER),
        layer_size = LAYER.len(),
    )
    .into_bytes()
}

fn push_image(node: &Node) -> String {
    for blob in [CONFIG, LAYER] {
        let (code, _) = node.oci_push_blob(REPO, blob).expect("blob push reaches the writer");
        assert!(
            matches!(code, 201 | 202),
            "a home blob push commits home-locally: {code}",
        );
    }
    let manifest = image_manifest();
    let (code, digest) = node
        .oci_put_manifest(REPO, "1.0", &manifest, OCI_MANIFEST_TYPE)
        .expect("manifest publish reaches the writer");
    assert!(
        matches!(code, 201 | 202),
        "a home manifest publish commits home-locally: {code}",
    );
    digest
}

fn assert_image_intact(node: &Node, manifest_digest: &str) {
    for blob in [CONFIG, LAYER] {
        let (code, bytes) = node
            .oci_pull_blob(REPO, &oci_digest(blob))
            .expect("blob pull reaches the node");
        assert_eq!((code, bytes.as_slice()), (200, blob), "the node serves the pushed blob");
    }
    let (code, resolved, bytes) = node
        .oci_get_manifest(REPO, "1.0")
        .expect("manifest resolution reaches the node");
    assert_eq!(code, 200, "the tag still resolves after recovery");
    assert_eq!(
        resolved.as_deref(),
        Some(manifest_digest),
        "the tag resolves to the same manifest digest",
    );
    assert_eq!(bytes, image_manifest(), "the resolved manifest is the published bytes");
    assert_eq!(
        node.oci_tags(REPO),
        vec!["1.0".to_owned()],
        "the repository lists the one tag once"
    );
}

#[test]
fn test_home_dc_oci_push_is_retry_safe_and_survives_a_home_failure() {
    let mut cluster = home_oci_group();
    await_writer_leader(&cluster);
    let writer = cluster.node("writer-east").expect("the home writer is present");

    let manifest_digest = push_image(writer);
    assert_image_intact(writer, &manifest_digest);

    // Two survivors preserve quorum while the writer reopens durable state.
    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    home.restart().expect("the home datacenter restarts on its store");
    await_writer_leader(&cluster);
    let writer = cluster.node("writer-east").expect("the home writer is back");

    assert_image_intact(writer, &manifest_digest);

    let retried_digest = push_image(writer);
    assert_eq!(
        retried_digest, manifest_digest,
        "the retry publishes the identical manifest digest",
    );
    assert_image_intact(writer, &manifest_digest);
}

fn await_writer_leader(cluster: &Cluster) {
    let leader = cluster
        .await_leader(Duration::from_secs(90))
        .expect("the ha group agrees on a leader");
    cluster
        .await_topology_signal(Duration::from_secs(90), |cluster| {
            let observed = cluster.node("writer-east").and_then(Node::consensus_leader);
            (
                (observed.as_deref() == Some(leader.as_str())).then_some(()),
                format!("writer-east last observed leader: {observed:?}"),
            )
        })
        .expect("the home writer observes the elected leader");
}

#[test]
fn test_ha_replica_rejects_oci_mutations_before_and_after_a_home_failure() {
    let mut cluster = home_oci_group();
    cluster
        .await_leader(Duration::from_secs(90))
        .expect("the ha group agrees on a leader");
    let replica = cluster.node("replica-west").expect("the replica is present");

    assert_replica_refuses_oci_mutations(replica);

    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    cluster
        .await_leader_change("east", Duration::from_secs(90))
        .expect("authority leaves the failed datacenter");

    let replica = cluster.node("replica-west").expect("the replica survives");
    assert!(
        replica.is_ready(),
        "the replica keeps serving reads after the home fails"
    );
    assert_replica_refuses_oci_mutations(replica);
}

fn assert_replica_refuses_oci_mutations(replica: &Node) {
    let route = OCI_ROUTE;
    let cases = [
        (reqwest::Method::POST, format!("/v2/{route}/{REPO}/blobs/uploads/")),
        (reqwest::Method::PUT, format!("/v2/{route}/{REPO}/manifests/1.0")),
        (reqwest::Method::DELETE, format!("/v2/{route}/{REPO}/manifests/1.0")),
        (
            reqwest::Method::DELETE,
            format!("/v2/{route}/{REPO}/blobs/{}", oci_digest(LAYER)),
        ),
    ];
    for (method, path) in cases {
        let (code, retry_after, body) = replica
            .oci_mutate(method.clone(), &path)
            .expect("mutation reaches the replica");
        assert_eq!(
            (code, retry_after.as_deref(), body.as_str()),
            (503, Some("1"), READ_ONLY_BODY),
            "the replica refuses a {method} {path}",
        );
    }
}

#[test]
fn test_registry_round_trip() {
    let cluster = Topology::single()
        .with_index_config(oci_config())
        .start()
        .expect("cluster starts");
    let node = &cluster.nodes()[0];

    assert_eq!(node.oci_v2().map(|response| response.0), Some(200));
    assert_eq!(
        node.oci_mutate(reqwest::Method::POST, &format!("/v2/{OCI_ROUTE}/app/blobs/uploads/"))
            .map(|response| response.0),
        Some(202),
    );
    let blob = b"harness-oci-layer";
    let (code, digest) = node.oci_push_blob("app", blob).expect("blob push reaches the node");
    assert_eq!(code, 201);
    assert_eq!(node.oci_pull_blob("app", &digest), Some((200, blob.to_vec())));

    let manifest = br#"{"schemaVersion":2}"#;
    assert_eq!(
        node.oci_put_manifest("app", "1.0", manifest, OCI_MANIFEST_TYPE)
            .expect("manifest reaches the node")
            .0,
        201,
    );
    assert_eq!(node.oci_tags("app"), vec!["1.0".to_owned()]);
}

#[test]
fn test_backup_round_trips_complete_oci_config() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let source = format!(
        r#"
data_dir = {data_dir:?}

[[index]]
name = "hub"
ecosystem = "oci"
upstream_concurrency = 9
offline = false

[index.prefetch]
mode = "metadata-only"
packages = ["library/nginx"]
requirements = []
metadata_only = true

[index.policy]
allow_projects = ["library/*"]
block_projects = []

[index.settings]
library_prefix = true

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
token_env = "DOCKERHUB_TOKEN"
credential_refresh_secs = 60

[[index]]
name = "images"
ecosystem = "oci"
hosted = true
volatile = false
anonymous_read = false

[index.policy]
allow_projects = []
block_projects = ["internal/*"]

[[index.access_token]]
name = "uploader"
secret_file = "/run/secrets/upload-token"
actions = ["write", "delete"]

[[index.access_token]]
name = "ci"
secret_file = "/run/secrets/ci-token"
resources = ["team/*"]
actions = ["read", "write"]
expires_at = "2027-01-01T00:00:00Z"

[[index.access_token]]
name = "janitor"
secret = "janitor-secret"
resources = ["*"]
actions = ["delete"]

[[index.webhook]]
name = "audit"
url = "https://hooks.example/audit"
secret_env = "AUDIT_WEBHOOK_SECRET"
events = ["upload", "delete"]

[[index.webhook]]
name = "local"
url = "https://hooks.example/local"
secret = "webhook-secret"
events = ["upload"]

[[index]]
name = "root-oci"
route = "root/oci"
ecosystem = "oci"
layers = ["images", "hub"]
write_target = "images"

[index.policy]
allow_projects = []
block_projects = []
"#
    );
    let config = Config::default()
        .apply(config::from_toml(PathBuf::from("source.toml"), &source).unwrap())
        .unwrap();
    let backup = root.path().join("backup");

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    let restored = Config::default()
        .apply(
            config::from_toml(
                PathBuf::from("config.toml"),
                &std::fs::read_to_string(backup.join("config.toml")).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(restored, config);
}

#[test]
fn test_registry_route_requires_an_oci_index() {
    let cluster = Topology::single().start().expect("cluster starts");
    let node = &cluster.nodes()[0];
    assert_eq!(node.oci_v2().map(|response| response.0), Some(404));
    assert!(node.oci_tags("app").is_empty());
}
