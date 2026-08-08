use peryx_bench_core::machine::{ProfileSettings, write_profile};

#[test]
fn profile_defaults_match_published_workload_scale() {
    let settings = ProfileSettings::default();
    assert_eq!(
        (
            settings.payload_bytes,
            settings.memory_bytes,
            settings.clients,
            settings.chunk_bytes,
            settings.rounds,
        ),
        (30 * 1024 * 1024, 256 * 1024 * 1024, 8, 8 * 1024 * 1024, 5)
    );
}

#[tokio::test]
async fn profile_writes_host_volumes_and_baselines() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("nested/machine.toml");
    write_profile(
        &path,
        directory.path(),
        ProfileSettings {
            payload_bytes: 64 * 1024,
            memory_bytes: 64 * 1024,
            clients: 2,
            chunk_bytes: 4 * 1024,
            rounds: 1,
        },
    )
    .await
    .expect("profile succeeds");
    let profile: toml::Value =
        toml::from_str(&std::fs::read_to_string(path).expect("profile exists")).expect("profile is valid TOML");
    assert_eq!(
        (
            profile["host"].is_table(),
            profile["volumes"].is_array(),
            profile["baselines"].as_array().map(Vec::len),
        ),
        (true, true, Some(4))
    );
}

#[tokio::test]
async fn profile_rejects_invalid_settings() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let defaults = ProfileSettings {
        payload_bytes: 1,
        memory_bytes: 1,
        clients: 1,
        chunk_bytes: 1,
        rounds: 1,
    };
    let cases = [
        (
            ProfileSettings {
                payload_bytes: 0,
                ..defaults
            },
            "payload size",
        ),
        (ProfileSettings { clients: 0, ..defaults }, "client count"),
        (
            ProfileSettings {
                memory_bytes: 1,
                clients: 2,
                ..defaults
            },
            "memory size",
        ),
        (
            ProfileSettings {
                chunk_bytes: 0,
                ..defaults
            },
            "chunk size",
        ),
        (ProfileSettings { rounds: 0, ..defaults }, "round count"),
    ];
    for (settings, message) in cases {
        let error = write_profile(&directory.path().join("machine.toml"), directory.path(), settings)
            .await
            .expect_err("invalid settings fail");
        assert!(error.to_string().contains(message), "{error:#}");
    }
}
