use crate::{UserId, UserName, UserNameError};
use rstest::rstest;

#[test]
fn test_user_id_is_opaque_and_random() {
    let first = UserId::random();
    let second = UserId::random();

    assert!(first.as_str().starts_with("usr_"));
    assert_eq!(first.as_str().len(), 36);
    assert_eq!(first.to_string(), first.as_str());
    assert_ne!(first, second);
}

#[test]
fn test_user_id_rehydrates_a_stored_value_verbatim() {
    assert_eq!(UserId::from_stored("usr_stored").as_str(), "usr_stored");
}

#[rstest]
#[case::normalization("E\u{301}LODIE", "Élodie", "élodie")]
#[case::sharp_s("Straße", "STRASSE", "strasse")]
#[case::sigma("ΟΣ", "οσ", "οσ")]
#[case::ypogegrammeni("ῃ", "ηι", "ηι")]
#[case::unicode_16_tje("\u{1c89}", "\u{1c8a}", "\u{1c8a}")]
fn test_user_name_preserves_display_and_canonicalizes_lookup(
    #[case] display: &str,
    #[case] equivalent: &str,
    #[case] canonical: &str,
) {
    let name = UserName::new(format!("  {display}  ").as_str()).unwrap();

    assert_eq!(
        (
            name.display(),
            name.canonical(),
            UserName::new(equivalent).unwrap().canonical()
        ),
        (display, canonical, canonical)
    );
}

#[test]
fn test_user_name_rejects_whitespace() {
    let error = UserName::new(" \n\t ").unwrap_err();

    assert_eq!(error, UserNameError::Empty);
    assert_eq!(error.to_string(), "user display name cannot be empty");
}

#[test]
fn test_user_name_appends_a_canonicalized_user_id_suffix() {
    let name = UserName::new("Alice")
        .unwrap()
        .with_id_suffix(&UserId::from_stored("usr_ÉLODIE"));

    assert_eq!(name.display(), "Alice (usr_ÉLODIE)");
    assert_eq!(name.canonical(), "alice (usr_élodie)");
}
