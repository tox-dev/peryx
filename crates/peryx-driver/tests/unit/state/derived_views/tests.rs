use std::collections::BTreeMap;

use super::{REQUIRED_VIEWS, ReadableFrontier, SEARCH_VIEW, readable_frontier};

#[test]
fn test_no_required_views_expose_the_whole_authority_frontier() {
    assert_eq!(
        readable_frontier(9, &BTreeMap::new(), &[]),
        ReadableFrontier {
            serial: 9,
            blocking: None,
        }
    );
}

#[test]
fn test_a_missing_view_pins_readability_to_zero_and_names_itself() {
    assert_eq!(
        readable_frontier(5, &BTreeMap::new(), REQUIRED_VIEWS),
        ReadableFrontier {
            serial: 0,
            blocking: Some(SEARCH_VIEW.to_owned()),
        }
    );
}

#[test]
fn test_a_caught_up_view_exposes_the_authority_frontier() {
    let frontiers = BTreeMap::from([(SEARCH_VIEW.to_owned(), 5)]);
    assert_eq!(
        readable_frontier(5, &frontiers, REQUIRED_VIEWS),
        ReadableFrontier {
            serial: 5,
            blocking: None,
        }
    );
}

#[test]
fn test_the_lagging_view_owns_the_minimum_and_holds_the_frontier() {
    let frontiers = BTreeMap::from([("search".to_owned(), 2), ("cache".to_owned(), 4)]);
    assert_eq!(
        readable_frontier(6, &frontiers, &["cache", "search"]),
        ReadableFrontier {
            serial: 2,
            blocking: Some("search".to_owned()),
        }
    );
}

#[test]
fn test_a_view_ahead_of_the_authority_never_lifts_readability_above_it() {
    let frontiers = BTreeMap::from([(SEARCH_VIEW.to_owned(), 8)]);
    assert_eq!(
        readable_frontier(3, &frontiers, REQUIRED_VIEWS),
        ReadableFrontier {
            serial: 3,
            blocking: None,
        }
    );
}
