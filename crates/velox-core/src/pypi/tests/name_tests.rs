use crate::pypi::{PackageName, normalize_name};

#[test]
fn test_normalize_name_matches_pep503() {
    let cases = [
        ("Flask", "flask"),
        ("Django-REST", "django-rest"),
        ("zope.interface", "zope-interface"),
        ("A__B", "a-b"),
        ("foo.bar_baz", "foo-bar-baz"),
        ("already-normal", "already-normal"),
        ("Mixed._-Seps", "mixed-seps"),
        ("UPPER", "upper"),
        ("_leading", "-leading"),
        ("trailing_", "trailing-"),
    ];
    for (input, expected) in cases {
        assert_eq!(normalize_name(input), expected, "{input:?}");
    }
}

#[test]
fn test_package_name_normalizes_and_displays() {
    let name = PackageName::new("Zope.Interface");
    assert_eq!(name.as_str(), "zope-interface");
    assert_eq!(name.to_string(), "zope-interface");
}

#[test]
fn test_package_name_equal_when_normalized_equal() {
    assert_eq!(PackageName::new("Foo_Bar"), PackageName::new("foo-bar"));
}
