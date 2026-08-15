use std::path::Path;

use sysinfo::{Disks, System};

use super::{
    FileMeasure, ProfileSettings, baselines, baselines_with, capacity, cpu, describe_cores, longest_prefix, mount_for,
    or_unknown, rate, read_one, repo_root, volumes, write_one, write_profile,
};
#[cfg(target_os = "macos")]
use super::{sysctl, sysctl_with};

const fn smoke_settings() -> ProfileSettings {
    ProfileSettings {
        payload_bytes: 64 * 1024,
        memory_bytes: 64 * 1024,
        clients: 2,
        chunk_bytes: 4 * 1024,
        rounds: 1,
    }
}

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

#[test]
fn capacity_uses_decimal_disk_units() {
    assert_eq!(
        (capacity(500_000_000_000), capacity(2_000_000_000_000)),
        ("500.0 GB".to_owned(), "2.0 TB".to_owned())
    );
}

#[test]
fn rate_uses_decimal_throughput_units() {
    assert_eq!(
        (rate(999_000_000.0), rate(1_250_000_000.0)),
        ("999 MB/s".to_owned(), "1.2 GB/s".to_owned())
    );
}

#[tokio::test]
async fn profile_writes_host_volumes_and_baselines() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("nested/machine.toml");
    write_profile(&path, directory.path(), smoke_settings())
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

#[test]
fn host_fallbacks_are_explicit() {
    assert_eq!(or_unknown(None), "unknown");
    assert_eq!(or_unknown(Some("value".to_owned())), "value");
    assert_eq!(cpu(&System::new()), "unknown");
    assert!(!cpu(&System::new_all()).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn core_description_handles_split_and_uniform_cpus() {
    assert_eq!(describe_cores(8, None, None), "8");
    assert_eq!(
        describe_cores(8, Some("4".to_owned()), Some("4".to_owned())),
        "8 (4 performance + 4 efficiency)"
    );
    assert_eq!(sysctl("peryx.invalid.sysctl"), None);
    assert_eq!(sysctl_with("unused", &|_| Err(std::io::Error::other("failed"))), None);
    assert_eq!(
        sysctl_with("unused", &|_| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: vec![0xff],
                stderr: Vec::new(),
            })
        }),
        None
    );
}

#[tokio::test]
async fn baselines_propagate_each_file_measurement_failure() {
    let succeeds = |_: &Path, _: usize, _: usize, _: usize| Ok(1.0);
    let fail_single = |_: &Path, _: usize, _: usize, _: usize| anyhow::bail!("single measurement failed");
    let fail_parallel = |_: &Path, clients: usize, _: usize, _: usize| {
        if clients > 1 {
            anyhow::bail!("parallel measurement failed")
        }
        Ok(1.0)
    };
    let cases: [(&FileMeasure, &FileMeasure, &str); 3] = [
        (&fail_parallel, &succeeds, "parallel measurement failed"),
        (&succeeds, &fail_single, "single measurement failed"),
        (&succeeds, &fail_parallel, "parallel measurement failed"),
    ];
    let directory = tempfile::tempdir().expect("temporary directory");
    for (disk_write, page_cache_read, expected) in cases {
        let error = baselines_with(directory.path(), &[], smoke_settings(), disk_write, page_cache_read)
            .await
            .err()
            .expect("measurement fails");
        assert_eq!(error.to_string(), expected);
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn core_and_model_descriptions_handle_missing_metadata() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let model = directory.path().join("model");
    std::fs::write(&model, " workstation \n").unwrap();
    assert_eq!(super::model_at(&model), "workstation");
    assert_eq!(super::model_at(&directory.path().join("missing")), "unknown");
    assert_eq!(describe_cores(8, None), "8");
    assert_eq!(describe_cores(8, Some(4)), "8 logical / 4 physical");
}

#[test]
fn mount_selection_falls_back_for_missing_descendants() {
    let disks = Disks::new_with_refreshed_list();
    let missing = repo_root().join("peryx-missing-mount-probe");
    let disk = mount_for(&disks, &missing).expect("repository has a containing mount");
    assert!(missing.starts_with(disk.mount_point()));
    assert_eq!(
        longest_prefix(&disks, &missing).map(sysinfo::Disk::mount_point),
        Some(disk.mount_point())
    );
}

#[test]
fn volume_roles_merge_on_one_mount() {
    let volumes = volumes(&repo_root());
    assert!(
        volumes
            .iter()
            .any(|volume| volume.benchmarked && volume.role.contains(';'))
    );
}

#[tokio::test]
async fn baselines_name_an_unreported_scratch_volume() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let rows = baselines(directory.path(), &[], smoke_settings()).await.unwrap();
    assert!(rows[1].measures.contains("scratch volume"));
}

#[tokio::test]
async fn profile_reports_a_path_without_a_filename() {
    let directory = tempfile::tempdir().expect("temporary directory");
    assert!(
        write_profile(Path::new(""), directory.path(), smoke_settings())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn profile_reports_measurement_and_write_failures() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing_scratch = directory.path().join("missing");
    assert!(
        write_profile(
            &directory.path().join("machine.toml"),
            &missing_scratch,
            smoke_settings(),
        )
        .await
        .is_err()
    );
    assert!(
        write_profile(directory.path(), directory.path(), smoke_settings())
            .await
            .is_err()
    );
}

#[test]
fn file_helpers_preserve_path_context() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("missing/file");
    assert!(
        write_one(&missing, &[1], 1)
            .unwrap_err()
            .to_string()
            .contains("cannot create")
    );
    assert!(read_one(&missing, 1).unwrap_err().to_string().contains("cannot open"));
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
