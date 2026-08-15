use crate::config::Config;
use crate::server::build_state;
use peryx_core::TopologyMode;

#[test]
fn disabled_availability_has_no_runtime_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let state = build_state(&config).unwrap();

    assert!(super::runtime_config(&config).unwrap().is_none());
    assert_eq!(state.serving.availability_topology().mode, TopologyMode::None);
}

#[test]
fn disabled_availability_rejects_a_required_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let state = build_state(&config).unwrap();

    assert!(super::ReplicationRuntime::new(&config, &state).is_err());
}
