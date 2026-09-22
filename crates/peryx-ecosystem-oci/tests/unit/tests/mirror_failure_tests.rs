//! What a mirror run does with a manifest body it cannot read to the end: one error row for that
//! reference, and everything selected or scheduled beside it is mirrored anyway.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rstest::rstest;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

use super::mirror_concurrency_tests::read_path;
use super::mirror_tests::{INDEX_TYPE, MANIFEST_TYPE, image_manifest_with_layers, index_over};
use super::{oci_digest, proxy, wait_for_staged_bytes};
use crate::mirror::{MirrorMode, MirrorRow, mirror};
use crate::registry::MAX_MANIFEST_BYTES;
use crate::settings::IndexSettings;

const BLOB_TYPE: &str = "application/octet-stream";

/// One canned response. `declared` is the length the head promises, so a value above what the body
/// holds closes the connection part way through and hands the client a truncated stream.
struct Answer {
    content_type: &'static str,
    declared: usize,
    body: Vec<u8>,
    stalled: Option<(bool, Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Notify>)>,
    challenge: bool,
}

impl Answer {
    fn whole(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type,
            declared: body.len(),
            body,
            stalled: None,
            challenge: false,
        }
    }

    fn truncated(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type,
            declared: body.len() + 1,
            body,
            stalled: None,
            challenge: false,
        }
    }

    fn stalled(content_type: &'static str, body: bool) -> Self {
        Self {
            content_type,
            declared: 1,
            body: vec![b'x'],
            stalled: Some((
                body,
                Arc::new(tokio::sync::Semaphore::new(0)),
                Arc::new(tokio::sync::Notify::new()),
            )),
            challenge: false,
        }
    }

    fn token_challenge() -> Self {
        Self {
            content_type: BLOB_TYPE,
            declared: 0,
            body: Vec::new(),
            stalled: None,
            challenge: true,
        }
    }
}

type Content = Arc<HashMap<String, Answer>>;

/// A registry keyed by request path, so references a run overlaps get the same answer however their
/// requests interleave.
async fn answer(content: Content, mut connection: TcpStream) {
    let path = read_path(&mut connection).await;
    let path = path.split('?').next().unwrap();
    let reply = &content[path];
    if reply.challenge {
        let head = format!(
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer realm=\"http://{}/token\",service=\"reg\",scope=\"repository:library/refused:pull\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            connection.local_addr().unwrap()
        );
        let _ = connection.write_all(head.as_bytes()).await;
        return;
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.content_type, reply.declared
    );
    if let Some((body, entered, release)) = &reply.stalled {
        if *body {
            let _ = connection.write_all(head.as_bytes()).await;
        }
        entered.add_permits(1);
        release.notified().await;
    } else {
        // peryx drops a body it will not finish reading, so the write ends in a closed pipe by design.
        let _ = connection.write_all(head.as_bytes()).await;
        let _ = connection.write_all(&reply.body).await;
    }
}

/// Serves `listener` for as long as `run` needs it, so the fixture leaves no accept loop behind.
async fn serve_until_done<T>(
    listener: TcpListener,
    content: Content,
    peers: &mut tokio::task::JoinSet<()>,
    run: impl Future<Output = T>,
) -> T {
    let mut run = Box::pin(run);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                peers.spawn(answer(Arc::clone(&content), accepted.unwrap().0));
            }
            outcome = &mut run => return outcome,
        }
    }
}

async fn finish_peers(peers: &mut tokio::task::JoinSet<()>) {
    peers.abort_all();
    while let Some(result) = tokio::time::timeout(Duration::from_secs(5), peers.join_next())
        .await
        .expect("mirror peer did not stop")
    {
        assert!(matches!(
            result.as_ref().map_err(tokio::task::JoinError::is_cancelled),
            Ok(()) | Err(true)
        ));
    }
}

fn manifest_path(repo: &str, reference: &str) -> String {
    format!("/v2/{repo}/manifests/{reference}")
}

fn blob_path(repo: &str, digest: &str) -> String {
    format!("/v2/{repo}/blobs/{digest}")
}

async fn synced_against(content: HashMap<String, Answer>, refs: &[String]) -> Vec<MirrorRow> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("http://{}/", listener.local_addr().unwrap()), false);
    let content = Arc::new(content);
    let stalled = content.values().find_map(|answer| {
        answer
            .stalled
            .as_ref()
            .map(|(_, entered, release)| (Arc::clone(entered), Arc::clone(release)))
    });
    let mut peers = tokio::task::JoinSet::new();
    let run = serve_until_done(
        listener,
        Arc::clone(&content),
        &mut peers,
        mirror(
            &state.serving,
            &state.serving.indexes[0],
            IndexSettings::default(),
            refs,
            MirrorMode::Sync,
        ),
    );
    if let Some((entered, release)) = stalled {
        let mut run = Box::pin(run);
        let (permit, outcome) = tokio::time::timeout(Duration::from_secs(40), async {
            tokio::join!(entered.acquire(), &mut run)
        })
        .await
        .expect("stalled mirror run did not time out");
        permit.unwrap().forget();
        drop(run);
        release.notify_waiters();
        finish_peers(&mut peers).await;
        outcome.unwrap()
    } else {
        let outcome = run.await;
        finish_peers(&mut peers).await;
        outcome.unwrap()
    }
}

/// A body of exactly the manifest ceiling is a legitimate manifest, not an abusive one: the bound
/// rejects a body that exceeds it, not one that just meets it.
#[tokio::test]
async fn test_mirror_accepts_a_manifest_body_at_exactly_the_ceiling() {
    let prefix = format!(r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[],"annotations":{{"pad":""#);
    let suffix = "\"}}";
    let padding = MAX_MANIFEST_BYTES - prefix.len() - suffix.len();
    let body = format!("{prefix}{}{suffix}", "x".repeat(padding)).into_bytes();
    assert_eq!(body.len(), MAX_MANIFEST_BYTES);
    let content = HashMap::from([(manifest_path("library/app", "latest"), Answer::whole(INDEX_TYPE, body))]);

    let rows = synced_against(content, &["library/app:latest".to_owned()]).await;

    assert_eq!(rows[0].status, "synced");
}

#[rstest]
#[case::before_headers(false)]
#[case::during_blob_body(true)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancelled_mirror_retries_a_blob_with_the_same_state(#[case] during_body: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = vec![b'c'; 2 * 1024 * 1024 + 1];
    let layer = vec![b'l'; 2 * 1024 * 1024 + 1];
    let manifest = image_manifest_with_layers(&config, &[&layer]);
    let content = Arc::new(HashMap::from([
        (
            manifest_path("library/app", "latest"),
            Answer::whole(MANIFEST_TYPE, manifest),
        ),
        (
            blob_path("library/app", &oci_digest(&config)),
            Answer::whole(BLOB_TYPE, config.clone()),
        ),
        (
            blob_path("library/app", &oci_digest(&layer)),
            Answer::whole(BLOB_TYPE, layer.clone()),
        ),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("http://{}/", listener.local_addr().unwrap()), false);
    let mirror_app = |state: Arc<peryx_driver::AppState>| async move {
        mirror(
            &state.serving,
            &state.serving.indexes[0],
            IndexSettings::default(),
            &["library/app:latest".to_owned()],
            MirrorMode::Sync,
        )
        .await
    };
    let cancelled = tokio::spawn(mirror_app(Arc::clone(&state)));
    let (manifest_connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("mirror manifest peer did not connect")
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        answer(Arc::clone(&content), manifest_connection),
    )
    .await
    .expect("mirror manifest peer did not complete");
    // The run pulls both blobs at once. Taking the sibling's request too leaves nothing from the
    // cancelled run in the backlog, where the recovery run would accept a connection the abort closed
    // before it wrote a byte.
    let mut in_flight = Vec::new();
    for _ in [&config, &layer] {
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("cancelled mirror blob peer did not connect")
            .unwrap();
        let path = tokio::time::timeout(Duration::from_secs(5), read_path(&mut connection))
            .await
            .expect("cancelled mirror blob peer did not send headers");
        in_flight.push((connection, path));
    }
    let (stalled, path) = &mut in_flight[0];
    let body = &content[path.as_str()].body;
    if during_body {
        stalled
            .write_all(
                format!("HTTP/1.1 200 OK\r\ncontent-type: {BLOB_TYPE}\r\ntransfer-encoding: chunked\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let prefix = &body[..2 * 1024 * 1024];
        stalled
            .write_all(format!("{:x}\r\n", prefix.len()).as_bytes())
            .await
            .unwrap();
        stalled.write_all(prefix).await.unwrap();
        stalled.write_all(b"\r\n").await.unwrap();
        wait_for_staged_bytes(&dir).await;
    }
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    assert_eq!(
        state
            .serving
            .blobs
            .head(&peryx_storage::blob::Digest::of(body))
            .await
            .unwrap(),
        None
    );
    drop(in_flight);

    let mut peers = tokio::task::JoinSet::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        serve_until_done(listener, Arc::clone(&content), &mut peers, mirror_app(state)),
    )
    .await;
    finish_peers(&mut peers).await;
    let rows = outcome.expect("mirror recovery did not complete").unwrap();

    assert_eq!(rows.last().unwrap().reason, "3 synced, 0 cached, 0 errors");
}

/// Both failures land after the response head, which is where the run used to abort with nothing
/// mirrored and no summary: the image selected behind the bad one was never even requested.
#[rstest]
#[case::a_body_over_the_manifest_ceiling(
    Answer::whole(MANIFEST_TYPE, vec![b'x'; MAX_MANIFEST_BYTES + 1]),
    "upstream transfer failed: upstream body exceeds 4194304 bytes"
)]
#[case::a_body_that_stops_mid_stream(
    Answer::truncated(MANIFEST_TYPE, br#"{"schemaVersion":2}"#.to_vec()),
    "upstream transfer failed: "
)]
#[case::before_headers(Answer::stalled(MANIFEST_TYPE, false), "upstream request timed out")]
#[case::during_body(Answer::stalled(MANIFEST_TYPE, true), "upstream request timed out")]
#[tokio::test]
async fn test_mirror_reports_a_manifest_body_failure_and_mirrors_the_image_behind_it(
    #[case] refused: Answer,
    #[case] expected: &str,
) {
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer = b"a-layer-of-bytes";
    let wanted = image_manifest_with_layers(config, &[layer]);
    let content = HashMap::from([
        (manifest_path("library/refused", "latest"), refused),
        (
            manifest_path("library/app", "latest"),
            Answer::whole(MANIFEST_TYPE, wanted),
        ),
        (
            blob_path("library/app", &oci_digest(config)),
            Answer::whole(BLOB_TYPE, config.to_vec()),
        ),
        (
            blob_path("library/app", &oci_digest(layer)),
            Answer::whole(BLOB_TYPE, layer.to_vec()),
        ),
    ]);

    let rows = synced_against(
        content,
        &["library/refused:latest".to_owned(), "library/app:latest".to_owned()],
    )
    .await;

    assert_eq!(
        rows.iter()
            .map(|row| (row.kind, row.repo.as_str(), row.status))
            .collect::<Vec<_>>(),
        [
            ("manifest", "library/refused", "error"),
            ("manifest", "library/app", "synced"),
            ("blob", "library/app", "synced"),
            ("blob", "library/app", "synced"),
            ("summary", "", "partial"),
        ]
    );
    assert_eq!(rows.last().unwrap().reason, "3 synced, 0 cached, 1 errors");
    let reason = &rows[0].reason;
    if expected == "upstream request timed out" {
        assert_eq!(reason, expected);
    } else {
        assert!(reason.starts_with(expected), "{reason}");
    }
}

#[rstest]
#[case::blob_body(false, "5 synced, 0 cached, 1 errors")]
#[case::token(true, "3 synced, 0 cached, 1 errors")]
#[tokio::test]
async fn test_mirror_reports_a_stalled_transfer_and_mirrors_the_image_behind_it(
    #[case] token: bool,
    #[case] summary: &str,
) {
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer = b"healthy-layer";
    let healthy = image_manifest_with_layers(config, &[layer]);
    let mut content = HashMap::from([
        (
            manifest_path("library/app", "latest"),
            Answer::whole(MANIFEST_TYPE, healthy),
        ),
        (
            blob_path("library/app", &oci_digest(config)),
            Answer::whole(BLOB_TYPE, config.to_vec()),
        ),
        (
            blob_path("library/app", &oci_digest(layer)),
            Answer::whole(BLOB_TYPE, layer.to_vec()),
        ),
    ]);
    if token {
        content.insert(manifest_path("library/refused", "latest"), Answer::token_challenge());
        content.insert("/token".to_owned(), Answer::stalled("application/json", true));
    } else {
        let refused_config = br#"{"architecture":"arm64","os":"linux"}"#;
        let stalled = b"stalled-layer";
        let refused = image_manifest_with_layers(refused_config, &[stalled]);
        content.insert(
            manifest_path("library/refused", "latest"),
            Answer::whole(MANIFEST_TYPE, refused),
        );
        content.insert(
            blob_path("library/refused", &oci_digest(refused_config)),
            Answer::whole(BLOB_TYPE, refused_config.to_vec()),
        );
        content.insert(
            blob_path("library/refused", &oci_digest(stalled)),
            Answer::stalled(BLOB_TYPE, true),
        );
    }

    let rows = synced_against(
        content,
        &["library/refused:latest".to_owned(), "library/app:latest".to_owned()],
    )
    .await;

    assert_eq!(
        rows.iter()
            .find(|row| row.repo == "library/refused" && row.status == "error")
            .unwrap()
            .reason,
        "upstream request timed out"
    );
    assert!(
        rows.iter()
            .any(|row| row.repo == "library/app" && row.status == "synced")
    );
    assert_eq!(rows.last().unwrap().reason, summary);
}

/// A child manifest whose body stops mid-stream is one error row of its level. The sibling the same
/// parent named is independent work, so the walk finishes the level and reports it in descriptor
/// order.
#[tokio::test]
async fn test_mirror_finishes_a_level_past_a_child_manifest_body_failure() {
    let refused = index_over(&[], "the child whose body stops");
    let wanted = index_over(&[], "the child beside it");
    let children = [oci_digest(&refused), oci_digest(&wanted)];
    let root = index_over(&children.iter().map(String::as_str).collect::<Vec<_>>(), "root");
    let content = HashMap::from([
        (manifest_path("library/app", "latest"), Answer::whole(INDEX_TYPE, root)),
        (
            manifest_path("library/app", &children[0]),
            Answer::truncated(INDEX_TYPE, refused),
        ),
        (
            manifest_path("library/app", &children[1]),
            Answer::whole(INDEX_TYPE, wanted),
        ),
    ]);

    let rows = synced_against(content, &["library/app:latest".to_owned()]).await;

    assert_eq!(
        rows.iter()
            .map(|row| (row.reference.as_str(), row.status))
            .collect::<Vec<_>>(),
        [
            ("latest", "synced"),
            (children[0].as_str(), "error"),
            (children[1].as_str(), "synced"),
            ("", "partial"),
        ]
    );
    assert_eq!(rows.last().unwrap().reason, "2 synced, 0 cached, 1 errors");
}
