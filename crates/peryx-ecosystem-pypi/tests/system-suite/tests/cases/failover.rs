use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;

use crate::harness::{Cluster, MemberSpec, Node, ProcessHarness, Role, Topology};
use crate::pypi_support::{
    UPLOAD_TOKEN, WHEEL, WHEEL_FILENAME, config as fixture_config, download_wheel, publish, publish_payload,
};

const READ_ONLY_BODY: &str = r#"{"error":"read_only_replica","message":"this replica does not accept mutations"}"#;
const REJECTED_UPLOAD_SIZE: usize = 2 * 1024 * 1024;

fn pypi_config() -> String {
    fixture_config().replace("projects =", "resources =")
}

fn home_dc_group() -> Cluster {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
            MemberSpec::new("replica-south", "south", Role::Replica),
        ],
    )
    .with_admin()
    .with_index_config(&pypi_config())
    .with_write_ack_deadline(1)
    .with_process_harness(ProcessHarness::new(peryx_test_support::peryx_binary()))
    .start()
    .expect("the ha group starts")
}

fn mutate(node: &Node, method: reqwest::Method, path: &str) -> (u16, Option<String>, String) {
    let response = node
        .request(method, path)
        .basic_auth("__token__", Some(UPLOAD_TOKEN))
        .send()
        .expect("mutation reaches the node");
    let code = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (code, retry_after, response.text().unwrap_or_default())
}

fn writer_serial(node: &Node) -> Option<u64> {
    reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{}/hosted/simple/veloxdemo/", node.port()))
        .header(reqwest::header::ACCEPT, "application/vnd.pypi.simple.v1+json")
        .send()
        .ok()?
        .headers()
        .get("x-pypi-last-serial")?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

#[test]
fn test_upload_is_retry_safe_and_survives_a_writer_restart() {
    let mut cluster = Topology::single()
        .with_index_config(&pypi_config())
        .with_process_harness(ProcessHarness::new(peryx_test_support::peryx_binary()))
        .start()
        .expect("the writer starts");
    let writer = cluster.node("node-a").expect("the writer is present");
    assert_eq!(download_wheel(writer).map(|response| response.0), Some(404));

    let (code, body) = publish(writer).expect("publish reaches the home writer");
    assert_eq!((code, body.as_str()), (200, "upload accepted"));

    let (code, bytes) = download_wheel(writer).expect("download reaches the home writer");
    assert_eq!(
        (code, bytes.as_slice()),
        (200, WHEEL),
        "the home serves the published bytes"
    );

    let serial = writer_serial(writer).expect("the writer reports a committed serial");
    assert_eq!(wheel_filenames(writer), [WHEEL_FILENAME]);

    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "node-a")
        .unwrap();
    home.restart().expect("the writer restarts on its store");
    let writer = cluster.node("node-a").expect("the writer is back");

    let (code, bytes) = download_wheel(writer).expect("download reaches the recovered home");
    assert_eq!(
        (code, bytes.as_slice()),
        (200, WHEEL),
        "the recovered home serves the identical bytes",
    );
    assert_eq!(
        writer_serial(writer),
        Some(serial),
        "the committed serial is unchanged across the failure",
    );
    assert_eq!(wheel_filenames(writer), [WHEEL_FILENAME]);

    let (code, body) = publish(writer).expect("the retry reaches the recovered home");
    assert_eq!((code, body.as_str()), (200, "upload accepted"));
    assert_eq!(
        writer_serial(writer),
        Some(serial),
        "the idempotent retry advances no serial",
    );
    assert_eq!(wheel_filenames(writer), [WHEEL_FILENAME]);
}

#[test]
fn test_ha_replica_rejects_pypi_mutations_before_and_after_a_home_failure() {
    let mut cluster = home_dc_group();
    let old_leader = cluster
        .await_leader(Duration::from_secs(90))
        .expect("the ha group agrees on a leader");
    assert_eq!(old_leader, "east");
    let replica = cluster.node("replica-west").expect("the replica is present");

    assert_replica_refuses_mutations(replica);

    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    assert!(
        ["south", "west",].contains(
            &cluster
                .await_leader_change(&old_leader, Duration::from_secs(90))
                .expect("authority leaves the home datacenter")
                .as_str(),
        )
    );

    let replica = cluster.node("replica-west").expect("the replica survives");
    assert!(
        replica.is_ready(),
        "the replica keeps serving reads after the home fails",
    );
    assert_replica_refuses_mutations(replica);
}

fn assert_replica_refuses_mutations(replica: &Node) {
    let (code, body) = publish_payload(replica, &vec![0; REJECTED_UPLOAD_SIZE]).expect("upload reaches the replica");
    assert_eq!(
        (code, body.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE.as_u16(), READ_ONLY_BODY),
        "the replica refuses an upload",
    );
    for (method, path) in [
        (reqwest::Method::PUT, "/hosted/veloxdemo/1.0.0/yank"),
        (reqwest::Method::DELETE, "/hosted/veloxdemo/1.0.0/"),
    ] {
        let (code, retry_after, body) = mutate(replica, method.clone(), path);
        assert_eq!(
            (code, retry_after.as_deref(), body.as_str()),
            (StatusCode::SERVICE_UNAVAILABLE.as_u16(), Some("1"), READ_ONLY_BODY),
            "the replica refuses a {method} {path}",
        );
    }
}

fn wheel_filenames(node: &Node) -> Vec<String> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build client")
        .get(format!("http://127.0.0.1:{}/hosted/simple/veloxdemo/", node.port()))
        .header(reqwest::header::ACCEPT, "application/vnd.pypi.simple.v1+json")
        .send()
        .expect("detail reaches the node");
    assert_eq!(response.status(), StatusCode::OK);
    let detail = response.json::<Value>().expect("detail is JSON");
    detail["files"]
        .as_array()
        .expect("detail contains files")
        .iter()
        .map(|file| file["filename"].as_str().expect("file has a filename").to_owned())
        .collect()
}
