use rstest::rstest;
use toml::Table;

use super::*;

fn realms(toml: &str) -> Result<TokenRealms, String> {
    let table = toml.parse::<Table>().unwrap();
    TokenRealms::parse(table.get(TOKEN_REALMS).unwrap())
}

fn url(value: &str) -> Url {
    Url::parse(value).unwrap()
}

#[rstest]
#[case::default_port(r#"token_realms = ["https://auth.example"]"#, "https://auth.example/token")]
#[case::trailing_slash(r#"token_realms = ["https://auth.example/"]"#, "https://auth.example/token")]
#[case::explicit_default_port(r#"token_realms = ["https://auth.example:443"]"#, "https://auth.example/token")]
#[case::custom_port(r#"token_realms = ["https://auth.example:8443"]"#, "https://auth.example:8443/token")]
#[case::cleartext(r#"token_realms = ["http://auth.internal:5001"]"#, "http://auth.internal:5001/token")]
#[case::uppercase_host(r#"token_realms = ["https://Auth.Example"]"#, "https://auth.example/token")]
#[case::second_entry(
    r#"token_realms = ["https://other.example", "https://auth.example"]"#,
    "https://auth.example/token"
)]
fn test_allows_a_configured_realm_origin(#[case] toml: &str, #[case] realm: &str) {
    assert!(
        realms(toml)
            .unwrap()
            .allows(&url("https://registry.example/"), &url(realm))
    );
}

#[rstest]
#[case::other_host("https://collector.example/token")]
#[case::subdomain("https://sso.auth.example/token")]
#[case::scheme_downgrade("http://auth.example/token")]
#[case::other_port("https://auth.example:8443/token")]
fn test_refuses_an_origin_outside_the_configured_list(#[case] realm: &str) {
    let configured = realms(r#"token_realms = ["https://auth.example"]"#).unwrap();

    assert!(!configured.allows(&url("https://registry.example/"), &url(realm)));
}

#[rstest]
#[case::same_path("https://registry.example/token")]
#[case::other_path("https://registry.example/v2/auth")]
#[case::explicit_default_port("https://registry.example:443/token")]
fn test_allows_the_upstream_origin_without_configuration(#[case] realm: &str) {
    assert!(TokenRealms::default().allows(&url("https://registry.example/v2/"), &url(realm)));
}

#[test]
fn test_allows_a_cleartext_upstream_its_own_realm() {
    let base = url("http://localhost:5000/");

    assert!(TokenRealms::default().allows(&base, &url("http://localhost:5000/token")));
}

#[test]
fn test_refuses_a_realm_that_only_shares_the_upstream_host() {
    let base = url("https://registry.example/");

    assert!(!TokenRealms::default().allows(&base, &url("http://registry.example/token")));
}

#[rstest]
#[case::not_a_url(
    r#"token_realms = ["auth.example"]"#,
    r#"`token_realms` entry "auth.example" is not an absolute URL"#
)]
#[case::wrong_scheme(
    r#"token_realms = ["ftp://auth.example"]"#,
    r#"`token_realms` entry "ftp://auth.example" must use http or https"#
)]
#[case::username(
    r#"token_realms = ["https://bob@auth.example"]"#,
    r#"`token_realms` entry "https://bob@auth.example" must not carry userinfo"#
)]
#[case::password(
    r#"token_realms = ["https://:secret@auth.example"]"#,
    r#"`token_realms` entry "https://:secret@auth.example" must not carry userinfo"#
)]
#[case::path(
    r#"token_realms = ["https://auth.example/token"]"#,
    r#"`token_realms` entry "https://auth.example/token" must not carry a path"#
)]
#[case::query(
    r#"token_realms = ["https://auth.example/?service=reg"]"#,
    r#"`token_realms` entry "https://auth.example/?service=reg" must not carry a query or fragment"#
)]
#[case::fragment(
    r#"token_realms = ["https://auth.example/#tail"]"#,
    r#"`token_realms` entry "https://auth.example/#tail" must not carry a query or fragment"#
)]
#[case::not_a_string("token_realms = [1]", "`token_realms` entries must be strings, not 1")]
#[case::not_an_array(
    r#"token_realms = "https://auth.example""#,
    r#"`token_realms` must be an array of origins, not "https://auth.example""#
)]
fn test_parse_rejects_a_malformed_entry(#[case] toml: &str, #[case] expected: &str) {
    assert_eq!(realms(toml).unwrap_err(), expected);
}

#[test]
fn test_parse_reads_an_empty_list() {
    assert_eq!(realms("token_realms = []").unwrap(), TokenRealms::default());
}
