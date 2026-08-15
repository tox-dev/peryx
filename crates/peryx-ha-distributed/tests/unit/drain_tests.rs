use crate::drain::{DrainIntent, DrainPlan, plan_drain};
use crate::reconcile::{Disposition, OldEpochOp};

fn intent(key: &str, op: OldEpochOp) -> DrainIntent {
    DrainIntent {
        key: key.to_owned(),
        op,
    }
}

const REPLAYABLE: OldEpochOp = OldEpochOp {
    durably_committed: true,
    already_applied: false,
    superseded: false,
};

#[test]
fn test_plan_finalizes_replayable_intents_in_key_order() {
    let plan = plan_drain(vec![
        intent("z", REPLAYABLE),
        intent("a", REPLAYABLE),
        intent("m", REPLAYABLE),
    ]);

    assert_eq!(
        plan,
        DrainPlan {
            finalize: vec!["a".to_owned(), "m".to_owned(), "z".to_owned()],
            retired: Vec::new(),
        }
    );
}

#[test]
fn test_plan_retires_each_non_replayable_disposition() {
    let plan = plan_drain(vec![
        intent(
            "already",
            OldEpochOp {
                durably_committed: true,
                already_applied: true,
                superseded: false,
            },
        ),
        intent(
            "superseded",
            OldEpochOp {
                durably_committed: true,
                already_applied: false,
                superseded: true,
            },
        ),
        intent(
            "failed",
            OldEpochOp {
                durably_committed: false,
                already_applied: false,
                superseded: false,
            },
        ),
        intent("replay", REPLAYABLE),
    ]);

    assert_eq!(plan.finalize, vec!["replay".to_owned()]);
    assert_eq!(
        plan.retired,
        vec![
            ("already".to_owned(), Disposition::AlreadyApplied),
            ("failed".to_owned(), Disposition::Failed),
            ("superseded".to_owned(), Disposition::Superseded),
        ]
    );
}

#[test]
fn test_plan_reaches_one_outcome_per_intent() {
    let plan = plan_drain(vec![
        intent("a", REPLAYABLE),
        intent(
            "b",
            OldEpochOp {
                durably_committed: true,
                already_applied: true,
                superseded: true,
            },
        ),
    ]);

    assert_eq!(plan.finalize.len() + plan.retired.len(), 2);
}

#[test]
fn test_planning_an_empty_drain_is_empty() {
    assert_eq!(plan_drain(Vec::new()), DrainPlan::default());
}
