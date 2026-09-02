//! What a mirror run does with a manifest body it cannot read to the end: one error row for that
//! reference, and everything selected or scheduled beside it is mirrored anyway.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use rstest::rstest;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

use super::mirror_concurrency_tests::read_path;
use super::mirror_tests::{INDEX_TYPE, MANIFEST_TYPE, image_manifest_with_layers, index_over};
use super::{oci_digest, proxy};
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
}

impl Answer {
    fn whole(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type,
            declared: body.len(),
            body,
        }
    }

    fn truncated(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type,
            declared: body.len() + 1,
            body,
        }
    }
}

type Content = Arc<HashMap<String, Answer>>;

/// A registry keyed by request path, so references a run overlaps get the same answer however their
/// requests interleave.
async fn answer(content: Content, mut connection: TcpStream) {
    let path = read_path(&mut connection).await;
    let reply = &content[&path];
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.content_type, reply.declared
    );
    // peryx drops a body it will not finish reading, so the write ends in a closed pipe by design.
    let _ = connection.write_all(head.as_bytes()).await;
    let _ = connection.write_all(&reply.body).await;
}

/// Serves `listener` for as long as `run` needs it, so the fixture leaves no accept loop behind.
async fn serve_until_done<T>(listener: TcpListener, content: &Content, run: impl Future<Output = T>) -> T {
    let mut run = Box::pin(run);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                tokio::spawn(answer(Arc::clone(content), accepted.unwrap().0));
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

async fn synced_against(content: HashMap<String, Answer>, refs: &[String]) -> Vec<MirrorRow> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, &format!("http://{}/", listener.local_addr().unwrap()), false);
    serve_until_done(
        listener,
        &Arc::new(content),
        mirror(
            &state.serving,
            &state.serving.indexes[0],
            IndexSettings::default(),
            refs,
            MirrorMode::Sync,
        ),
    )
    .await
    .unwrap()
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
    assert!(reason.starts_with(expected), "{reason}");
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
