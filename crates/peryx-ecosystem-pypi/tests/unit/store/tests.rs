use super::project_of_key;

#[test]
fn test_project_of_key_maps_project_upload_and_override_keys() {
    for (key, expected) in [
        ("pypi\u{0}p\u{0}hosted/flask", Some(("hosted", "flask"))),
        (
            "pypi\u{0}u\u{0}hosted/flask/flask-1.0-py3-none-any.whl",
            Some(("hosted", "flask")),
        ),
        (
            "pypi\u{0}o\u{0}hosted/flask/flask-1.0.tar.gz",
            Some(("hosted", "flask")),
        ),
        ("pypi\u{0}p\u{0}team/dev/flask", Some(("team/dev", "flask"))),
        (
            "pypi\u{0}u\u{0}team/dev/flask/flask-1.0.whl",
            Some(("team/dev", "flask")),
        ),
        ("pypi\u{0}f\u{0}deadbeef", None),
        ("pypi\u{0}d\u{0}deadbeef", None),
        ("oci\u{0}m\u{0}store/app", None),
    ] {
        assert_eq!(project_of_key(key), expected, "{key}");
    }
}
