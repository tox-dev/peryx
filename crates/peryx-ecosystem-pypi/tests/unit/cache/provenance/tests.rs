#[test]
fn test_artifact_project_falls_back_for_a_legacy_distribution() {
    assert_eq!(super::artifact_project("Legacy_Name-1.0.egg"), "legacy-name");
}
