use super::{ArtifactKey, GroupKey, RepositoryKey, ResourceKey};

#[test]
fn test_coordinates_preserve_owner_values() {
    assert_eq!(
        [
            RepositoryKey::from("root/private").to_string(),
            ResourceKey::from("library/example").to_string(),
            GroupKey::from("stable").to_string(),
            ArtifactKey::from("sha256:abc").to_string(),
        ],
        ["root/private", "library/example", "stable", "sha256:abc"]
    );
}

#[test]
fn test_coordinates_serialize_as_strings() {
    let coordinate = ResourceKey::new(String::from("owner/value"));
    assert_eq!(serde_json::to_string(&coordinate).unwrap(), r#""owner/value""#);
    assert_eq!(
        serde_json::from_str::<ResourceKey>(r#""owner/value""#).unwrap(),
        coordinate
    );
}

#[test]
fn test_coordinates_expose_owned_and_borrowed_values() {
    macro_rules! assert_contract {
        ($coordinate:ty) => {
            let borrowed = <$coordinate>::from("borrowed");
            assert_eq!(AsRef::<str>::as_ref(&borrowed), "borrowed");
            assert_eq!(std::borrow::Borrow::<str>::borrow(&borrowed), "borrowed");
            assert_eq!(<$coordinate>::from(String::from("string")).as_str(), "string");
            assert_eq!(
                <$coordinate>::from(Box::<str>::from("boxed")).into_boxed_str(),
                Box::<str>::from("boxed")
            );
        };
    }

    assert_contract!(RepositoryKey);
    assert_contract!(ResourceKey);
    assert_contract!(GroupKey);
    assert_contract!(ArtifactKey);
}
