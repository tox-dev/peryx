use rstest::rstest;

use super::{FileOverride, OverrideMutation};
use crate::Yanked;

fn yanked(yanked: Yanked) -> FileOverride {
    FileOverride { hidden: false, yanked }
}

fn hidden() -> FileOverride {
    FileOverride {
        hidden: true,
        yanked: Yanked::No,
    }
}

#[rstest]
#[case::empty(FileOverride::default(), r#"{"hidden":false,"yanked":false}"#)]
#[case::hidden(hidden(), r#"{"hidden":true,"yanked":false}"#)]
#[case::yanked(yanked(Yanked::Yes), r#"{"hidden":false,"yanked":true}"#)]
#[case::reason(yanked(Yanked::Reason("CVE-2026-1234".to_owned())), r#"{"hidden":false,"yanked":"CVE-2026-1234"}"#)]
#[case::both(
    FileOverride { hidden: true, yanked: Yanked::Reason("bad build".to_owned()) },
    r#"{"hidden":true,"yanked":"bad build"}"#
)]
fn test_file_override_round_trips_through_its_stored_form(#[case] record: FileOverride, #[case] encoded: &str) {
    assert_eq!(record.encode(), encoded);
    assert_eq!(FileOverride::decode(encoded), Some(record));
}

#[rstest]
#[case::not_json("hidden")]
#[case::legacy_hidden_scalar(r#""hidden""#)]
#[case::unknown_field(r#"{"hidden":false,"yanked":false,"kind":"yanked"}"#)]
#[case::missing_yanked(r#"{"hidden":true}"#)]
#[case::missing_hidden(r#"{"yanked":true}"#)]
#[case::wrong_hidden_type(r#"{"hidden":"true","yanked":false}"#)]
fn test_file_override_rejects_a_record_it_did_not_write(#[case] raw: &str) {
    assert_eq!(FileOverride::decode(raw), None);
}

#[rstest]
#[case::nothing(FileOverride::default(), true)]
#[case::hidden(hidden(), false)]
#[case::yanked(yanked(Yanked::Yes), false)]
fn test_file_override_is_empty_only_when_it_imposes_nothing(#[case] record: FileOverride, #[case] empty: bool) {
    assert_eq!(record.is_empty(), empty);
}

#[test]
fn test_decode_all_keeps_readable_records_and_drops_corrupt_ones() {
    let decoded = FileOverride::decode_all(vec![
        ("demo-1.0.whl".to_owned(), hidden().encode()),
        ("demo-2.0.whl".to_owned(), "garbage".to_owned()),
    ]);
    assert_eq!(
        decoded,
        std::collections::BTreeMap::from([("demo-1.0.whl".to_owned(), hidden())])
    );
}

#[rstest]
#[case::hide(FileOverride::default(), OverrideMutation::Hidden(true), Some("hide"), hidden())]
#[case::restore(hidden(), OverrideMutation::Hidden(false), Some("restore"), FileOverride::default())]
#[case::yank(FileOverride::default(), OverrideMutation::Yanked(&Yanked::Yes), Some("yank"), yanked(Yanked::Yes))]
#[case::reyank(
    yanked(Yanked::Yes),
    OverrideMutation::Yanked(&Yanked::Reason(String::from("CVE-2026-1234"))),
    Some("yank"),
    yanked(Yanked::Reason(String::from("CVE-2026-1234")))
)]
#[case::unyank(yanked(Yanked::Yes), OverrideMutation::Yanked(&Yanked::No), Some("unyank"), FileOverride::default())]
#[case::hide_leaves_the_yank(
    yanked(Yanked::Reason(String::from("CVE-2026-1234"))),
    OverrideMutation::Hidden(true),
    Some("hide"),
    FileOverride { hidden: true, yanked: Yanked::Reason(String::from("CVE-2026-1234")) }
)]
#[case::unyank_leaves_the_hide(
    FileOverride { hidden: true, yanked: Yanked::Yes },
    OverrideMutation::Yanked(&Yanked::No),
    Some("unyank"),
    hidden()
)]
#[case::hide_of_a_hidden_file(hidden(), OverrideMutation::Hidden(true), None, hidden())]
#[case::restore_of_a_visible_file(
    FileOverride::default(),
    OverrideMutation::Hidden(false),
    None,
    FileOverride::default()
)]
#[case::unyank_of_an_unyanked_file(
    FileOverride::default(),
    OverrideMutation::Yanked(&Yanked::No),
    None,
    FileOverride::default()
)]
fn test_override_mutation_moves_one_field_and_reports_its_journal_action(
    #[case] mut record: FileOverride,
    #[case] mutation: OverrideMutation<'_>,
    #[case] action: Option<&str>,
    #[case] expected: FileOverride,
) {
    assert_eq!(mutation.apply(&mut record), action);
    assert_eq!(record, expected);
}
