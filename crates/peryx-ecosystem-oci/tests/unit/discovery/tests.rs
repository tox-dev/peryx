use peryx_driver::discovery::BaseUrl;
use peryx_driver::state::IndexDescription;

use super::index_entry;

fn description(uploads: bool) -> IndexDescription {
    IndexDescription {
        name: "images".to_owned(),
        route: "root/oci".to_owned(),
        ecosystem: "oci".to_owned(),
        kind: "virtual",
        layers: vec!["root/oci-store".to_owned()],
        precedence: vec![peryx_driver::state::MemberDescription {
            name: "root/oci-store".to_owned(),
            role: "hosted",
        }],
        uploads,
        volatile_deletes: false,
        upload_to: uploads.then(|| "root/oci-store".to_owned()),
        upstream: None,
        hosted: None,
    }
}

#[test]
fn test_entry_renders_registry_endpoint_and_pull_snippet() {
    let base = BaseUrl::parse("https://registry.example:5000/").unwrap();
    let entry = index_entry(description(false), Some(&base));
    assert_eq!(entry["ecosystem"], "oci");
    assert_eq!(entry["urls"]["registry"], "https://registry.example:5000/v2/");
    assert_eq!(entry["capabilities"]["manifest_push"], false);
    let docker = entry["client_configuration"]["docker"].as_str().unwrap();
    assert!(docker.contains("docker pull registry.example:5000/root/oci/<image>:<tag>"));
    assert!(!docker.contains("docker push"));
}

#[test]
fn test_writable_entry_includes_push_and_login() {
    let base = BaseUrl::parse("https://registry.example/").unwrap();
    let entry = index_entry(description(true), Some(&base));
    assert_eq!(entry["capabilities"]["manifest_push"], true);
    let docker = entry["client_configuration"]["docker"].as_str().unwrap();
    assert!(docker.contains("docker login registry.example"));
    assert!(docker.contains("docker push registry.example/root/oci/<image>:<tag>"));
}

#[test]
fn test_entry_without_base_uses_host_placeholder() {
    let entry = index_entry(description(false), None);
    assert_eq!(entry["urls"]["registry"], "/v2/");
    let docker = entry["client_configuration"]["docker"].as_str().unwrap();
    assert!(docker.contains("docker pull <host>/root/oci/<image>:<tag>"));
}
