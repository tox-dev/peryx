use std::io::Cursor;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rstest::rstest;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::FailImmediately;
use crate::app::revocation;
use crate::cli::{
    AdministratorClientArgs, InspectRevocationArgs, LiftRevocationArgs, ListRevocationsArgs, PutRevocationArgs,
    RevocationCommand, RevocationStatusArg,
};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "administrator password";

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

fn inspect(client: AdministratorClientArgs) -> RevocationCommand {
    RevocationCommand::Inspect(InspectRevocationArgs {
        client,
        digest: DIGEST.to_owned(),
    })
}

fn authorization() -> String {
    format!("Basic {}", STANDARD.encode(format!("Alice:{PASSWORD}")))
}

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

#[rstest]
#[case::active(RevocationStatusArg::Active, "active", Some(DIGEST), Some(10))]
#[case::lifted(RevocationStatusArg::Lifted, "lifted", None, None)]
fn test_revocation_client_lists_with_stable_query_parameters(
    #[case] status: RevocationStatusArg,
    #[case] expected_status: &str,
    #[case] cursor: Option<&str>,
    #[case] limit: Option<usize>,
) {
    let (runtime, server) = runtime_and_server();
    let mut request = Mock::given(method("GET"))
        .and(path("/+revocations"))
        .and(query_param("status", expected_status));
    if let Some(cursor) = cursor {
        request = request.and(query_param("cursor", cursor));
    }
    if let Some(limit) = limit {
        request = request.and(query_param("limit", limit.to_string()));
    }
    runtime.block_on(
        request
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"revocations": []})))
            .expect(1)
            .mount(&server),
    );
    let command = RevocationCommand::List(ListRevocationsArgs {
        client: client(&server),
        status: Some(status),
        cursor: cursor.map(str::to_owned),
        limit,
    });

    revocation(&command, &mut Cursor::new(PASSWORD), &mut Vec::new()).unwrap();
}

#[rstest]
#[case::inspect("GET", format!("/admin/+revocations/{DIGEST}"), false)]
#[case::lift("POST", format!("/admin/+revocations/{DIGEST}/lift"), true)]
fn test_revocation_client_targets_digest_operations(
    #[case] request_method: &str,
    #[case] request_path: String,
    #[case] lift: bool,
) {
    let (runtime, server) = runtime_and_server();
    runtime.block_on(
        Mock::given(method(request_method))
            .and(path(request_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"revision": 1})))
            .expect(1)
            .mount(&server),
    );
    let mut client = client(&server);
    client.server = format!("{}/admin", server.uri());
    let command = if lift {
        RevocationCommand::Lift(LiftRevocationArgs {
            client,
            digest: DIGEST.to_owned(),
        })
    } else {
        inspect(client)
    };

    revocation(&command, &mut Cursor::new(PASSWORD), &mut Vec::new()).unwrap();
}

#[test]
fn test_revocation_client_reads_password_file_without_exposing_it() {
    let (runtime, server) = runtime_and_server();
    runtime.block_on(
        Mock::given(header("authorization", authorization()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("password");
    std::fs::write(&path, format!("{PASSWORD}\n")).unwrap();
    let command = inspect(AdministratorClientArgs {
        password_stdin: false,
        password_file: Some(path),
        ..client(&server)
    });

    revocation(&command, &mut Cursor::new(Vec::new()), &mut Vec::new()).unwrap();
}

#[rstest]
#[case::remote_http("http://packages.example", "HTTP is allowed only for a loopback")]
#[case::unsupported_scheme("ftp://127.0.0.1", "must use HTTPS or loopback HTTP")]
#[case::embedded_credentials("https://Alice:secret@packages.example", "must not contain credentials")]
#[case::missing_host("file:///path", "must contain a host")]
fn test_revocation_client_rejects_unsafe_server_urls(#[case] server: &str, #[case] expected: &str) {
    let command = inspect(AdministratorClientArgs {
        server: server.to_owned(),
        user: "Alice".to_owned(),
        password_stdin: true,
        password_file: None,
    });

    assert!(
        revocation(&command, &mut Cursor::new(PASSWORD), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
}

#[derive(Debug, Clone, Copy)]
enum ResponseFailure {
    Conflict,
    Redirect,
    Malformed,
    Oversized,
}

#[rstest]
#[case::conflict(ResponseFailure::Conflict, "HTTP 409 Conflict")]
#[case::redirect(ResponseFailure::Redirect, "HTTP 302 Found")]
#[case::malformed(ResponseFailure::Malformed, "not valid JSON")]
#[case::oversized(ResponseFailure::Oversized, "exceeds the 1048576-byte limit")]
fn test_revocation_client_rejects_failed_or_unbounded_responses(
    #[case] failure: ResponseFailure,
    #[case] expected: &str,
) {
    let (runtime, server) = runtime_and_server();
    let response = match failure {
        ResponseFailure::Conflict => ResponseTemplate::new(409).set_body_string(PASSWORD),
        ResponseFailure::Redirect => ResponseTemplate::new(302).insert_header("location", "/other"),
        ResponseFailure::Malformed => ResponseTemplate::new(200).set_body_string("not json"),
        ResponseFailure::Oversized => ResponseTemplate::new(200).set_body_bytes(vec![b'a'; 1_048_577]),
    };
    runtime.block_on(Mock::given(method("GET")).respond_with(response).mount(&server));

    let error = revocation(&inspect(client(&server)), &mut Cursor::new(PASSWORD), &mut Vec::new()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(expected), "{message}");
    assert!(!message.contains(PASSWORD));
}

#[rstest]
#[case::digest("bad", None, "invalid artifact digest")]
#[case::cursor(DIGEST, Some("bad"), "invalid revocation cursor")]
fn test_revocation_client_validates_digest_inputs_before_request(
    #[case] digest: &str,
    #[case] cursor: Option<&str>,
    #[case] expected: &str,
) {
    let (_runtime, server) = runtime_and_server();
    let command = cursor.map_or_else(
        || {
            RevocationCommand::Inspect(InspectRevocationArgs {
                client: client(&server),
                digest: digest.to_owned(),
            })
        },
        |cursor| {
            RevocationCommand::List(ListRevocationsArgs {
                client: client(&server),
                status: None,
                cursor: Some(cursor.to_owned()),
                limit: None,
            })
        },
    );

    assert!(
        revocation(&command, &mut Cursor::new(PASSWORD), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
}

#[test]
fn test_revocation_client_propagates_output_failure() {
    let (runtime, server) = runtime_and_server();
    runtime.block_on(
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server),
    );

    assert!(
        revocation(
            &inspect(client(&server)),
            &mut Cursor::new(PASSWORD),
            &mut FailImmediately
        )
        .is_err()
    );
}

#[test]
fn test_revocation_client_reports_missing_password_file() {
    let (_runtime, server) = runtime_and_server();
    let missing = PathBuf::from("missing-administrator-password");
    let command = inspect(AdministratorClientArgs {
        password_stdin: false,
        password_file: Some(missing),
        ..client(&server)
    });

    assert!(
        revocation(&command, &mut Cursor::new(Vec::new()), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("open password file")
    );
}
