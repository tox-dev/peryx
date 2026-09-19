//! Each of these builder functions was found with a surviving `Default::default()` mutant: a
//! wrong-return would still type-check as an `OperationBuilder`/`ParameterBuilder`/`ResponseBuilder`,
//! so only a field the default construction cannot carry - a name, a summary, a description -
//! actually distinguishes the real builder from a blank one.

#[test]
fn test_unyank_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::publish::unyank().build()).unwrap()["summary"],
        "Un-yank files"
    );
}

#[test]
fn test_delete_version_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::publish::delete_version().build()).unwrap()["summary"],
        "Delete a version"
    );
}

#[test]
fn test_delete_project_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::publish::delete_project().build()).unwrap()["summary"],
        "Delete a project"
    );
}

#[test]
fn test_yank_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::publish::yank().build()).unwrap()["summary"],
        "Yank files"
    );
}

#[test]
fn test_restore_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::publish::restore().build()).unwrap()["summary"],
        "Restore hidden files"
    );
}

#[test]
fn test_if_modified_since_param_names_the_header() {
    assert_eq!(
        serde_json::to_value(super::shared::if_modified_since_param().build()).unwrap()["name"],
        "If-Modified-Since"
    );
}

#[test]
fn test_project_param_names_the_path_segment() {
    assert_eq!(
        serde_json::to_value(super::shared::project_param().build()).unwrap()["name"],
        "project"
    );
}

#[test]
fn test_version_param_names_the_path_segment() {
    assert_eq!(
        serde_json::to_value(super::shared::version_param().build()).unwrap()["name"],
        "version"
    );
}

#[test]
fn test_accept_param_names_the_header() {
    assert_eq!(
        serde_json::to_value(super::shared::accept_param().build()).unwrap()["name"],
        "Accept"
    );
}

#[test]
fn test_read_challenge_describes_the_credential_it_wants() {
    let description = serde_json::to_value(super::shared::read_challenge().build()).unwrap()["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(description.contains("does not allow anonymous reads"), "{description}");
}

#[test]
fn test_forbidden_read_response_describes_the_denial() {
    assert_eq!(
        serde_json::to_value(super::shared::forbidden_read_response().build()).unwrap()["description"],
        "The presented credential grants no read of this resource"
    );
}

#[test]
fn test_sha256_param_names_the_path_segment() {
    assert_eq!(
        serde_json::to_value(super::shared::sha256_param().build()).unwrap()["name"],
        "sha256"
    );
}

#[test]
fn test_range_param_names_the_header() {
    assert_eq!(
        serde_json::to_value(super::shared::range_param().build()).unwrap()["name"],
        "Range"
    );
}

#[test]
fn test_if_none_match_param_names_the_header() {
    assert_eq!(
        serde_json::to_value(super::shared::if_none_match_param().build()).unwrap()["name"],
        "If-None-Match"
    );
}

#[test]
fn test_if_range_param_names_the_header() {
    assert_eq!(
        serde_json::to_value(super::shared::if_range_param().build()).unwrap()["name"],
        "If-Range"
    );
}

#[test]
fn test_oidc_audience_names_its_operation() {
    assert_eq!(
        serde_json::to_value(super::trusted_publishing::oidc_audience().build()).unwrap()["summary"],
        "Discover the CI identity audience"
    );
}
