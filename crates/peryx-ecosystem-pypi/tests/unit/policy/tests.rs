use super::PackageType;

#[test]
fn test_package_type_parse_rejects_an_unknown_value() {
    assert_eq!(PackageType::parse("wheel"), Some(PackageType::Wheel));
    assert_eq!(PackageType::parse("sdist"), Some(PackageType::Sdist));
    assert_eq!(PackageType::parse("egg"), None);
}
