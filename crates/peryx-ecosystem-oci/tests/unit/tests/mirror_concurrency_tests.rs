//! What a mirror run overlaps: sibling manifests of one level, the blobs of one manifest, and the
//! roots of one run, all under a single ceiling.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;
use wiremock::MockServer;

use super::mirror_tests::{INDEX_TYPE, MANIFEST_TYPE, image_manifest_with_layers, index_over, mount_blob};
use super::mirror_tests::{manifest_fetches, mount_manifest};
use super::{oci_digest, proxy, proxy_with_upstream_limit};
use crate::mirror::{MirrorMode, MirrorRow, mirror};
use crate::settings::IndexSettings;
use rstest::rstest;

/// The peak overlap a fixture registry saw.
#[derive(Default)]
struct InFlight {
    current: usize,
    peak: usize,
}

impl InFlight {
    fn enter(&mut self) {
        self.current += 1;
        self.peak = self.peak.max(self.current);
    }

    const fn leave(&mut self) {
        self.current -= 1;
    }
}

/// A registry that answers from `content` and parks each path in `held` until a ceiling's worth of
/// them are in flight together. A run that transferred one at a time would never fill the barrier;
/// one that ignored the ceiling would park more than the barrier releases.
struct Registry {
    content: HashMap<String, (&'static str, Vec<u8>)>,
    held: HashSet<String>,
    gate: Barrier,
    observed: Mutex<InFlight>,
}

pub(super) async fn read_path(connection: &mut TcpStream) -> String {
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0; 1024];
        let read = connection.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "the request ended before its headers");
        request.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&request)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

async fn answer(registry: Arc<Registry>, mut connection: TcpStream) {
    let path = read_path(&mut connection).await;
    if registry.held.contains(&path) {
        registry.observed.lock().unwrap().enter();
        registry.gate.wait().await;
        registry.observed.lock().unwrap().leave();
    }
    let (content_type, body) = &registry.content[&path];
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    connection.write_all(head.as_bytes()).await.unwrap();
    connection.write_all(body).await.unwrap();
}

/// Serves `listener` for as long as `run` needs it, so the fixture leaves no accept loop behind.
async fn serve_until_done<T>(listener: TcpListener, registry: &Arc<Registry>, run: impl Future<Output = T>) -> T {
    let mut run = Box::pin(run);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                tokio::spawn(answer(Arc::clone(registry), accepted.unwrap().0));
            }
            outcome = &mut run => return outcome,
        }
    }
}

fn manifest_path(repo: &str, reference: &str) -> String {
    format!("/v2/{repo}/manifests/{reference}")
}

fn blob_path(repo: &str, digest: &str) -> String {
    format!("/v2/{repo}/blobs/{digest}")
}

async fn blob_fetches(server: &MockServer, repo: &str, digest: &str) -> usize {
    let target = blob_path(repo, digest);
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == target)
        .count()
}

/// Runs one mirror sync against a parked registry and hands back the report with the overlap the
/// registry measured.
async fn synced_under(
    listener: TcpListener,
    registry: &Arc<Registry>,
    upstream_concurrency: Option<usize>,
    refs: &[String],
) -> (Vec<MirrorRow>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let state = proxy_with_upstream_limit(&dir, &base, upstream_concurrency);
    let rows = serve_until_done(
        listener,
        registry,
        mirror(
            &state.serving,
            &state.serving.indexes[0],
            IndexSettings::default(),
            refs,
            MirrorMode::Sync,
        ),
    )
    .await
    .unwrap();
    let peak = registry.observed.lock().unwrap().peak;
    (rows, peak)
}

fn registry(content: HashMap<String, (&'static str, Vec<u8>)>, held: HashSet<String>, ceiling: usize) -> Arc<Registry> {
    Arc::new(Registry {
        content,
        held,
        gate: Barrier::new(ceiling),
        observed: Mutex::default(),
    })
}

/// An index over `ceiling * 2` empty child indexes: the per-platform manifests of a wide image index,
/// with nothing under them, so the only transfers the fixture counts are the siblings themselves.
#[rstest]
#[case::the_index_ceiling(Some(2), 2)]
#[case::an_uncapped_index(None, 3)]
#[tokio::test]
async fn test_mirror_overlaps_sibling_manifests_up_to_the_ceiling(
    #[case] upstream_concurrency: Option<usize>,
    #[case] ceiling: usize,
) {
    let children: Vec<Vec<u8>> = (0..ceiling * 2)
        .map(|slot| index_over(&[], &format!("child-{slot}")))
        .collect();
    let digests: Vec<String> = children.iter().map(|body| oci_digest(body)).collect();
    let root = index_over(
        &digests.iter().map(String::as_str).collect::<Vec<_>>(),
        "a wide image index",
    );
    let mut content = HashMap::from([(manifest_path("library/app", "latest"), (INDEX_TYPE, root))]);
    content.extend(
        digests
            .iter()
            .zip(children)
            .map(|(digest, body)| (manifest_path("library/app", digest), (INDEX_TYPE, body))),
    );
    let held = digests
        .iter()
        .map(|digest| manifest_path("library/app", digest))
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (rows, peak) = synced_under(
        listener,
        &registry(content, held, ceiling),
        upstream_concurrency,
        &["library/app:latest".to_owned()],
    )
    .await;

    assert_eq!(peak, ceiling);
    assert_eq!(
        rows.iter()
            .skip(1)
            .take(digests.len())
            .map(|row| &row.reference)
            .collect::<Vec<_>>(),
        digests.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        rows.last().unwrap().reason,
        format!("{} synced, 0 cached, 0 errors", digests.len() + 1)
    );
}

/// A manifest's config and layers are independent of one another, so they move together rather than
/// one at a time behind the manifest that named them.
#[tokio::test]
async fn test_mirror_overlaps_the_blobs_of_one_manifest_up_to_the_ceiling() {
    const CEILING: usize = 2;
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layers: Vec<Vec<u8>> = (0..CEILING * 2 - 1)
        .map(|slot| format!("a-layer-of-bytes-{slot}").into_bytes())
        .collect();
    let manifest = image_manifest_with_layers(config, &layers.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let mut content = HashMap::from([
        (manifest_path("library/app", "latest"), (MANIFEST_TYPE, manifest)),
        (
            blob_path("library/app", &oci_digest(config)),
            (MANIFEST_TYPE, config.to_vec()),
        ),
    ]);
    content.extend(layers.iter().map(|layer| {
        (
            blob_path("library/app", &oci_digest(layer)),
            (MANIFEST_TYPE, layer.clone()),
        )
    }));
    let held = std::iter::once(blob_path("library/app", &oci_digest(config)))
        .chain(layers.iter().map(|layer| blob_path("library/app", &oci_digest(layer))))
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (rows, peak) = synced_under(
        listener,
        &registry(content, held, CEILING),
        Some(CEILING),
        &["library/app:latest".to_owned()],
    )
    .await;

    assert_eq!(peak, CEILING);
    assert_eq!(
        rows.iter().map(|row| (row.kind, row.status)).collect::<Vec<_>>(),
        [
            ("manifest", "synced"),
            ("blob", "synced"),
            ("blob", "synced"),
            ("blob", "synced"),
            ("blob", "synced"),
            ("summary", "synced"),
        ]
    );
}

/// Selected references spend one budget between them, and the report still reads in selection order
/// however the roots finish.
#[tokio::test]
async fn test_mirror_shares_one_ceiling_across_roots() {
    const CEILING: usize = 2;
    let repos: Vec<String> = (0..CEILING * 2).map(|slot| format!("library/app{slot}")).collect();
    let content = repos
        .iter()
        .map(|repo| {
            (
                manifest_path(repo, "latest"),
                (INDEX_TYPE, index_over(&[], &format!("root of {repo}"))),
            )
        })
        .collect();
    let held = repos.iter().map(|repo| manifest_path(repo, "latest")).collect();
    let refs: Vec<String> = repos.iter().map(|repo| format!("{repo}:latest")).collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (rows, peak) = synced_under(listener, &registry(content, held, CEILING), Some(CEILING), &refs).await;

    assert_eq!(peak, CEILING);
    assert_eq!(
        rows.iter().take(repos.len()).map(|row| &row.repo).collect::<Vec<_>>(),
        repos.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        rows.last().unwrap().reason,
        format!("{} synced, 0 cached, 0 errors", repos.len())
    );
}

/// A sibling the upstream will not serve is one error row. The siblings scheduled beside it are
/// independent work, so they finish, and the run reports them in the order the parent named them.
#[tokio::test]
async fn test_mirror_finishes_a_level_past_a_refused_sibling() {
    let server = MockServer::start().await;
    let present: Vec<Vec<u8>> = (0..2).map(|slot| index_over(&[], &format!("child-{slot}"))).collect();
    let refused = format!("sha512:{}", "f".repeat(128));
    let children = [oci_digest(&present[0]), refused.clone(), oci_digest(&present[1])];
    let root = index_over(&children.iter().map(String::as_str).collect::<Vec<_>>(), "root");
    mount_manifest(&server, "library/app", "latest", &root, INDEX_TYPE).await;
    for (digest, body) in [&children[0], &children[2]].into_iter().zip(&present) {
        mount_manifest(&server, "library/app", digest, body, INDEX_TYPE).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        IndexSettings::default(),
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    assert_eq!(
        rows.iter()
            .map(|row| (row.reference.as_str(), row.status))
            .collect::<Vec<_>>(),
        [
            ("latest", "synced"),
            (children[0].as_str(), "synced"),
            (refused.as_str(), "error"),
            (children[2].as_str(), "synced"),
            ("", "partial"),
        ]
    );
    assert_eq!(rows.last().unwrap().reason, "3 synced, 0 cached, 1 errors");
}

/// Platform manifests over one base share layers. Overlapping them must not turn one layer into one
/// transfer per manifest: the second manifest waits for the first and then reports the store.
#[tokio::test]
async fn test_mirror_pulls_a_layer_two_manifests_share_once() {
    let server = MockServer::start().await;
    let layer = b"a-shared-base-layer";
    let configs: [&[u8]; 2] = [
        br#"{"os":"linux","architecture":"amd64"}"#,
        br#"{"os":"linux","architecture":"arm64"}"#,
    ];
    let platforms: Vec<Vec<u8>> = configs
        .iter()
        .map(|config| image_manifest_with_layers(config, &[layer]))
        .collect();
    let children: Vec<String> = platforms.iter().map(|body| oci_digest(body)).collect();
    let root = index_over(&children.iter().map(String::as_str).collect::<Vec<_>>(), "root");
    mount_manifest(&server, "library/app", "latest", &root, INDEX_TYPE).await;
    for (digest, body) in children.iter().zip(&platforms) {
        mount_manifest(&server, "library/app", digest, body, MANIFEST_TYPE).await;
    }
    for config in configs {
        mount_blob(&server, "library/app", config).await;
    }
    mount_blob(&server, "library/app", layer).await;
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let rows = mirror(
        &state.serving,
        &state.serving.indexes[0],
        IndexSettings::default(),
        &["library/app:latest".to_owned()],
        MirrorMode::Sync,
    )
    .await
    .unwrap();

    let shared = oci_digest(layer);
    assert_eq!(blob_fetches(&server, "library/app", &shared).await, 1);
    assert_eq!(manifest_fetches(&server, "library/app", &children[0]).await, 1);
    let mut reported: Vec<&str> = rows
        .iter()
        .filter(|row| row.kind == "blob" && row.reference == shared)
        .map(|row| row.status)
        .collect();
    reported.sort_unstable();
    assert_eq!(reported, ["cached", "synced"]);
}
