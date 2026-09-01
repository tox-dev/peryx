use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderValue;
use axum::response::IntoResponse;
use rstest::rstest;
use serde::{Deserialize, Serialize};

use crate::raft::network::{
    DEFAULT_MAX_RPC_RESPONSE_BYTES, RaftRpc, RaftRpcClient, RaftRpcConfigError, RaftRpcError, RaftRpcHandler,
    RaftRpcRejection, raft_rpc_router,
};
use crate::support::{RequestBlocker, TestServer, http_contract};

const TOKEN: &str = "secret";
const LOCAL: u64 = 3;
/// Outlasts every deadline the tests grant, so a call always ends on its own bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Ping {
    n: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Pong {
    rpc: String,
    n: u64,
}

#[derive(Serialize)]
struct BigPing {
    n: u64,
    pad: String,
}

struct EchoHandler;

#[async_trait::async_trait]
impl RaftRpcHandler for EchoHandler {
    async fn handle(&self, rpc: RaftRpc, body: Bytes) -> Result<Vec<u8>, RaftRpcRejection> {
        let ping: Ping = serde_json::from_slice(&body).map_err(|_| RaftRpcRejection::Malformed)?;
        Ok(serde_json::to_vec(&Pong {
            rpc: rpc.as_str().to_owned(),
            n: ping.n,
        })
        .unwrap())
    }
}

struct BlockedHandler {
    blocker: RequestBlocker,
}

#[async_trait::async_trait]
impl RaftRpcHandler for BlockedHandler {
    async fn handle(&self, _rpc: RaftRpc, _body: Bytes) -> Result<Vec<u8>, RaftRpcRejection> {
        self.blocker.wait().await
    }
}

fn client(base: &str, token: &str) -> RaftRpcClient {
    RaftRpcClient::new(LOCAL, base, token, CONNECT_TIMEOUT).unwrap()
}

async fn echo_server() -> TestServer {
    TestServer::start(raft_rpc_router(LOCAL, TOKEN, Arc::new(EchoHandler)).unwrap()).await
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| RaftRpcClient::new(LOCAL, base, token, CONNECT_TIMEOUT).map(|_| ()),
        |error| matches!(error, RaftRpcConfigError::EmptyToken),
        |error| matches!(error, RaftRpcConfigError::InvalidBase(_)),
    );
}

#[test]
fn test_debug_names_the_response_bound_without_the_token() {
    http_contract::assert_redacted(
        &client("http://peer.example/root", TOKEN),
        TOKEN,
        &["RaftRpcClient", "target", "max_response_bytes"],
    );
}

#[test]
fn test_router_rejects_an_empty_token() {
    let error = raft_rpc_router(LOCAL, "", Arc::new(EchoHandler)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::EmptyToken));
}

#[test]
fn test_rpc_labels_are_stable() {
    assert_eq!(RaftRpc::AppendEntries.as_str(), "append_entries");
    assert_eq!(RaftRpc::ClientWrite.as_str(), "client_write");
    assert_eq!(RaftRpc::Vote.as_str(), "vote");
    assert_eq!(RaftRpc::InstallSnapshot.as_str(), "install_snapshot");
}

async fn assert_round_trip(rpc: RaftRpc, label: &str) {
    let server = echo_server().await;
    let client = client(&server.url, TOKEN);

    let pong: Pong = client.send(rpc, &Ping { n: 7 }, DEADLINE).await.unwrap();

    assert_eq!(
        pong,
        Pong {
            rpc: label.to_owned(),
            n: 7,
        }
    );
}

#[tokio::test]
async fn test_send_round_trips_append_entries_to_its_endpoint() {
    assert_round_trip(RaftRpc::AppendEntries, "append_entries").await;
}

#[tokio::test]
async fn test_send_round_trips_client_write_to_its_endpoint() {
    assert_round_trip(RaftRpc::ClientWrite, "client_write").await;
}

#[tokio::test]
async fn test_send_round_trips_vote_to_its_endpoint() {
    assert_round_trip(RaftRpc::Vote, "vote").await;
}

#[tokio::test]
async fn test_send_round_trips_install_snapshot_to_its_endpoint() {
    assert_round_trip(RaftRpc::InstallSnapshot, "install_snapshot").await;
}

#[tokio::test]
async fn test_send_maps_a_rejected_token_to_unauthenticated() {
    let server = echo_server().await;

    let error = client(&server.url, "wrong")
        .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 7 }, DEADLINE)
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unauthenticated);
}

#[tokio::test]
async fn test_send_maps_an_rpc_the_peer_does_not_answer_for_to_a_target_mismatch() {
    let server = echo_server().await;
    let misdirected = RaftRpcClient::new(LOCAL + 1, &server.url, TOKEN, CONNECT_TIMEOUT).unwrap();

    let error = misdirected
        .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 7 }, DEADLINE)
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::TargetMismatch);
}

#[rstest]
#[case::unaddressed(None)]
#[case::not_a_voter_id(Some(HeaderValue::from_static("east")))]
#[case::not_text(Some(HeaderValue::from_bytes(&[0xff]).unwrap()))]
#[case::another_voter(Some(HeaderValue::from_static("9")))]
#[tokio::test]
async fn test_the_router_answers_only_for_the_voter_it_holds(#[case] target: Option<HeaderValue>) {
    let server = echo_server().await;
    let request = reqwest::Client::new()
        .post(format!("{}+replication/v1/raft/vote", server.url))
        .bearer_auth(TOKEN)
        .body(serde_json::to_vec(&Ping { n: 7 }).unwrap());

    let status = target
        .into_iter()
        .fold(request, |request, value| request.header("x-peryx-raft-target", value))
        .send()
        .await
        .unwrap()
        .status();

    assert_eq!(status, reqwest::StatusCode::MISDIRECTED_REQUEST);
}

/// A caller that fails the group credential learns nothing about which voter the process holds.
#[tokio::test]
async fn test_an_unauthenticated_misdirected_rpc_reports_only_the_credential_failure() {
    let server = echo_server().await;
    let misdirected = RaftRpcClient::new(LOCAL + 1, &server.url, "wrong", CONNECT_TIMEOUT).unwrap();

    let error = misdirected
        .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 7 }, DEADLINE)
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unauthenticated);
}

#[tokio::test]
async fn test_router_rejects_a_malformed_request() {
    let server = echo_server().await;

    let status = reqwest::Client::new()
        .post(format!("{}+replication/v1/raft/vote", server.url))
        .bearer_auth(TOKEN)
        .header("x-peryx-raft-target", LOCAL)
        .body("not json")
        .send()
        .await
        .unwrap()
        .status();

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_router_accepts_a_body_over_the_default_limit() {
    let server = echo_server().await;
    let client = client(&server.url, TOKEN);
    let big = BigPing {
        n: 9,
        pad: "x".repeat(3 * 1024 * 1024),
    };

    let pong: Pong = client.send(RaftRpc::AppendEntries, &big, DEADLINE).await.unwrap();

    assert_eq!(pong.n, 9);
}

#[tokio::test]
async fn test_send_maps_a_non_decodable_reply_to_malformed() {
    http_contract::assert_mapping(
        http_contract::fixed_post("/+replication/v1/raft/vote", || "not a pong".into_response()),
        |base| async move {
            client(&base, TOKEN)
                .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 1 }, DEADLINE)
                .await
        },
        Err(RaftRpcError::Malformed),
    )
    .await;
}

#[tokio::test]
async fn test_send_caps_an_oversized_reply() {
    let server = echo_server().await;
    let client = client(&server.url, TOKEN).with_response_cap(4);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 7 }, DEADLINE)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RaftRpcError::ResponseTooLarge { limit: 4, actual } if actual > 4
    ));
}

#[tokio::test]
async fn test_send_maps_a_refused_connection_to_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Keep the port bound so another test cannot replace the failing peer.
    let reset = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });
    let client = client(&format!("http://{address}/"), TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 }, DEADLINE)
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unreachable);
    reset.await.unwrap();
}

#[tokio::test]
async fn test_send_maps_a_truncated_body_to_unreachable() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        assert_ne!(stream.read(&mut request).await.unwrap(), 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
    });
    let client = client(&format!("http://{address}/"), TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 }, DEADLINE)
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unreachable);
    task.await.unwrap();
}

/// Under a paused clock the elapsed span is the deadline the call was handed, so a client-wide bound
/// would show up as an unrelated number rather than as a slow test.
#[rstest]
#[case::under_the_old_client_bound(Duration::from_secs(1))]
#[case::over_the_old_client_bound(Duration::from_secs(9))]
#[tokio::test(start_paused = true)]
async fn test_send_gives_up_on_a_silent_peer_at_the_deadline_it_was_handed(#[case] deadline: Duration) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (_connection, _) = listener.accept().await.unwrap();
        release_rx.await.unwrap();
    });
    let client = client(&format!("http://{address}/"), TOKEN);
    let start = tokio::time::Instant::now();

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 }, deadline)
        .await
        .unwrap_err();

    let elapsed = start.elapsed().as_secs();
    assert_eq!((error, elapsed), (RaftRpcError::Timeout, deadline.as_secs()));
    release_tx.send(()).unwrap();
    task.await.unwrap();
}

/// A peer still working on the request learns the caller left, so it can stop instead of persisting a
/// reply nobody will read.
#[tokio::test]
async fn test_a_deadline_abandons_the_request_the_peer_is_still_serving() {
    let (blocker, entered, dropped) = RequestBlocker::new();
    let server = TestServer::start(raft_rpc_router(LOCAL, TOKEN, Arc::new(BlockedHandler { blocker })).unwrap()).await;
    let client = client(&server.url, TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::InstallSnapshot, &Ping { n: 1 }, Duration::from_millis(50))
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Timeout);
    entered.await.unwrap();
    dropped.await.unwrap();
}

/// A reply that lands inside the deadline still resolves the call.
#[tokio::test]
async fn test_a_generous_deadline_still_answers_the_call() {
    let server = echo_server().await;

    let pong: Pong = client(&server.url, TOKEN)
        .send(RaftRpc::InstallSnapshot, &Ping { n: 4 }, Duration::from_mins(1))
        .await
        .unwrap();

    assert_eq!(
        pong,
        Pong {
            rpc: "install_snapshot".to_owned(),
            n: 4,
        }
    );
}

#[test]
fn test_default_response_cap_is_the_documented_bound() {
    assert_eq!(DEFAULT_MAX_RPC_RESPONSE_BYTES, 64 * 1024 * 1024);
}

#[tokio::test]
#[should_panic(expected = "a raft rpc request serializes")]
async fn test_send_panics_when_the_request_will_not_serialize() {
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("this request never serializes"))
        }
    }

    let client = client("http://peer.example/", TOKEN);
    let _: Result<Pong, RaftRpcError> = client.send(RaftRpc::Vote, &Unserializable, DEADLINE).await;
}

mod adapter {
    use std::time::Duration;

    use axum::body::Bytes;
    use openraft::Vote;
    use openraft::error::{Fatal, InstallSnapshotError, RPCError, RaftError, RemoteError, Timeout};
    use openraft::network::{RPCOption, RPCTypes, RaftNetwork, RaftNetworkFactory};
    use openraft::raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse, VoteRequest,
        VoteResponse,
    };
    use openraft::storage::SnapshotMeta;
    use serde::Serialize;

    use super::{TOKEN, TestServer};
    use crate::DatacenterId;
    use crate::raft::network::{PeerRaftNetwork, PeerRaftNetworkFactory, RaftRpc, RaftRpcHandler, RaftRpcRejection};
    use crate::raft::{PeryxNode, TypeConfig};
    use std::sync::Arc;

    type NodeId = u64;
    const TARGET: NodeId = 7;
    const LOCAL_VOTER: NodeId = 3;
    /// Outlasts every deadline these tests grant.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

    struct StubVoter {
        remote_error: bool,
    }

    /// Uses a serializable error type with the same wire representation.
    fn wire<Resp: Serialize>(remote_error: bool, ok: Resp) -> Vec<u8> {
        let reply: Result<Resp, RaftError<NodeId, InstallSnapshotError>> = if remote_error {
            Err(RaftError::Fatal(Fatal::Panicked))
        } else {
            Ok(ok)
        };
        serde_json::to_vec(&reply).unwrap()
    }

    #[async_trait::async_trait]
    impl RaftRpcHandler for StubVoter {
        async fn handle(&self, rpc: RaftRpc, _body: Bytes) -> Result<Vec<u8>, RaftRpcRejection> {
            let vote = Vote::new(1, 1);
            let bytes = match rpc {
                RaftRpc::AppendEntries => wire(self.remote_error, AppendEntriesResponse::<NodeId>::Success),
                RaftRpc::ClientWrite => unreachable!("the OpenRaft adapter never sends client writes"),
                RaftRpc::Vote => wire(self.remote_error, VoteResponse::new(vote, None, true)),
                RaftRpc::InstallSnapshot => wire(self.remote_error, InstallSnapshotResponse { vote }),
            };
            Ok(bytes)
        }
    }

    fn node(endpoint: &str) -> PeryxNode {
        PeryxNode {
            datacenter: DatacenterId::default(),
            endpoint: endpoint.to_owned(),
        }
    }

    async fn client_to(endpoint: &str) -> PeerRaftNetwork {
        PeerRaftNetworkFactory::new(LOCAL_VOTER, TOKEN, CONNECT_TIMEOUT)
            .new_client(TARGET, &node(endpoint))
            .await
    }

    async fn stub_server(remote_error: bool) -> TestServer {
        TestServer::start(
            crate::raft::network::raft_rpc_router(TARGET, TOKEN, Arc::new(StubVoter { remote_error })).unwrap(),
        )
        .await
    }

    fn append_req() -> AppendEntriesRequest<TypeConfig> {
        AppendEntriesRequest {
            vote: Vote::new(1, 1),
            prev_log_id: None,
            entries: vec![],
            leader_commit: None,
        }
    }

    fn vote_req() -> VoteRequest<NodeId> {
        VoteRequest::new(Vote::new(1, 1), None)
    }

    fn snapshot_req() -> InstallSnapshotRequest<TypeConfig> {
        InstallSnapshotRequest {
            vote: Vote::new(1, 1),
            meta: SnapshotMeta::default(),
            offset: 0,
            data: vec![],
            done: true,
        }
    }

    #[tokio::test]
    async fn test_append_entries_round_trips_a_success() {
        let server = stub_server(false).await;
        let mut network = client_to(&server.url).await;
        let response = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(matches!(response, AppendEntriesResponse::Success));
    }

    #[tokio::test]
    async fn test_vote_round_trips_a_grant() {
        let server = stub_server(false).await;
        let mut network = client_to(&server.url).await;
        let response = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(response.vote_granted);
    }

    #[tokio::test]
    async fn test_install_snapshot_round_trips_a_response() {
        let server = stub_server(false).await;
        let mut network = client_to(&server.url).await;
        let response = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(response.vote, Vote::new(1, 1));
    }

    /// Accepts the connection and never answers, so the call can only end on its own deadline.
    async fn silent_peer() -> (String, tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            released.await.unwrap();
        });
        (format!("http://{address}/"), release, task)
    }

    /// `OpenRaft` grants a vote the election window, far shorter than the budget it grants a snapshot
    /// chunk, and expects the transport to start cancelling once the soft TTL passes.
    #[tokio::test(start_paused = true)]
    async fn test_a_vote_ends_on_the_ttl_of_its_own_option() {
        let (url, release, task) = silent_peer().await;
        let mut network = client_to(&url).await;
        let option = RPCOption::new(Duration::from_secs(4));
        let deadline = option.soft_ttl();
        let start = tokio::time::Instant::now();

        let error = network.vote(vote_req(), option).await.unwrap_err();

        assert_eq!(
            (error, start.elapsed().as_secs()),
            (
                RPCError::Timeout(Timeout {
                    action: RPCTypes::Vote,
                    id: LOCAL_VOTER,
                    target: TARGET,
                    timeout: deadline,
                }),
                deadline.as_secs()
            )
        );
        release.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_an_append_ends_on_the_ttl_of_its_own_option() {
        let (url, release, task) = silent_peer().await;
        let mut network = client_to(&url).await;
        let option = RPCOption::new(Duration::from_secs(8));
        let deadline = option.soft_ttl();
        let start = tokio::time::Instant::now();

        let error = network.append_entries(append_req(), option).await.unwrap_err();

        assert_eq!(
            (error, start.elapsed().as_secs()),
            (
                RPCError::Timeout(Timeout {
                    action: RPCTypes::AppendEntries,
                    id: LOCAL_VOTER,
                    target: TARGET,
                    timeout: deadline,
                }),
                deadline.as_secs()
            )
        );
        release.send(()).unwrap();
        task.await.unwrap();
    }

    /// The chunked transport retries a chunk in place after a `Timeout`; the whole transfer restarts at
    /// offset zero when the hard TTL elapses around the call instead.
    #[tokio::test(start_paused = true)]
    async fn test_a_snapshot_chunk_ends_on_the_ttl_of_its_own_option() {
        let (url, release, task) = silent_peer().await;
        let mut network = client_to(&url).await;
        let option = RPCOption::new(Duration::from_secs(12));
        let deadline = option.soft_ttl();
        let start = tokio::time::Instant::now();

        let error = network.install_snapshot(snapshot_req(), option).await.unwrap_err();

        assert_eq!(
            (error, start.elapsed().as_secs()),
            (
                RPCError::Timeout(Timeout {
                    action: RPCTypes::InstallSnapshot,
                    id: LOCAL_VOTER,
                    target: TARGET,
                    timeout: deadline,
                }),
                deadline.as_secs()
            )
        );
        release.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_a_remote_raft_error_retains_its_target() {
        let server = stub_server(true).await;
        let address = server.url.clone();
        let mut network = client_to(&address).await;
        let error = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RPCError::RemoteError(RemoteError::new_with_node(
                TARGET,
                node(&address),
                RaftError::Fatal(Fatal::Panicked)
            ))
        );
    }

    #[tokio::test]
    async fn test_a_remote_snapshot_error_retains_its_target() {
        let server = stub_server(true).await;
        let address = server.url.clone();
        let mut network = client_to(&address).await;
        let error = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RPCError::RemoteError(RemoteError::new_with_node(
                TARGET,
                node(&address),
                RaftError::Fatal(Fatal::Panicked)
            ))
        );
    }

    #[tokio::test]
    async fn test_an_unreachable_peer_maps_to_unreachable() {
        let mut network = client_to("http://127.0.0.1:1/").await;
        let error = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_an_unreachable_peer_maps_a_snapshot_rpc_to_unreachable() {
        let mut network = client_to("http://127.0.0.1:1/").await;
        let error = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }

    async fn error_server() -> TestServer {
        use axum::routing::post;
        let router = axum::Router::new()
            .route(
                &format!("/{}", "+replication/v1/raft/vote"),
                post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                &format!("/{}", "+replication/v1/raft/install-snapshot"),
                post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            );
        TestServer::start(router).await
    }

    #[tokio::test]
    async fn test_a_remote_status_maps_a_vote_to_a_network_error() {
        let server = error_server().await;
        let mut network = client_to(&server.url).await;
        let error = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_a_remote_status_maps_a_snapshot_to_a_network_error() {
        let server = error_server().await;
        let mut network = client_to(&server.url).await;
        let error = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_a_malformed_addr_fails_the_rpc_unreachable_without_panicking() {
        // OpenRaft creates peer clients lazily; invalid addresses must remain retryable.
        let mut network = client_to("bad host").await;
        let error = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }

    /// The plaintext stub answers `http://` but never completes a TLS handshake, so a successful append
    /// would mean the transport silently downgraded a configured `https://` peer.
    #[tokio::test]
    async fn test_an_https_peer_is_never_dialed_over_plaintext() {
        let server = stub_server(false).await;
        let mut network = client_to(&server.url.replace("http://", "https://")).await;

        let error = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();

        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_a_malformed_addr_fails_a_snapshot_rpc_unreachable() {
        let mut network = client_to("bad host").await;
        let error = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }
}
