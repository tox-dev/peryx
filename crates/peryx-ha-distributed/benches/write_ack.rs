#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

//! Measures the pure write-durability decision for local, datacenter, and cross-datacenter evidence.
//! Transport polling has separate benchmarks.

use std::collections::BTreeSet;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ha_distributed::{
    AckDecision, Deadline, DurabilityPolicy, FilesystemAck, MetadataOperation, ReceiptAck, RemoteAck,
    assess_remote_metadata_durability,
};
use peryx_storage::blob::Digest;

/// Repetitions folded into one measured iteration so a stray allocator housekeeping pass stays in the
/// noise and the per-write instruction count reads steady.
const BATCH: usize = 256;

/// The client is still within its write deadline: the live-deadline path a healthy write takes, so
/// the reading is the decision cost rather than a timed-out fallback.
const DEADLINE: Deadline = Deadline::Live;

/// The ingress node the local placement receipt is attributed to.
const LOCAL: &str = "local";

/// One availability posture's fixed inputs: the quorum a write resolves against and whether it also
/// waits on a remote datacenter's metadata durability.
struct Posture {
    name: &'static str,
    policy: DurabilityPolicy,
    members: BTreeSet<String>,
    /// Peer receipts already gathered toward the byte quorum, beyond the local node's own.
    peers: Vec<String>,
    /// The remote metadata dimension an `ha` write folds in; `None` for `none` and `dc`, whose
    /// metadata commits to the local journal synchronously.
    remote: Option<Remote>,
}

/// An `ha` write's remote metadata evidence: the operation it waits to see remote-durable and the
/// acknowledgements gathered from eligible datacenters.
struct Remote {
    operation: MetadataOperation,
    acks: Vec<RemoteAck>,
}

fn members(nodes: &[&str]) -> BTreeSet<String> {
    nodes.iter().map(|node| (*node).to_owned()).collect()
}

/// A single-node write: its own receipt satisfies a local quorum and its metadata is durable the
/// moment it commits, so availability adds nothing beyond the floor decision.
fn none() -> Posture {
    Posture {
        name: "none",
        policy: DurabilityPolicy::Local,
        members: members(&[LOCAL]),
        peers: Vec::new(),
        remote: None,
    }
}

/// A datacenter write: the byte quorum is a strict majority of the datacenter's members, reached here
/// from the local receipt plus one peer, and the metadata dimension is the local journal commit.
fn dc() -> Posture {
    Posture {
        name: "dc",
        policy: DurabilityPolicy::Majority,
        members: members(&[LOCAL, "peer-b", "peer-c"]),
        peers: vec!["peer-b".to_owned()],
        remote: None,
    }
}

/// A highly-available write: the datacenter byte quorum of `dc`, plus a remote metadata dimension that
/// an eligible remote datacenter has committed the exact operation under the write's authority epoch.
fn ha() -> Posture {
    Posture {
        name: "ha",
        policy: DurabilityPolicy::Majority,
        members: members(&[LOCAL, "peer-b", "peer-c"]),
        peers: vec!["peer-b".to_owned()],
        remote: Some(Remote {
            operation: MetadataOperation {
                epoch: 7,
                frontier: 4_812,
            },
            acks: vec![RemoteAck {
                datacenter: "east".to_owned(),
                epoch: 7,
                applied_frontier: 4_812,
            }],
        }),
    }
}

/// Resolve one write's datacenter acknowledgement exactly as the hosted upload path does: seed the
/// local receipt, fold the gathered peer receipts into the byte quorum, decide the metadata dimension
/// (a remote-durability fold in `ha`, an immediate local commit otherwise), and combine them.
fn resolve(posture: &Posture, digest: &Digest) -> peryx_ha_distributed::DcAck {
    let mut ack = FilesystemAck::new(digest.clone(), posture.members.clone(), posture.policy);
    ack.record(ReceiptAck {
        node: LOCAL.to_owned(),
        digest: digest.clone(),
    });
    for peer in &posture.peers {
        ack.record(ReceiptAck {
            node: peer.clone(),
            digest: digest.clone(),
        });
    }
    let metadata = posture.remote.as_ref().map_or(AckDecision::Acknowledged, |remote| {
        let durability = assess_remote_metadata_durability(&remote.operation, &remote.acks);
        if durability.is_durable() {
            AckDecision::Acknowledged
        } else {
            AckDecision::NotYetDurable {
                target: remote.operation.frontier,
                durable_frontier: 0,
            }
        }
    });
    ack.decide(metadata, DEADLINE)
}

fn bench_availability_ack(criterion: &mut Criterion) {
    let digest = Digest::of(b"artifact");
    let mut group = criterion.benchmark_group("availability_ack");
    for posture in [none(), dc(), ha()] {
        group.bench_function(posture.name, |bencher| {
            bencher.iter(|| {
                for _ in 0..BATCH {
                    black_box(resolve(black_box(&posture), black_box(&digest)));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_availability_ack);
criterion_main!(benches);
