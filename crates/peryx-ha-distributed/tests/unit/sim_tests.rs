use std::num::NonZeroUsize;

use super::{
    Action, Config, Defect, Fault, Invariant, Outcome, Rng, Topology, World, execute, generate_plan, minimize, run,
};
use crate::envelope::{AuthorityEpoch, OperationKind};

fn config(seed: u64, topology: Topology, steps: usize, defect: Option<Defect>) -> Config {
    Config {
        seed,
        topology,
        steps,
        defect,
    }
}

fn topo(nodes: usize, sources: usize) -> Topology {
    Topology::new(nodes, sources).expect("nonzero topology")
}

fn solo() -> Topology {
    topo(1, 1)
}

#[test]
fn topology_rejects_a_zero_dimension() {
    assert!(Topology::new(0, 1).is_none());
    assert!(Topology::new(1, 0).is_none());
    assert!(Topology::new(0, 0).is_none());
    let ok = Topology::new(2, 3).expect("nonzero topology");
    assert_eq!(ok.nodes.get(), 2);
    assert_eq!(ok.sources.get(), 3);
    assert!(serde_json::from_str::<Topology>(r#"{"nodes":0,"sources":1}"#).is_err());
}

#[test]
fn rng_repeats_its_stream_per_seed() {
    let mut first = Rng::seeded(42);
    let mut second = Rng::seeded(42);
    let mut third = Rng::seeded(7);
    let a: Vec<u64> = (0..8).map(|_| first.next_u64()).collect();
    let b: Vec<u64> = (0..8).map(|_| second.next_u64()).collect();
    let c: Vec<u64> = (0..8).map(|_| third.next_u64()).collect();
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn rng_below_stays_within_bound() {
    let five = NonZeroUsize::new(5).expect("nonzero");
    let one = NonZeroUsize::new(1).expect("nonzero");
    let mut rng = Rng::seeded(99);
    assert!((0..64).map(|_| rng.below(five)).all(|value| value < 5));
    assert_eq!(rng.below(one), 0);
}

#[test]
fn generate_plan_is_deterministic() {
    let one = generate_plan(&config(1234, topo(3, 2), 60, None));
    let two = generate_plan(&config(1234, topo(3, 2), 60, None));
    assert_eq!(one, two);
    assert_eq!(one.len(), 60);
}

#[test]
fn generate_plan_reaches_every_action_shape() {
    let plan = generate_plan(&config(1, topo(2, 2), 300, None));
    let has = |matcher: fn(&Action) -> bool| plan.iter().any(matcher);
    assert!(has(|action| matches!(action, Action::Produce { .. })));
    assert!(has(|action| matches!(action, Action::Failover { .. })));
    assert!(has(|action| matches!(action, Action::StaleProduce { .. })));
    assert!(has(|action| matches!(action, Action::Deliver { .. })));
    assert!(has(|action| matches!(action, Action::Redeliver { .. })));
    assert!(has(|action| matches!(action, Action::Acknowledge { .. })));
    assert!(plan.iter().any(|action| matches!(
        action,
        Action::Produce {
            kind: OperationKind::Delete,
            ..
        }
    )));
}

#[test]
fn run_reproduces_the_same_trace() {
    let build = || config(0xDEAD_BEEF, topo(3, 2), 120, None);
    assert_eq!(run(&build()), run(&build()));
}

#[test]
fn execute_reproduces_the_same_trace() {
    let cfg = config(5, topo(2, 2), 40, None);
    let plan = generate_plan(&cfg);
    assert_eq!(execute(&cfg, &plan), execute(&cfg, &plan));
}

#[test]
fn correct_model_holds_every_invariant() {
    for seed in 0..64_u64 {
        let cfg = config(seed, topo(3, 2), 160, None);
        let trace = run(&cfg);
        assert_eq!(
            trace.outcome,
            Outcome::Held,
            "seed {seed} broke an invariant: {trace:?}"
        );
    }
}

fn stale_epoch_plan() -> Vec<Action> {
    vec![
        Action::Failover { source: 0 },
        Action::StaleProduce { source: 0 },
        Action::Deliver { node: 0 },
    ]
}

fn duplicate_plan() -> Vec<Action> {
    vec![
        Action::Produce {
            source: 0,
            kind: OperationKind::Publish,
        },
        Action::Deliver { node: 0 },
        Action::Redeliver { node: 0, source: 0 },
    ]
}

fn acknowledge_plan() -> Vec<Action> {
    vec![
        Action::Produce {
            source: 0,
            kind: OperationKind::Publish,
        },
        Action::Acknowledge { source: 0 },
    ]
}

#[test]
fn defect_turns_into_its_matching_invariant_failure() {
    let cases: [(Defect, Vec<Action>, Invariant); 5] = [
        (Defect::AcceptStaleEpoch, stale_epoch_plan(), Invariant::Authority),
        (Defect::ReapplyDuplicate, duplicate_plan(), Invariant::Idempotency),
        (Defect::RegressFrontier, duplicate_plan(), Invariant::Frontier),
        (Defect::HideCommitted, duplicate_plan(), Invariant::Visibility),
        (Defect::LoseAcknowledged, acknowledge_plan(), Invariant::Rpo),
    ];
    for (defect, plan, invariant) in cases {
        let trace = execute(&config(0, solo(), 0, Some(defect)), &plan);
        assert!(
            matches!(trace.outcome, Outcome::Violated { invariant: broken, .. } if broken == invariant),
            "{defect:?} did not break {invariant:?}: {trace:?}"
        );
    }
}

#[test]
fn defect_free_model_preserves_invariants_for_each_plan() {
    for plan in [stale_epoch_plan(), duplicate_plan(), acknowledge_plan()] {
        let trace = execute(&config(0, solo(), 0, None), &plan);
        assert_eq!(trace.outcome, Outcome::Held, "clean run broke on {plan:?}");
    }
}

#[test]
fn injected_defect_is_recorded_as_a_fault() {
    let trace = execute(
        &config(0, solo(), 0, Some(Defect::LoseAcknowledged)),
        &acknowledge_plan(),
    );
    assert!(trace.faults.contains(&Fault::Injected {
        defect: Defect::LoseAcknowledged,
    }));
}

#[test]
fn failover_and_stale_primary_are_recorded_as_faults() {
    let plan = vec![Action::Failover { source: 0 }, Action::StaleProduce { source: 0 }];
    let trace = execute(&config(0, solo(), 0, None), &plan);
    assert_eq!(
        trace.faults,
        vec![
            Fault::Failover { source: 0, step: 0 },
            Fault::StalePrimary { source: 0, step: 1 },
        ]
    );
}

#[test]
fn trace_round_trips_through_json() {
    let topology = topo(2, 2);
    let mut plan = generate_plan(&config(3, topology, 50, None));
    plan.extend(duplicate_plan());
    let trace = execute(&config(3, topology, 50, Some(Defect::RegressFrontier)), &plan);
    let bytes = serde_json::to_vec(&trace).expect("trace serializes");
    let restored: super::Trace = serde_json::from_slice(&bytes).expect("trace deserializes");
    assert_eq!(trace, restored);

    let held = run(&config(3, topology, 50, None));
    assert_eq!(held.outcome, Outcome::Held);
    let held_bytes = serde_json::to_vec(&held).expect("held trace serializes");
    let held_restored: super::Trace = serde_json::from_slice(&held_bytes).expect("held trace deserializes");
    assert_eq!(held, held_restored);
}

#[test]
fn minimize_shrinks_a_padded_failing_plan() {
    let cfg = config(0, solo(), 0, Some(Defect::ReapplyDuplicate));
    let padded: Vec<Action> = vec![
        Action::Acknowledge { source: 0 },
        Action::Produce {
            source: 0,
            kind: OperationKind::Publish,
        },
        Action::Deliver { node: 0 },
        Action::Acknowledge { source: 0 },
        Action::Redeliver { node: 0, source: 0 },
        Action::Deliver { node: 0 },
    ];
    let reduced = minimize(&cfg, &padded);
    assert!(reduced.len() < padded.len());
    assert!(reduced.iter().any(|action| matches!(action, Action::Redeliver { .. })));
    let trace = execute(&cfg, &reduced);
    assert!(matches!(
        trace.outcome,
        Outcome::Violated {
            invariant: Invariant::Idempotency,
            ..
        }
    ));
}

#[test]
fn minimize_leaves_a_holding_plan_unchanged() {
    let cfg = config(0, solo(), 0, None);
    let plan = duplicate_plan();
    assert_eq!(minimize(&cfg, &plan), plan);
}

#[test]
fn fenced_stale_message_never_commits() {
    let plan = [Action::StaleProduce { source: 0 }, Action::Deliver { node: 0 }];
    let mut world = World::new(&config(0, solo(), 0, None));
    for (step, action) in plan.iter().enumerate() {
        world.step(step, action);
    }
    assert_eq!(world.nodes[0].frontier[0], 0);
    assert!(world.nodes[0].log.is_empty());
    assert_eq!(world.check(), None);
}

#[test]
fn deliver_advances_frontier_and_accepted_epoch() {
    let plan = [
        Action::Produce {
            source: 0,
            kind: OperationKind::Publish,
        },
        Action::Deliver { node: 0 },
        Action::Produce {
            source: 0,
            kind: OperationKind::Withdraw,
        },
        Action::Deliver { node: 0 },
        Action::Failover { source: 0 },
        Action::Produce {
            source: 0,
            kind: OperationKind::Delete,
        },
        Action::Deliver { node: 0 },
    ];
    let mut world = World::new(&config(0, solo(), 0, None));
    for (step, action) in plan.iter().enumerate() {
        world.step(step, action);
        assert_eq!(world.check(), None);
    }
    assert_eq!(world.nodes[0].frontier[0], 3);
    assert_eq!(world.nodes[0].accepted[0], AuthorityEpoch(2));
    let committed = world.nodes[0].log.get(&(0, 3)).expect("serial 3 committed");
    assert_eq!(committed.epoch, AuthorityEpoch(2));
    assert_eq!(committed.kind, OperationKind::Delete);
}

#[test]
fn delivering_an_empty_queue_changes_nothing() {
    let mut world = World::new(&config(0, solo(), 0, None));
    world.step(0, &Action::Deliver { node: 0 });
    assert_eq!(world.nodes[0].frontier[0], 0);
    assert_eq!(world.check(), None);
}

#[test]
fn redeliver_without_an_applied_operation_is_a_no_op() {
    let mut world = World::new(&config(0, solo(), 0, Some(Defect::RegressFrontier)));
    world.step(0, &Action::Redeliver { node: 0, source: 0 });
    assert_eq!(world.nodes[0].frontier[0], 0);
    assert_eq!(world.check(), None);
}

#[test]
fn redeliver_without_a_defect_stays_idempotent() {
    let mut world = World::new(&config(0, solo(), 0, None));
    for (step, action) in duplicate_plan().iter().enumerate() {
        world.step(step, action);
    }
    assert_eq!(world.nodes[0].applied.get(&(0, 1)), Some(&1));
    assert_eq!(world.check(), None);
}

#[test]
fn acknowledge_marks_every_durable_operation() {
    let plan = [
        Action::Produce {
            source: 0,
            kind: OperationKind::Publish,
        },
        Action::Produce {
            source: 0,
            kind: OperationKind::Withdraw,
        },
        Action::Acknowledge { source: 0 },
    ];
    let mut world = World::new(&config(0, solo(), 0, None));
    for (step, action) in plan.iter().enumerate() {
        world.step(step, action);
    }
    assert!(world.acked.contains(&(0, 1)));
    assert!(world.acked.contains(&(0, 2)));
    assert_eq!(world.check(), None);
}

#[test]
fn invariants_re_hold_after_a_fenced_fault() {
    let plan = [
        Action::Failover { source: 0 },
        Action::StaleProduce { source: 0 },
        Action::Produce {
            source: 0,
            kind: OperationKind::Delete,
        },
        Action::Deliver { node: 0 },
        Action::Deliver { node: 0 },
    ];
    let mut world = World::new(&config(0, solo(), 0, None));
    for (step, action) in plan.iter().enumerate() {
        world.step(step, action);
        assert_eq!(world.check(), None, "step {step} broke an invariant");
    }
    assert_eq!(world.nodes[0].frontier[0], 1);
    let committed = world.nodes[0].log.get(&(0, 1)).expect("serial 1 committed");
    assert_eq!(committed.epoch, AuthorityEpoch(2));
    assert_eq!(committed.kind, OperationKind::Delete);
}
