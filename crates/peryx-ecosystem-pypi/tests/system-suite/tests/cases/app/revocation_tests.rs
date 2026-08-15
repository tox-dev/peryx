use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::app::revocation;
use crate::cli::{AdministratorClientArgs, PutRevocationArgs, RevocationCommand};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "administrator password";

#[test]
fn test_revocation_client_puts_json_with_basic_auth_and_bounded_stdin_secret() {
    let (runtime, server) = runtime_and_server();
    runtime.block_on(
        Mock::given(method("PUT"))
            .and(path(format!("/+revocations/{DIGEST}")))
            .and(header("authorization", authorization()))
            .and(body_json(serde_json::json!({"reason": "incident"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"revision": 1})))
            .expect(1)
            .mount(&server),
    );
    let command = RevocationCommand::Put(PutRevocationArgs {
        client: client(&server),
        digest: DIGEST.to_owned(),
        reason: "incident".to_owned(),
    });
    let mut output = Vec::new();

    revocation(&command, &mut Cursor::new(format!("{PASSWORD}\r\n")), &mut output).unwrap();

    assert_eq!(output, b"{\"revision\":1}\n");
    assert!(!String::from_utf8(output).unwrap().contains(PASSWORD));
}

fn runtime_and_server() -> (tokio::runtime::Runtime, MockServer) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.block_on(MockServer::start());
    (runtime, server)
}

fn client(server: &MockServer) -> AdministratorClientArgs {
    AdministratorClientArgs {
        server: server.uri(),
        user: "Alice".to_owned(),
        password_stdin: true,
        password_file: None,
    }
}

fn authorization() -> String {
    format!("Basic {}", STANDARD.encode(format!("Alice:{PASSWORD}")))
}
