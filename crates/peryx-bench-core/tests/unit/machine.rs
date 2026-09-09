use std::path::Path;

use sysinfo::{Disks, System};

use super::{
    FileMeasure, ProfileSettings, baselines, baselines_with, capacity, cpu, describe_cores, disk_write, drain,
    exact_mount, gibibytes, http_client, longest_prefix, memory_copy, mount_for, or_unknown, page_cache_read, rate,
    read_one, repeat, repo_root, reported_mount, serve_loopback, shares, spans, summarize, throughput, volumes,
    write_one, write_profile,
};
#[cfg(target_os = "macos")]
use super::{model, sysctl, sysctl_with};

/// Below this a returned figure cannot be a measurement: even a saturated CI disk moves far more
/// than a kilobyte a second, so anything slower is a constant standing in for one.
const SLOWEST_CREDIBLE_RATE: f64 = 1000.0;

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

#[test]
fn shares_round_the_budget_down_to_whole_streams() {
    assert_eq!(shares(10, 3), (3, 9));
}

#[test]
fn spans_cover_the_budget_with_a_short_last_write() {
    assert_eq!(spans(10, 4).collect::<Vec<_>>(), vec![4, 4, 2]);
}

#[test]
fn file_helpers_move_exactly_the_requested_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("payload");
    write_one(&path, &[7u8; 4], 10).expect("the write succeeds");
    assert_eq!(std::fs::metadata(&path).expect("the file exists").len(), 10);
    assert_eq!(read_one(&path, 4).expect("the read succeeds"), 10);
}

#[test]
fn summarize_reports_the_median_and_its_dispersion() {
    assert_eq!(summarize(vec![3e9, 1e9, 2e9]), "2.0 GB/s ±41%");
}

#[test]
fn repeat_discards_the_first_sample() {
    let mut samples = [1e9, 2e9, 3e9, 4e9].into_iter();
    let mut measure = || samples.next().expect("a sample per round");
    assert_eq!(repeat(3, &mut measure), "3.0 GB/s ±27%");
}

#[test]
fn throughput_is_bytes_over_seconds() {
    assert_eq!((throughput(1000, 0.5), throughput(3, 4.0)), (2000.0, 0.75));
}

#[test]
fn gibibytes_reports_memory_in_binary_units() {
    assert_eq!(gibibytes(8 * 1024 * 1024 * 1024), "8 GB");
}

#[test]
fn reported_mount_reads_the_mount_point_df_names() {
    assert_eq!(reported_mount(Path::new("/")).as_deref(), Some("/"));
}

#[test]
fn exact_mount_matches_a_whole_mount_point() {
    let disks = Disks::new_with_refreshed_list();
    let first = disks.list().first().expect("the host reports at least one disk");
    assert_eq!(
        exact_mount(&disks, first.mount_point()).map(sysinfo::Disk::mount_point),
        Some(first.mount_point())
    );
    assert!(exact_mount(&disks, Path::new("/peryx-not-a-mount-point")).is_none());
}

#[test]
fn memory_copy_reports_a_measured_rate() {
    assert!(memory_copy(2, 1 << 20) > SLOWEST_CREDIBLE_RATE);
}

#[test]
fn file_baselines_report_measured_rates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let written = disk_write(directory.path(), 2, 64 * 1024, 4 * 1024).expect("the write succeeds");
    let read = page_cache_read(directory.path(), 2, 64 * 1024, 4 * 1024).expect("the read succeeds");
    assert!(written > SLOWEST_CREDIBLE_RATE);
    assert!(read > SLOWEST_CREDIBLE_RATE);
}

#[tokio::test]
async fn drain_reports_the_bytes_the_loopback_server_sent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    listener.set_nonblocking(true).expect("a non-blocking listener");
    let address = listener.local_addr().expect("the bound address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(serve_loopback(listener, stopped, 2048));
    let http = http_client().expect("an HTTP client");
    let url = format!("http://{address}/payload");

    assert_eq!(drain(&http, &url, 2048).await.expect("the whole body arrives"), 2048);
    let short = drain(&http, &url, 4096).await.expect_err("a short body is refused");
    assert_eq!(short.to_string(), "loopback served 2048 bytes, expected 4096");

    stop.send(()).expect("the server is still listening");
    serving
        .await
        .expect("the server task joins")
        .expect("the server stops cleanly");
}

#[cfg(target_os = "macos")]
#[test]
fn sysctl_trims_the_value_it_reads() {
    assert_eq!(
        sysctl_with("unused", &|_| Ok(std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b" value \n".to_vec(),
            stderr: Vec::new(),
        })),
        Some("value".to_owned())
    );
}

#[cfg(target_os = "macos")]
#[test]
fn model_reads_the_board_sysctl_reports() {
    let board = std::process::Command::new("sysctl")
        .args(["-n", "hw.model"])
        .output()
        .expect("sysctl runs");
    let reported = String::from_utf8(board.stdout).expect("sysctl prints text");
    assert_eq!(model(), reported.trim());
}
