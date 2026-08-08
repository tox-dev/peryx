use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{CompiledEcosystemSettings, EcosystemPlugin as _};
use peryx_driver::state::AppState;
use utoipa::openapi::PathsBuilder;

use crate::{ECOSYSTEM, OciPlugin};

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

#[test]
fn plugin_exposes_its_contract() {
    let plugin = OciPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert!(!plugin.default_indexes().is_empty());
    assert_eq!(plugin.driver().ecosystem(), ECOSYSTEM);
    assert!(
        plugin
            .compile_index_settings("oci", &toml::Table::new())
            .unwrap()
            .is_some()
    );
    assert!(!plugin.openapi_paths(PathsBuilder::new()).build().paths.is_empty());
    assert_eq!(
        plugin
            .snippet_text(
                &BaseUrl::parse("https://registry.example/").unwrap(),
                "oci",
                false,
                "docker"
            )
            .unwrap_err(),
        "OCI does not provide client snippet \"docker\""
    );
}

#[test]
fn plugin_installs_local_and_distributed_drivers() {
    let plugin = OciPlugin;
    let (_dir, mut state) = state();

    plugin.install(&mut state, &[]).unwrap();
    plugin.install_distributed(&mut state, &[]).unwrap();
}

#[test]
fn plugin_rejects_settings_compiled_for_another_type() {
    let plugin = OciPlugin;
    let (_dir, mut state) = state();
    let settings = CompiledEcosystemSettings::new(ECOSYSTEM, ());

    assert_eq!(
        plugin.install(&mut state, &[("oci", &settings)]).unwrap_err(),
        "compiled settings for oci have the wrong type"
    );
}
