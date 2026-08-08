use super::project_of_key;
use rstest::rstest;

#[rstest]
#[case::project_marker("pypi\u{0}p\u{0}hosted/flask", Some(("hosted", "flask")))]
#[case::upload("pypi\u{0}u\u{0}hosted/flask/flask-1.0-py3-none-any.whl", Some(("hosted", "flask")))]
#[case::override_marker("pypi\u{0}o\u{0}hosted/flask/flask-1.0.tar.gz", Some(("hosted", "flask")))]
#[case::slashed_index("pypi\u{0}p\u{0}team/dev/flask", Some(("team/dev", "flask")))]
#[case::slashed_index_upload("pypi\u{0}u\u{0}team/dev/flask/flask-1.0.whl", Some(("team/dev", "flask")))]
#[case::file_digest("pypi\u{0}f\u{0}deadbeef", None)]
#[case::metadata_digest("pypi\u{0}d\u{0}deadbeef", None)]
#[case::foreign_prefix("oci\u{0}m\u{0}store/app", None)]
fn test_project_of_key_maps_project_upload_and_override_keys(
    #[case] key: &str,
    #[case] expected: Option<(&str, &str)>,
) {
    assert_eq!(project_of_key(key), expected);
}
