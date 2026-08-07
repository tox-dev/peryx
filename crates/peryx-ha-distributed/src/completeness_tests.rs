use crate::analytics::ProducerId;
use crate::completeness::{Completeness, ProducerCoverage, assess};
use crate::envelope::AuthorityEpoch;

fn cov(producer: &str, accepted: Option<(u64, u64)>, required: (u64, u64)) -> ProducerCoverage {
    ProducerCoverage {
        producer: ProducerId(producer.to_owned()),
        accepted: accepted.map(|(epoch, sequence)| (AuthorityEpoch(epoch), sequence)),
        required: (AuthorityEpoch(required.0), required.1),
    }
}

#[test]
fn test_every_producer_covered_is_complete() {
    let coverage = [cov("east", Some((1, 10)), (1, 10)), cov("west", Some((2, 5)), (2, 3))];

    assert_eq!(assess(&coverage), Completeness::Complete);
}

#[test]
fn test_a_trailing_producer_is_delayed() {
    let coverage = [cov("east", Some((1, 10)), (1, 10)), cov("west", Some((1, 2)), (1, 5))];

    assert_eq!(assess(&coverage), Completeness::Delayed);
}

#[test]
fn test_a_missing_producer_is_unavailable_even_with_covered() {
    let coverage = [
        cov("east", Some((1, 10)), (1, 10)),
        cov("west", None, (1, 5)),
        cov("south", Some((1, 9)), (1, 9)),
    ];

    assert_eq!(assess(&coverage), Completeness::Unavailable);
}

#[test]
fn test_missing_outranks_behind() {
    let coverage = [cov("east", Some((1, 2)), (1, 5)), cov("west", None, (1, 5))];

    assert_eq!(assess(&coverage), Completeness::Unavailable);
}

#[test]
fn test_accepted_equal_to_required_is_covered() {
    let coverage = [cov("east", Some((3, 7)), (3, 7))];

    assert_eq!(assess(&coverage), Completeness::Complete);
}

#[test]
fn test_a_higher_epoch_covers_a_lower_epoch_boundary() {
    let coverage = [cov("east", Some((2, 0)), (1, 100))];

    assert_eq!(assess(&coverage), Completeness::Complete);
}

#[test]
fn test_empty_expected_set_is_unavailable() {
    assert_eq!(assess(&[]), Completeness::Unavailable);
}
