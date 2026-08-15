use peryx_storage::meta::{JobKind, JobState};

use super::*;
use crate::app;
use crate::cli::{JobCommand, JobListArgs};

fn list_command() -> JobCommand {
    JobCommand::List(JobListArgs {
        runtime: RuntimeArgs::default(),
    })
}

fn run_command(repository: &str) -> JobCommand {
    JobCommand::Run {
        runtime: RuntimeArgs::default(),
        target: repository.to_owned(),
        source: None,
        item_limit: Some(1),
        concurrency: Some(1),
        timeout_secs: Some(30),
    }
}

fn catalog_server() -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let read = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let body = if request.starts_with("GET /simple/ ") {
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#
            } else {
                assert!(request.starts_with("GET /simple/flask/ "), "{request}");
                r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[]}"#
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{address}/simple/"), handle)
}

#[test]
fn registered_catalog_job_syncs_and_records_history() {
    let (upstream, server) = catalog_server();
    let (_dir, meta, mut config) = store_and_config();
    config.indexes[0].kind = crate::config::IndexKind::Cached {
        routing: crate::config::UpstreamRoutingConfig {
            upstreams: vec![crate::config::UpstreamConfig {
                name: "primary".to_owned(),
                url: upstream,
                artifact_url: None,
                username: None,
                password: None,
                token: None,
                credential_exec: None,
                credential_refresh: None,
                tls: crate::config::UpstreamTlsConfig::default(),
            }],
            fallback: true,
            protected: Vec::new(),
            pins: BTreeMap::new(),
        },
        upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
        offline: false,
        prefetch: Box::default(),
    };
    drop(meta);
    let mut output = Vec::new();

    app::job(&config, &run_command("pypi"), &mut output).unwrap();

    assert_eq!(String::from_utf8(output).unwrap(), "processed\t1\nchanged\t2\n");
    server.join().unwrap();
    let runs = MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(runs[0].kind, JobKind::new("catalog_sync").unwrap());
    assert_eq!(runs[0].state, JobState::Succeeded);
    let mut history = Vec::new();
    app::job(&config, &list_command(), &mut history).unwrap();
    let history: serde_json::Value = serde_json::from_slice(&history).unwrap();
    assert_eq!(history["attempts"][0]["kind"], "catalog_sync");
    assert_eq!(history["attempts"][0]["scope"], "pypi");
    assert_eq!(history["attempts"][0]["state"], "succeeded");
}
