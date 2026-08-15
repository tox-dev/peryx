use super::*;
use rstest::rstest;

#[rstest]
#[case::absent("", LibraryPrefix::Auto)]
#[case::auto("library_prefix = \"auto\"", LibraryPrefix::Auto)]
#[case::always("library_prefix = true", LibraryPrefix::Always)]
#[case::never("library_prefix = false", LibraryPrefix::Never)]
fn test_compile_reads_library_prefix(#[case] toml: &str, #[case] expected: LibraryPrefix) {
    let settings = IndexSettings::compile(&toml.parse::<Table>().unwrap()).unwrap();
    assert_eq!(settings.library_prefix, expected);
}

#[test]
fn test_compile_rejects_an_unknown_key() {
    let settings = "libary_prefix = true".parse::<Table>().unwrap();
    assert_eq!(
        IndexSettings::compile(&settings).unwrap_err(),
        "unknown field `libary_prefix` in `[index.settings]`"
    );
}

#[rstest]
#[case::string("library_prefix = \"always\"")]
#[case::integer("library_prefix = 1")]
fn test_compile_rejects_an_invalid_library_prefix(#[case] toml: &str) {
    let err = IndexSettings::compile(&toml.parse::<Table>().unwrap()).unwrap_err();
    assert!(
        err.starts_with("`library_prefix` must be true, false, or \"auto\""),
        "{err}"
    );
}

#[rstest]
#[case::auto_hub(LibraryPrefix::Auto, "https://registry-1.docker.io/", "ubuntu", "library/ubuntu")]
#[case::auto_hub_alias(LibraryPrefix::Auto, "https://index.docker.io/", "ubuntu", "library/ubuntu")]
#[case::auto_hub_short(LibraryPrefix::Auto, "https://docker.io/", "ubuntu", "library/ubuntu")]
#[case::auto_other(LibraryPrefix::Auto, "https://ghcr.io/", "ubuntu", "ubuntu")]
#[case::auto_hub_multi(LibraryPrefix::Auto, "https://registry-1.docker.io/", "acme/app", "acme/app")]
#[case::always_other(LibraryPrefix::Always, "https://mirror.example/", "ubuntu", "library/ubuntu")]
#[case::always_multi(LibraryPrefix::Always, "https://mirror.example/", "acme/app", "acme/app")]
#[case::never_hub(LibraryPrefix::Never, "https://registry-1.docker.io/", "ubuntu", "ubuntu")]
#[case::auto_unparseable_base(LibraryPrefix::Auto, "not a url", "ubuntu", "ubuntu")]
fn test_upstream_repo_rewrites_only_a_single_segment_hub_name(
    #[case] prefix: LibraryPrefix,
    #[case] base: &str,
    #[case] repo: &str,
    #[case] expected: &str,
) {
    assert_eq!(upstream_repo(prefix, base, repo), expected);
}
