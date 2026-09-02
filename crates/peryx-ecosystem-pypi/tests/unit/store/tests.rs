use super::{derives_no_view, metadata_artifact_of_key, project_of_key};

#[test]
fn test_project_of_key_maps_every_project_scoped_key() {
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
        ("pypi\u{0}i\u{0}root-pypi/flask", Some(("root-pypi", "flask"))),
        ("pypi\u{0}h\u{0}root-pypi/flask", Some(("root-pypi", "flask"))),
        ("pypi\u{0}p\u{0}team/flask", Some(("team", "flask"))),
        ("pypi\u{0}u\u{0}team/flask/flask-1.0.whl", Some(("team", "flask"))),
        ("pypi\u{0}f\u{0}deadbeef", None),
        ("pypi\u{0}d\u{0}deadbeef", None),
        ("oci\u{0}m\u{0}store/app", None),
    ] {
        assert_eq!(project_of_key(key), expected, "{key}");
    }
}

#[test]
fn test_metadata_artifact_of_key_names_only_a_digest_keyed_pointer() {
    for (key, expected) in [
        ("pypi\u{0}d\u{0}deadbeef", Some("deadbeef")),
        ("pypi\u{0}d\u{0}", None),
        ("pypi\u{0}f\u{0}deadbeef", None),
        ("pypi\u{0}p\u{0}hosted/flask", None),
    ] {
        assert_eq!(metadata_artifact_of_key(key), expected, "{key}");
    }
}

#[test]
fn test_derives_no_view_names_the_reporting_and_byte_route_rows() {
    for (key, expected) in [
        ("pypi\u{0}f\u{0}deadbeef", true),
        ("pypi\u{0}k\u{0}hosted", true),
        ("pypi\u{0}w\u{0}hosted\u{0}00000000000000000001/flask", true),
        ("pypi\u{0}d\u{0}deadbeef", false),
        ("pypi\u{0}u\u{0}hosted/flask/flask-1.0.whl", false),
        ("oci\u{0}m\u{0}store/app", false),
    ] {
        assert_eq!(derives_no_view(key), expected, "{key}");
    }
}
