use super::{BaseUrl, SnippetKind, snippet_text};

#[test]
fn test_relative_links_are_used_without_base_url() {
    let urls = super::IndexUrls::new(None, "root/pypi", true);
    assert_eq!(urls.api, "/root/pypi/+api");
    assert_eq!(urls.simple, "/root/pypi/simple/");
    assert_eq!(urls.upload, Some("/root/pypi/".to_owned()));
    assert_eq!(urls.web, "/browse?index=root%2Fpypi");
}

#[test]
fn test_snippets_use_absolute_urls_and_redact_token() {
    let base = BaseUrl::parse("https://packages.example/cache/").unwrap();
    let text = snippet_text(&base, "root/pypi", true, SnippetKind::Pypirc).unwrap();
    assert_eq!(
        text,
        "[distutils]\nindex-servers =\n    peryx\n\n[peryx]\nrepository = https://packages.example/cache/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
    );
}
