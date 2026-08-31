//! Write-durability decision costs without transport polling.

#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

use std::collections::BTreeSet;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ha_distributed::{
    AckDecision, Deadline, DurabilityPolicy, FilesystemAck, MetadataOperation, ReceiptAck, RemoteAck, RemoteDurability,
    assess_remote_metadata_durability,
};
use peryx_storage::blob::Digest;

const BATCH: usize = 256;

const DEADLINE: Deadline = Deadline::Live;

const LOCAL: &str = "local";

struct Posture {
    name: &'static str,
    policy: DurabilityPolicy,
    members: BTreeSet<String>,
    peers: Vec<String>,
    remote: Option<Remote>,
}

struct Remote {
    operation: MetadataOperation,
    configured: usize,
    acks: Vec<RemoteAck>,
}

fn members(nodes: &[&str]) -> BTreeSet<String> {
    nodes.iter().map(|node| (*node).to_owned()).collect()
}

fn none() -> Posture {
    Posture {
        name: "none",
        policy: DurabilityPolicy::Local,
        members: members(&[LOCAL]),
        peers: Vec::new(),
        remote: None,
    }
}

fn dc() -> Posture {
    Posture {
        name: "dc",
        policy: DurabilityPolicy::Majority,
        members: members(&[LOCAL, "peer-b", "peer-c"]),
        peers: vec!["peer-b".to_owned()],
        remote: None,
    }
}

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
            configured: 1,
            acks: vec![RemoteAck {
                datacenter: "east".to_owned(),
                epoch: 7,
                applied_frontier: 4_812,
            }],
        }),
    }
}

fn ha_pending() -> Posture {
    let mut posture = ha();
    posture.name = "ha-pending";
    posture.remote.as_mut().unwrap().acks.clear();
    posture
}

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
        metadata_decision(remote, posture.policy)
    });
    ack.decide(metadata, DEADLINE)
}

fn metadata_decision(remote: &Remote, policy: DurabilityPolicy) -> AckDecision {
    match assess_remote_metadata_durability(&remote.operation, &remote.acks, remote.configured, policy) {
        RemoteDurability::Durable { .. } => AckDecision::Acknowledged,
        RemoteDurability::Pending { durable_frontier, .. } => AckDecision::NotYetDurable {
            target: remote.operation.frontier,
            durable_frontier,
        },
    }
}

fn bench_availability_ack(criterion: &mut Criterion) {
    let digest = Digest::of(b"artifact");
    let mut group = criterion.benchmark_group("availability_ack");
    for posture in [none(), dc(), ha(), ha_pending()] {
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
