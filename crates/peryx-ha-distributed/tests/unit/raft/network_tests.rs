use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::routing::post;
use serde::{Deserialize, Serialize};

use crate::raft::network::{
    DEFAULT_MAX_RPC_RESPONSE_BYTES, RaftRpc, RaftRpcClient, RaftRpcConfigError, RaftRpcError, RaftRpcHandler,
    RaftRpcRejection, raft_rpc_router,
};

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
struct WrongShape {
    note: String,
}

#[derive(Serialize)]
struct BigPing {
    n: u64,
    pad: String,
}

/// Decodes a [`Ping`] and answers a [`Pong`] tagged with the RPC it arrived on, so a round-trip proves
/// both the transport and that the request reached the endpoint its RPC names.
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

struct TestServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/"),
            task,
        }
    }

    async fn echo() -> Self {
        Self::start(raft_rpc_router(TOKEN, Arc::new(EchoHandler)).unwrap()).await
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn client(base: &str, token: &str) -> RaftRpcClient {
    RaftRpcClient::new(base, token, Duration::from_secs(5)).unwrap()
}

#[test]
fn test_new_rejects_an_empty_token() {
    let error = RaftRpcClient::new("http://peer.example/", "", Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::EmptyToken));
}

#[test]
fn test_new_rejects_an_unparseable_url() {
    let error = RaftRpcClient::new("not a url", TOKEN, Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::InvalidBase(_)));
}

#[test]
fn test_new_rejects_a_non_http_scheme() {
    let error = RaftRpcClient::new("ftp://peer.example/", TOKEN, Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::InvalidBase(_)));
}

#[test]
fn test_router_rejects_an_empty_token() {
    let error = raft_rpc_router("", Arc::new(EchoHandler)).unwrap_err();
    assert!(matches!(error, RaftRpcConfigError::EmptyToken));
}

#[test]
fn test_debug_redacts_the_token() {
    let rendered = format!("{:?}", client("http://peer.example/", TOKEN));
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
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
    let server = TestServer::echo().await;
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
async fn test_send_appends_the_endpoint_to_a_base_path() {
    // A base URL without a trailing slash and with its own path prefix: the client normalizes the base
    // and appends the RPC path, so the request lands on the peer's nested mount.
    let mounted = Router::new().nest("/mirror", raft_rpc_router(TOKEN, Arc::new(EchoHandler)).unwrap());
    let server = TestServer::start(mounted).await;
    let client = client(&format!("{}mirror", server.url), TOKEN);

    let pong: Pong = client.send(RaftRpc::Vote, &Ping { n: 3 }).await.unwrap();

    assert_eq!(
        pong,
        Pong {
            rpc: "vote".to_owned(),
            n: 3,
        }
    );
}

#[tokio::test]
async fn test_router_accepts_a_body_over_the_default_limit() {
    // axum defaults a request body to 2 MiB; the router raises it so a large append-entries batch or
    // snapshot chunk is accepted instead of 413'd into a terminal client error. The echo handler decodes
    // the leading field and ignores the padding.
    let server = TestServer::echo().await;
    let client = client(&server.url, TOKEN);
    let big = BigPing {
        n: 9,
        pad: "x".repeat(3 * 1024 * 1024),
    };

    let pong: Pong = client.send(RaftRpc::AppendEntries, &big).await.unwrap();

    assert_eq!(pong.n, 9);
}

#[tokio::test]
async fn test_send_maps_an_auth_reject_to_unauthenticated() {
    let server = TestServer::echo().await;
    let client = client(&server.url, "wrong");

    let error = client.send::<_, Pong>(RaftRpc::Vote, &Ping { n: 1 }).await.unwrap_err();

    assert_eq!(error, RaftRpcError::Unauthenticated);
}

#[tokio::test]
async fn test_send_maps_a_malformed_request_to_a_remote_bad_request() {
    // The endpoint cannot decode the body as a Vote request, so it answers 400 and the client surfaces
    // the remote status rather than trying to decode a response that never came.
    let server = TestServer::echo().await;
    let client = client(&server.url, TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::Vote, &WrongShape { note: "x".to_owned() })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::RemoteError { status: 400 });
}

#[tokio::test]
async fn test_send_maps_a_server_error_to_a_remote_status() {
    let router = Router::new().route(
        &format!("/{}", "+replication/v1/raft/vote"),
        post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let server = TestServer::start(router).await;
    let client = client(&server.url, TOKEN);

    let error = client.send::<_, Pong>(RaftRpc::Vote, &Ping { n: 1 }).await.unwrap_err();

    assert_eq!(error, RaftRpcError::RemoteError { status: 500 });
}

#[tokio::test]
async fn test_send_maps_a_non_decodable_reply_to_malformed() {
    let router = Router::new().route(
        &format!("/{}", "+replication/v1/raft/vote"),
        post(|| async { "not a pong" }),
    );
    let server = TestServer::start(router).await;
    let client = client(&server.url, TOKEN);

    let error = client.send::<_, Pong>(RaftRpc::Vote, &Ping { n: 1 }).await.unwrap_err();

    assert_eq!(error, RaftRpcError::Malformed);
}

#[tokio::test]
async fn test_send_caps_an_oversized_reply() {
    let server = TestServer::echo().await;
    let client = client(&server.url, TOKEN).with_response_cap(4);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 7 })
        .await
        .unwrap_err();

    let RaftRpcError::ResponseTooLarge { limit, actual } = error else {
        panic!("expected an oversized-reply rejection, got {error:?}");
    };
    assert_eq!(limit, 4);
    assert!(actual > 4);
}

#[tokio::test]
async fn test_send_maps_a_refused_connection_to_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Hold the port and reset every connection so the send always fails at the transport. Dropping the
    // listener would free the port for a parallel test to rebind and answer, flaking the loss away.
    let _reset = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let client = client(&format!("http://{address}/"), TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unreachable);
}

#[tokio::test]
async fn test_send_maps_a_truncated_body_to_unreachable() {
    // The peer answers 200 with a Content-Length it never delivers, then closes. The status read
    // succeeds, so the failure surfaces mid-body: reading the next chunk fails and maps to unreachable,
    // the transport-loss arm the body read shares with the initial send.
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await;
        }
    });
    let client = client(&format!("http://{address}/"), TOKEN);

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Unreachable);
    task.abort();
}

#[tokio::test]
async fn test_send_maps_a_deadline_to_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _held = stream;
            std::future::pending::<()>().await;
        }
    });
    let client = RaftRpcClient::new(&format!("http://{address}/"), TOKEN, Duration::from_millis(200)).unwrap();

    let error = client
        .send::<_, Pong>(RaftRpc::AppendEntries, &Ping { n: 1 })
        .await
        .unwrap_err();

    assert_eq!(error, RaftRpcError::Timeout);
    task.abort();
}

#[test]
fn test_default_response_cap_is_the_documented_bound() {
    assert_eq!(DEFAULT_MAX_RPC_RESPONSE_BYTES, 64 * 1024 * 1024);
}

#[tokio::test]
#[should_panic(expected = "a raft rpc request serializes")]
async fn test_send_panics_when_the_request_will_not_serialize() {
    // A request whose Serialize impl fails cannot encode. `send` documents this as a caller bug the
    // OpenRaft RPC types never provoke, so it panics on the encode rather than fabricating a transport
    // error. The panic fires before any dial, so no server is needed.
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
    use openraft::error::{Fatal, InstallSnapshotError, RPCError, RaftError};
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

    /// A peer voter that ignores the request and answers each RPC a canned success or, when
    /// `remote_error` is set, a serialized `RaftError` so the client's remote-error mapping runs.
    struct StubVoter {
        remote_error: bool,
    }

    /// Serialize `Result<Resp, RaftError>`. `RaftError<_, Infallible>` (the append/vote error) is not
    /// `Serialize`, but the wire JSON is identical across error types and the client deserializes it as
    /// its own error, so a serializable error type stands in here.
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
            .new_client(1, &node(addr))
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
    async fn test_a_remote_raft_error_maps_to_a_network_error() {
        let server = stub_server(true).await;
        let mut network = client_to(&peer_addr(&server.url)).await;
        let error = network
            .vote(vote_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn test_a_remote_snapshot_error_maps_to_a_network_error() {
        let server = stub_server(true).await;
        let mut network = client_to(&peer_addr(&server.url)).await;
        let error = network
            .install_snapshot(snapshot_req(), RPCOption::new(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(error, RPCError::Network(_)), "{error:?}");
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
        // openraft builds a peer client lazily inside the replication task, so a bad roster addr must
        // fail that peer's RPCs retryably rather than panic the task and abort the node. A space is not a
        // legal host, so the dialed base never forms and every RPC to this peer reports Unreachable.
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
