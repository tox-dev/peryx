use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::raft::network::{
    DEFAULT_MAX_RPC_RESPONSE_BYTES, RaftRpc, RaftRpcClient, RaftRpcConfigError, RaftRpcError, RaftRpcHandler,
    RaftRpcRejection, raft_rpc_router,
};
use crate::support::{TestServer, http_contract};

const TOKEN: &str = "secret";

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

fn client(base: &str, token: &str) -> RaftRpcClient {
    RaftRpcClient::new(base, token, Duration::from_secs(5)).unwrap()
}

async fn echo_server() -> TestServer {
    TestServer::start(raft_rpc_router(TOKEN, Arc::new(EchoHandler)).unwrap()).await
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| RaftRpcClient::new(base, token, Duration::from_secs(5)).map(|_| ()),
        |error| matches!(error, RaftRpcConfigError::EmptyToken),
        |error| matches!(error, RaftRpcConfigError::InvalidBase(_)),
    );
}

#[test]
fn test_debug_names_the_response_bound_without_the_token() {
    http_contract::assert_redacted(
        &client("http://peer.example/root", TOKEN),
        TOKEN,
        &["RaftRpcClient", "max_response_bytes"],
    );
}

#[test]
fn test_router_rejects_an_empty_token() {
    let error = raft_rpc_router("", Arc::new(EchoHandler)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::EmptyToken));
}

#[test]
fn test_rpc_labels_are_stable() {
    assert_eq!(RaftRpc::AppendEntries.as_str(), "append_entries");
    assert_eq!(RaftRpc::Vote.as_str(), "vote");
    assert_eq!(RaftRpc::InstallSnapshot.as_str(), "install_snapshot");
}

#[test]
fn test_only_a_transport_loss_reads_as_unreachable() {
    assert!(RaftRpcError::Unreachable.is_unreachable());
    assert!(RaftRpcError::Timeout.is_unreachable());
    assert!(!RaftRpcError::Unauthenticated.is_unreachable());
    assert!(!RaftRpcError::RemoteError { status: 500 }.is_unreachable());
    assert!(!RaftRpcError::ResponseTooLarge { limit: 1, actual: 2 }.is_unreachable());
    assert!(!RaftRpcError::Malformed.is_unreachable());
}

async fn assert_round_trip(rpc: RaftRpc, label: &str) {
    let server = echo_server().await;
    let client = client(&server.url, TOKEN);

    let pong: Pong = client.send(rpc, &Ping { n: 7 }).await.unwrap();

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
        .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 7 })
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

    let pong: Pong = client.send(RaftRpc::AppendEntries, &big).await.unwrap();

    assert_eq!(pong.n, 9);
}

#[tokio::test]
async fn test_send_maps_a_non_decodable_reply_to_malformed() {
    http_contract::assert_mapping(
        http_contract::fixed_post("/+replication/v1/raft/vote", || "not a pong".into_response()),
        |base| async move {
            client(&base, TOKEN)
                .send::<_, Pong>(RaftRpc::Vote, &Ping { n: 1 })
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
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 7 })
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
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
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
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unreachable);
    task.await.unwrap();
}

#[tokio::test]
async fn test_send_maps_a_deadline_to_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (_connection, _) = listener.accept().await.unwrap();
        release_rx.await.unwrap();
    });
    let client = RaftRpcClient::new(&format!("http://{address}/"), TOKEN, Duration::from_millis(200)).unwrap();

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Timeout);
    release_tx.send(()).unwrap();
    task.await.unwrap();
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
    let _: Result<Pong, RaftRpcError> = client.send(RaftRpc::Vote, &Unserializable).await;
}

mod adapter {
    use std::time::Duration;

    use axum::body::Bytes;
    use openraft::Vote;
    use openraft::error::{Fatal, InstallSnapshotError, RPCError, RaftError, RemoteError};
    use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
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
                RaftRpc::Vote => wire(self.remote_error, VoteResponse::new(vote, None, true)),
                RaftRpc::InstallSnapshot => wire(self.remote_error, InstallSnapshotResponse { vote }),
            };
            Ok(bytes)
        }
    }

    fn node(addr: &str) -> PeryxNode {
        PeryxNode {
            datacenter: DatacenterId::default(),
            addr: addr.to_owned(),
        }
    }

    fn peer_addr(url: &str) -> String {
        url.trim_start_matches("http://").trim_end_matches('/').to_owned()
    }

    async fn client_to(addr: &str) -> PeerRaftNetwork {
        PeerRaftNetworkFactory::new(TOKEN, Duration::from_secs(5))
            .new_client(TARGET, &node(addr))
            .await
    }

    async fn stub_server(remote_error: bool) -> TestServer {
        TestServer::start(crate::raft::network::raft_rpc_router(TOKEN, Arc::new(StubVoter { remote_error })).unwrap())
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
        let mut network = client_to(&peer_addr(&server.url)).await;
        let response = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(matches!(response, AppendEntriesResponse::Success));
    }

    #[tokio::test]
    async fn test_vote_round_trips_a_grant() {
        let server = stub_server(false).await;
        let mut network = client_to(&peer_addr(&server.url)).await;
        let response = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(response.vote_granted);
    }

    #[tokio::test]
    async fn test_install_snapshot_round_trips_a_response() {
        let server = stub_server(false).await;
        let mut network = client_to(&peer_addr(&server.url)).await;
        let response = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(response.vote, Vote::new(1, 1));
    }

    #[tokio::test]
    async fn test_a_remote_raft_error_retains_its_target() {
        let server = stub_server(true).await;
        let address = peer_addr(&server.url);
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
        let address = peer_addr(&server.url);
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
        let mut network = client_to("127.0.0.1:1").await;
        let error = network
            .append_entries(append_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Unreachable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_an_unreachable_peer_maps_a_snapshot_rpc_to_unreachable() {
        let mut network = client_to("127.0.0.1:1").await;
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
        let mut network = client_to(&peer_addr(&server.url)).await;
        let error = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_a_remote_status_maps_a_snapshot_to_a_network_error() {
        let server = error_server().await;
        let mut network = client_to(&peer_addr(&server.url)).await;
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
