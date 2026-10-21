use rstest::rstest;

use super::*;

const HISTORY_LIMIT: usize = 7;
const REPOSITORIES: [&str; 3] = ["alpha", "beta", "gamma"];
const RESOURCES: [&str; 4] = ["one", "two", "three", "four"];

/// A fixed-seed generator, so a failing case replays.
struct Random(u64);

impl Random {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap() % bound
    }
}

struct Spec {
    repository: &'static str,
    resource: &'static str,
    group: Option<&'static str>,
    artifact: Option<&'static str>,
    source: Option<&'static str>,
    action: PolicyAction,
    state: PolicyDecisionState,
    rule: Option<&'static str>,
    reason: Option<&'static str>,
    evaluated_at_unix: i64,
    next_eligible_at_unix: Option<i64>,
}

impl Spec {
    fn random(random: &mut Random, evaluated_at_unix: i64) -> Self {
        Self {
            repository: REPOSITORIES[random.below(REPOSITORIES.len())],
            resource: RESOURCES[random.below(RESOURCES.len())],
            group: [None, Some("1.0"), Some("2.0")][random.below(3)],
            artifact: [None, Some("a-1.0.bin"), Some("b-2.0.bin")][random.below(3)],
            source: [None, Some("src")][random.below(2)],
            action: [PolicyAction::Upload, PolicyAction::Cached, PolicyAction::Serve][random.below(3)],
            state: [
                PolicyDecisionState::Allow,
                PolicyDecisionState::Deny,
                PolicyDecisionState::Wait,
            ][random.below(3)],
            rule: [None, Some("rule \"quoted\"")][random.below(2)],
            reason: [None, Some("reason with \u{e9} and \n newline")][random.below(2)],
            evaluated_at_unix,
            next_eligible_at_unix: [None, Some(evaluated_at_unix + 60)][random.below(2)],
        }
    }

    fn as_new(&self) -> NewPolicyDecision<'static> {
        NewPolicyDecision {
            repository: self.repository,
            resource: self.resource,
            group: self.group,
            artifact: self.artifact,
            source: self.source,
            action: self.action,
            state: self.state,
            rule: self.rule,
            reason: self.reason,
            evaluated_at_unix: self.evaluated_at_unix,
            next_eligible_at_unix: self.next_eligible_at_unix,
        }
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

/// A stored record with its random id replaced, so two stores compare byte for byte.
fn without_id(bytes: &[u8]) -> String {
    let id = decode_policy_decision(bytes).unwrap().id.to_string();
    String::from_utf8(bytes.to_vec()).unwrap().replace(&id, "ID")
}

/// Every row the policy decision path owns, keyed by table.
fn dump(store: &MetaStore) -> Vec<(&'static str, Vec<(String, String)>)> {
    let txn = store.db.begin_read().unwrap();
    let mut tables = Vec::new();
    let rows = |table: redb::ReadOnlyTable<&str, &[u8]>, normalize: fn(&[u8]) -> String| {
        table
            .iter()
            .unwrap()
            .map(|entry| {
                let (key, value) = entry.unwrap();
                (key.value().to_owned(), normalize(value.value()))
            })
            .collect::<Vec<_>>()
    };
    tables.push(("history", rows(txn.open_table(POLICY_DECISION).unwrap(), without_id)));
    tables.push((
        "current_id",
        rows(txn.open_table(POLICY_DECISION_CURRENT_ID).unwrap(), without_id),
    ));
    tables.push((
        "generation",
        rows(txn.open_table(POLICY_INPUT_GENERATION).unwrap(), |bytes| {
            String::from_utf8(bytes.to_vec()).unwrap()
        }),
    ));
    tables.push((
        "current",
        txn.open_table(POLICY_DECISION_CURRENT)
            .unwrap()
            .iter()
            .unwrap()
            .map(|entry| {
                let (key, value) = entry.unwrap();
                (key.value().to_owned(), value.value().to_owned())
            })
            .collect(),
    ));
    tables.push((
        "serial",
        txn.open_table(SERIAL)
            .unwrap()
            .iter()
            .unwrap()
            .map(|entry| {
                let (key, value) = entry.unwrap();
                (key.value().to_owned(), value.value().to_string())
            })
            .collect(),
    ));
    tables
}

fn record_one_at_a_time(store: &MetaStore, decision: &NewPolicyDecision<'_>) {
    let txn = store.db.begin_write().unwrap();
    record_in(&txn, decision).unwrap();
    prune_history(&txn, HISTORY_LIMIT).unwrap();
    txn.commit().unwrap();
}

fn record_as_a_batch(store: &MetaStore, decisions: &[Spec]) {
    let txn = store.db.begin_write().unwrap();
    let mut batch = BatchWriter::new(&txn).unwrap();
    for decision in decisions {
        batch.record(&decision.as_new()).unwrap();
    }
    batch.finish().unwrap();
    prune_history(&txn, HISTORY_LIMIT).unwrap();
    txn.commit().unwrap();
}

/// Moves a repository's generations on both stores between batches, as other transactions do.
fn advance(stores: [&MetaStore; 2], random: &mut Random) {
    let repository = REPOSITORIES[random.below(REPOSITORIES.len())];
    let by_repository = random.below(2) == 0;
    for store in stores {
        if by_repository {
            store
                .advance_repository_generations(&BTreeSet::from([repository.to_owned()]))
                .unwrap();
        } else {
            store.advance_policy_generation(repository).unwrap();
        }
    }
}

#[rstest]
fn test_a_batch_leaves_what_the_same_decisions_recorded_one_at_a_time_leave(#[values(1, 2, 3, 4, 5, 6)] seed: u64) {
    let (_sequential_dir, sequential) = store();
    let (_batched_dir, batched) = store();
    let mut random = Random(seed);
    let mut clock = 0;
    // Enough decisions to cross the history bound several times and repeat subjects.
    for _ in 0..12 {
        let size = 1 + random.below(9);
        let chunk = (0..size)
            .map(|_| {
                clock += 1;
                Spec::random(&mut random, clock)
            })
            .collect::<Vec<_>>();
        for decision in &chunk {
            record_one_at_a_time(&sequential, &decision.as_new());
        }
        record_as_a_batch(&batched, &chunk);
        advance([&sequential, &batched], &mut random);

        assert_eq!(dump(&batched), dump(&sequential));
    }

    let history = dump(&batched).remove(0).1;
    assert_eq!(history.len(), HISTORY_LIMIT);
}

#[test]
fn test_a_batch_assigns_sequential_ids_and_stamps_each_record_with_its_repository_generation() {
    let (_dir, store) = store();
    store.advance_policy_generation("alpha").unwrap();
    let chunk = ["alpha", "beta", "alpha"].map(|repository| Spec {
        repository,
        resource: "one",
        group: None,
        artifact: None,
        source: None,
        action: PolicyAction::Serve,
        state: PolicyDecisionState::Allow,
        rule: None,
        reason: None,
        evaluated_at_unix: 0,
        next_eligible_at_unix: None,
    });

    record_as_a_batch(&store, &chunk);

    let page = store
        .query_policy_decisions(&PolicyDecisionQuery {
            limit: 10,
            ..PolicyDecisionQuery::default()
        })
        .unwrap();
    let observed = page
        .decisions
        .iter()
        .rev()
        .map(|item| (item.record.repository.as_str(), item.record.input_generation.policy))
        .collect::<Vec<_>>();
    assert_eq!(observed, [("alpha", 1), ("beta", 0), ("alpha", 1)]);
    assert_eq!(
        dump(&store)[0].1.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["pd_0000000000000001", "pd_0000000000000002", "pd_0000000000000003"]
    );
}
