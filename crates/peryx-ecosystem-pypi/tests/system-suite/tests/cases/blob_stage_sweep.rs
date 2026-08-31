use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::harness::{Cluster, ProcessHarness, Topology};

/// Mirrors the store's own bound; a stage younger than this may belong to a write still streaming.
const STAGE_MAX_AGE: Duration = Duration::from_hours(24);

fn stage(directory: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(directory).expect("the stage directory is created");
    let path = directory.join(name);
    std::fs::write(&path, b"interrupted").expect("the stage is written");
    path
}

/// Backdating past the bound is what makes a planted stage look like one a killed process abandoned.
fn abandoned(path: PathBuf) -> PathBuf {
    let aged = SystemTime::now() - STAGE_MAX_AGE - Duration::from_secs(1);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("the stage reopens")
        .set_times(std::fs::FileTimes::new().set_modified(aged))
        .expect("the stage is backdated");
    path
}

fn restart(cluster: &mut Cluster) {
    cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "node-a")
        .expect("the writer is present")
        .restart()
        .expect("the writer restarts on its store");
}

#[test]
fn test_a_restart_sweeps_the_blob_stages_a_killed_process_left_behind() {
    let mut cluster = Topology::single()
        .with_process_harness(ProcessHarness::new(peryx_test_support::peryx_binary()))
        .start()
        .expect("the writer starts");
    let blobs = cluster
        .node("node-a")
        .expect("the writer is present")
        .data_dir()
        .join("blobs");
    let root = abandoned(stage(&blobs, ".peryx-stage-root"));
    let fan_out = abandoned(stage(&blobs.join("sha256/aa/bb"), ".peryx-stage-fan-out"));
    let streaming = stage(&blobs, ".peryx-stage-streaming");

    restart(&mut cluster);

    assert!(!root.exists(), "the root stage survived the restart");
    assert!(!fan_out.exists(), "the fan-out stage survived the restart");
    assert!(streaming.exists(), "the sweep reached a stage inside the age bound");
}
