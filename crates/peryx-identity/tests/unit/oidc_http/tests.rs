use reqwest::header::HeaderValue;
use rstest::rstest;

use super::*;

const NOW: i64 = 2_000_000_000;

fn policy(lines: &[&str]) -> CachePolicy {
    let mut headers = HeaderMap::new();
    for line in lines {
        headers.append(CACHE_CONTROL, HeaderValue::from_str(line).unwrap());
    }
    cache_policy(&headers)
}

fn window(lines: &[&str]) -> (i64, i64, bool) {
    let window = policy(lines).window(NOW);
    (window.fresh_until - NOW, window.hard_until - NOW, window.storable)
}

#[rstest]
#[case::absent(&[], (DEFAULT_FRESH_SECS, HARD_CACHE_SECS, true))]
#[case::max_age(&["max-age=120"], (120, HARD_CACHE_SECS, true))]
#[case::below_the_old_floor(&["max-age=30"], (30, HARD_CACHE_SECS, true))]
#[case::above_the_ceiling(&["max-age=100000"], (MAX_FRESH_SECS, HARD_CACHE_SECS, true))]
#[case::saturating(&["max-age=99999999999999999999999999"], (MAX_FRESH_SECS, HARD_CACHE_SECS, true))]
#[case::quoted(&["max-age=\"45\""], (45, HARD_CACHE_SECS, true))]
#[case::zero(&["max-age=0"], (0, HARD_CACHE_SECS, true))]
#[case::zero_must_revalidate(&["max-age=0, must-revalidate"], (0, 0, true))]
#[case::proxy_revalidate(&["proxy-revalidate, max-age=30"], (30, 30, true))]
#[case::no_cache(&["no-cache"], (0, 0, true))]
#[case::qualified_no_cache(&["no-cache=\"Set-Cookie, Authorization\", max-age=600"], (0, 0, true))]
#[case::mixed_case(&["No-Cache"], (0, 0, true))]
#[case::no_store(&["no-store"], (DEFAULT_FRESH_SECS, HARD_CACHE_SECS, false))]
#[case::private(&["private, max-age=\"0\", must-revalidate"], (0, 0, false))]
#[case::unknown_directive(&["community=\"peryx\", max-age=30"], (30, HARD_CACHE_SECS, true))]
#[case::space_before_separator(&["max-age =45"], (45, HARD_CACHE_SECS, true))]
#[case::space_after_separator(&["max-age= 45"], (0, HARD_CACHE_SECS, true))]
#[case::no_argument(&["max-age"], (0, HARD_CACHE_SECS, true))]
#[case::empty_argument(&["max-age="], (0, HARD_CACHE_SECS, true))]
#[case::non_numeric(&["max-age=abc"], (0, HARD_CACHE_SECS, true))]
#[case::quoted_non_numeric(&["max-age=\"12,30\""], (0, HARD_CACHE_SECS, true))]
#[case::trailing_garbage(&["max-age=\"12\"x"], (0, HARD_CACHE_SECS, true))]
#[case::repeated(&["max-age=120, max-age=300"], (0, HARD_CACHE_SECS, true))]
#[case::unbalanced_quote(&["max-age=\"120"], (0, 0, true))]
#[case::separate_field_lines(&["max-age=30", "must-revalidate"], (30, 30, true))]
#[case::repeated_across_field_lines(&["max-age=30", "max-age=120"], (0, HARD_CACHE_SECS, true))]
fn test_cache_control_window(#[case] lines: &[&str], #[case] expected: (i64, i64, bool)) {
    assert_eq!(window(lines), expected);
}

#[test]
fn test_undecodable_field_line_requires_validation() {
    let mut headers = HeaderMap::new();
    headers.append(CACHE_CONTROL, HeaderValue::from_bytes(&[0xff]).unwrap());
    let window = cache_policy(&headers).window(NOW);
    assert_eq!((window.fresh_until, window.hard_until), (NOW, NOW));
}

#[rstest]
#[case::shortest_age(&["max-age=120"], &["max-age=30"], (30, HARD_CACHE_SECS, true))]
#[case::default_against_age(&[], &["max-age=30"], (30, HARD_CACHE_SECS, true))]
#[case::either_forbids_storage(&["no-store"], &["max-age=120"], (120, HARD_CACHE_SECS, false))]
#[case::either_demands_validation(&["max-age=120"], &["no-cache"], (0, 0, true))]
#[case::either_forbids_stale(&["must-revalidate"], &["max-age=120"], (120, 120, true))]
fn test_strictest_of_two_documents(
    #[case] discovery: &[&str],
    #[case] jwks: &[&str],
    #[case] expected: (i64, i64, bool),
) {
    let window = policy(discovery).strictest(&policy(jwks)).window(NOW);
    assert_eq!(
        (window.fresh_until - NOW, window.hard_until - NOW, window.storable),
        expected
    );
}
