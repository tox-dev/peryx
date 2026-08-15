use crate::{UserId, UserName, UserNameError};

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

#[test]
fn test_user_name_preserves_display_and_canonicalizes_lookup() {
    let name = UserName::new("  E\u{301}LODIE  ").unwrap();

    assert_eq!(name.display(), "E\u{301}LODIE");
    assert_eq!(name.canonical(), "élodie");
    assert_eq!(name.canonical(), UserName::new("Élodie").unwrap().canonical());
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
