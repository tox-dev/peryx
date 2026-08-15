//! Link classification for rendered owner content.

use url::Url;

pub(crate) const EXTERNAL_LINK_REL: &str = "external nofollow noopener noreferrer";

/// An HTTP or HTTPS destination leaves the UI and gets the hardened relationship; a relative peryx
/// route stays inside it and gets none.
pub(crate) fn external_link_rel(target: &str) -> Option<&'static str> {
    let external = is_network_path_reference(target)
        || Url::parse(target).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
    external.then_some(EXTERNAL_LINK_REL)
}

/// A `//host/path` network-path reference has no scheme, so `Url::parse` rejects it as relative even
/// though a browser resolves it to an off-host HTTP or HTTPS URL. Classify it as external so it never
/// passes as a same-origin route.
fn is_network_path_reference(target: &str) -> bool {
    target.starts_with("//")
}
