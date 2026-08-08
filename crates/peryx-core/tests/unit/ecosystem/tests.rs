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
