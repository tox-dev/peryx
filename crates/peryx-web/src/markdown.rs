//! Link safety for project pages.
//!
//! Package authors control the URLs on a project page, so links are kept only when their scheme is one
//! a browser can follow safely, and outbound ones carry a hardened relationship. The long description
//! itself is rendered to HTML by the ecosystem driver on the server, not here.

use url::{ParseError, Url};

pub(crate) const EXTERNAL_LINK_REL: &str = "external nofollow noopener noreferrer";

/// An HTTP or HTTPS destination leaves the UI and gets the hardened relationship; a relative peryx
/// route stays inside it and gets none.
pub(crate) fn external_link_rel(target: &str) -> Option<&'static str> {
    Url::parse(target)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        .then_some(EXTERNAL_LINK_REL)
}

pub(crate) fn is_safe_link(target: &str) -> bool {
    is_safe_url(target, |scheme| matches!(scheme, "http" | "https" | "mailto"))
}

pub(crate) fn is_safe_artifact_link(target: &str) -> bool {
    is_safe_url(target, |scheme| matches!(scheme, "http" | "https"))
}

fn is_safe_url(target: &str, allowed_scheme: impl FnOnce(&str) -> bool) -> bool {
    match Url::parse(target) {
        Ok(url) => allowed_scheme(url.scheme()),
        Err(ParseError::RelativeUrlWithoutBase) => true,
        Err(_) => false,
    }
}
