use std::collections::{BTreeSet, HashSet};

use super::{Ecosystem, InvalidEcosystem};

#[test]
fn identity_round_trips_through_text_and_serde() {
    let ecosystem: Ecosystem = "example-2".parse().unwrap();
    assert_eq!(ecosystem.as_str(), "example-2");
    assert_eq!(ecosystem.to_string(), "example-2");
    assert_eq!(serde_json::from_str::<Ecosystem>(r#""example-2""#).unwrap(), ecosystem);
    assert_eq!(serde_json::to_string(&ecosystem).unwrap(), r#""example-2""#);
}

#[test]
fn identity_rejects_invalid_text() {
    let error = "Example".parse::<Ecosystem>().unwrap_err();
    assert_eq!(error, InvalidEcosystem("Example".to_owned()));
    assert_eq!(error.to_string(), "invalid ecosystem: Example");
}

#[test]
fn static_and_owned_identities_share_value_semantics() {
    let owned: Ecosystem = "example".parse().unwrap();
    let static_value = Ecosystem::new("example");

    assert_eq!(owned, static_value);
    assert_eq!(owned.cmp(&Ecosystem::new("later")), std::cmp::Ordering::Less);
    assert_eq!(HashSet::from([owned]).get(&static_value), Some(&static_value));
}

#[test]
fn many_unknown_identities_remain_independent_values() {
    let identities = (0..4_096)
        .map(|index| format!("unknown-{index}").parse::<Ecosystem>().unwrap())
        .collect::<Vec<_>>();
    let retained = identities[2_048].clone();

    assert_eq!(identities.iter().cloned().collect::<HashSet<_>>().len(), 4_096);
    assert_eq!(BTreeSet::<_>::from_iter(identities).len(), 4_096);
    assert_eq!(retained.as_str(), "unknown-2048");
}
