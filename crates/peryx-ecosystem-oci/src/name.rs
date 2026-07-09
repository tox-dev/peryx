//! Parsing a `/v2/<name>/<verb>/<reference>` distribution-spec request into its `name` and the
//! verb-specific tail. `<name>` may contain slashes, so the split is anchored on the known verb
//! segments (`manifests`, `blobs`, `tags`) counted from the end of the path, exactly as the
//! reference registry's route regexes resolve it.

/// A parsed pull-path: the full `<name>` (still carrying the peryx index-route prefix) and what it
/// addresses. The registry resolves the index prefix off `name` afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OciRoute {
    /// `GET /v2/_catalog`: the repository catalog across every configured index.
    Catalog,
    /// `GET|HEAD|PUT|DELETE /v2/<name>/manifests/<reference>`.
    Manifest { name: String, reference: Reference },
    /// `GET|HEAD|DELETE /v2/<name>/blobs/<digest>`.
    Blob { name: String, digest: String },
    /// `GET /v2/<name>/blobs/<digest>/contents`: peryx's own layer file browser, listing the tar
    /// members of a stored layer blob or previewing one text member. Not part of the distribution
    /// spec; a real registry `404`s it, so it never collides with a pull.
    BlobContents { name: String, digest: String },
    /// `GET /v2/<name>/tags/list`.
    TagsList { name: String },
    /// `GET /v2/<name>/referrers/<digest>`: manifests that declare `<digest>` as their subject.
    Referrers { name: String, digest: String },
    /// `POST /v2/<name>/blobs/uploads/`: begin (or cross-repo mount) a blob upload.
    UploadStart { name: String },
    /// `PATCH|PUT /v2/<name>/blobs/uploads/<session>`: append to or finish an upload.
    UploadSession { name: String, session: String },
}

/// A manifest reference is either a mutable tag or an immutable digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(String),
}

/// Classify a full request path (`/v2/...`) into a pull route, or `None` when it is neither a
/// recognized verb nor a well-formed name/reference. The bare `/v2/` version check is handled before
/// this and never reaches here.
#[must_use]
pub fn classify(path: &str) -> Option<OciRoute> {
    let rest = path.strip_prefix("/v2/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest == "_catalog" {
        return Some(OciRoute::Catalog);
    }
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let len = segments.len();
    if len < 2 {
        return None;
    }
    // `blobs/uploads/<session>` (an in-progress upload) is anchored three from the end, before the
    // `blobs/<digest>` shape it would otherwise look like.
    if len >= 3 && segments[len - 3] == "blobs" && segments[len - 2] == "uploads" {
        return Some(OciRoute::UploadSession {
            name: join_name(&segments[..len - 3])?,
            session: segments[len - 1].to_owned(),
        });
    }
    // `blobs/<digest>/contents` (peryx's layer browser) is likewise anchored three from the end,
    // before the bare `blobs/<digest>` pull shape.
    if len >= 3 && segments[len - 3] == "blobs" && segments[len - 1] == "contents" {
        return Some(OciRoute::BlobContents {
            name: join_name(&segments[..len - 3])?,
            digest: parse_digest(segments[len - 2])?,
        });
    }
    let (verb, tail) = (segments[len - 2], segments[len - 1]);
    match verb {
        "blobs" if tail == "uploads" => Some(OciRoute::UploadStart {
            name: join_name(&segments[..len - 2])?,
        }),
        "manifests" => {
            let name = join_name(&segments[..len - 2])?;
            Some(OciRoute::Manifest {
                name,
                reference: parse_reference(tail)?,
            })
        }
        "blobs" => {
            let name = join_name(&segments[..len - 2])?;
            Some(OciRoute::Blob {
                name,
                digest: parse_digest(tail)?,
            })
        }
        "tags" if tail == "list" => Some(OciRoute::TagsList {
            name: join_name(&segments[..len - 2])?,
        }),
        "referrers" => Some(OciRoute::Referrers {
            name: join_name(&segments[..len - 2])?,
            digest: parse_digest(tail)?,
        }),
        _ => None,
    }
}

/// Join validated name components back into the repository name, rejecting an empty name.
fn join_name(components: &[&str]) -> Option<String> {
    if components.is_empty() || !components.iter().all(|component| valid_name_component(component)) {
        return None;
    }
    let name = components.join("/");
    (name.len() <= 255).then_some(name)
}

/// A single `<name>` path component: lowercase alphanumerics with `.`/`_`/`-` separators, never a
/// bare `.`/`..` (which would let a crafted name escape a storage-key or URL path).
fn valid_name_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !bytes.first().is_some_and(|&b| alnum(b)) || !bytes.last().is_some_and(|&b| alnum(b)) {
        return false;
    }
    // Between two alphanumeric runs the OCI grammar allows one separator only: a single `.`, one or
    // two `_`, or a run of `-`. A mixed or longer run (`..`, `._`, `___`) is rejected.
    let mut index = 0;
    while index < bytes.len() {
        if alnum(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && !alnum(bytes[index]) {
            index += 1;
        }
        let separator = &bytes[start..index];
        if separator != b"." && separator != b"_" && separator != b"__" && !separator.iter().all(|&b| b == b'-') {
            return false;
        }
    }
    true
}

/// A manifest reference: a digest if it carries an `algorithm:` prefix, otherwise a tag.
fn parse_reference(reference: &str) -> Option<Reference> {
    if reference.contains(':') {
        parse_digest(reference).map(Reference::Digest)
    } else if valid_tag(reference) {
        Some(Reference::Tag(reference.to_owned()))
    } else {
        None
    }
}

/// A tag: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
fn valid_tag(tag: &str) -> bool {
    let mut bytes = tag.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    tag.len() <= 128
        && (first.is_ascii_alphanumeric() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A content digest `algorithm:encoded`, returned verbatim once shape-checked. The algorithm is a
/// lowercase token; the encoded part is a non-empty hex-ish run. sha256 is verified byte-for-byte
/// later against the stored blob.
fn parse_digest(digest: &str) -> Option<String> {
    let (algorithm, encoded) = digest.split_once(':')?;
    let algorithm_ok = !algorithm.is_empty()
        && algorithm.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'.' | b'_' | b'-')
        });
    // Reject uppercase in the encoding: a digest is a cache and storage key, and peryx serves only
    // lowercase-hex sha256, so accepting `sha256:ABC…` would key a second copy of the same content.
    let encoded_ok = !encoded.is_empty()
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'=' | b'_' | b'-'));
    (algorithm_ok && encoded_ok).then(|| digest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_route_is_recognized() {
        assert_eq!(classify("/v2/_catalog"), Some(OciRoute::Catalog));
        assert_eq!(classify("/v2/_catalog/"), Some(OciRoute::Catalog));
    }

    #[test]
    fn test_valid_name_component_enforces_the_oci_grammar() {
        for ok in ["foo", "foo-bar", "foo--bar", "foo__bar", "foo.bar", "a1b2"] {
            assert!(valid_name_component(ok), "{ok} should be valid");
        }
        for bad in ["", "-foo", "foo-", ".foo", "foo..bar", "foo._bar", "___", "Foo"] {
            assert!(!valid_name_component(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn test_parse_digest_requires_a_lowercase_canonical_encoding() {
        assert!(parse_digest("sha256:abc123").is_some());
        assert!(parse_digest("sha256:ABC123").is_none());
        assert!(parse_digest("nocolon").is_none());
        assert!(parse_digest("sha256:").is_none());
    }

    #[test]
    fn test_manifest_by_tag_splits_a_multi_segment_name() {
        assert_eq!(
            classify("/v2/dockerhub/library/nginx/manifests/latest"),
            Some(OciRoute::Manifest {
                name: "dockerhub/library/nginx".to_owned(),
                reference: Reference::Tag("latest".to_owned()),
            })
        );
    }

    #[test]
    fn test_manifest_by_digest_is_a_digest_reference() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        assert_eq!(
            classify(&format!("/v2/alpine/manifests/{digest}")),
            Some(OciRoute::Manifest {
                name: "alpine".to_owned(),
                reference: Reference::Digest(digest.to_owned()),
            })
        );
    }

    #[test]
    fn test_blob_route_carries_the_digest() {
        let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        assert_eq!(
            classify(&format!("/v2/alpine/blobs/{digest}")),
            Some(OciRoute::Blob {
                name: "alpine".to_owned(),
                digest: digest.to_owned(),
            })
        );
    }

    #[test]
    fn test_blob_contents_route() {
        let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        assert_eq!(
            classify(&format!("/v2/team/app/blobs/{digest}/contents")),
            Some(OciRoute::BlobContents {
                name: "team/app".to_owned(),
                digest: digest.to_owned(),
            })
        );
        assert_eq!(classify("/v2/app/blobs/not-a-digest/contents"), None);
    }

    #[test]
    fn test_referrers_route() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        assert_eq!(
            classify(&format!("/v2/team/app/referrers/{digest}")),
            Some(OciRoute::Referrers {
                name: "team/app".to_owned(),
                digest: digest.to_owned(),
            })
        );
        assert_eq!(classify("/v2/app/referrers/not-a-digest"), None);
    }

    #[test]
    fn test_upload_start_route() {
        assert_eq!(
            classify("/v2/team/app/blobs/uploads/"),
            Some(OciRoute::UploadStart {
                name: "team/app".to_owned(),
            })
        );
        assert_eq!(
            classify("/v2/app/blobs/uploads"),
            Some(OciRoute::UploadStart { name: "app".to_owned() })
        );
    }

    #[test]
    fn test_upload_session_route() {
        assert_eq!(
            classify("/v2/team/app/blobs/uploads/abc123"),
            Some(OciRoute::UploadSession {
                name: "team/app".to_owned(),
                session: "abc123".to_owned(),
            })
        );
    }

    #[test]
    fn test_upload_session_without_a_name_is_rejected() {
        assert_eq!(classify("/v2/blobs/uploads/abc"), None);
    }

    #[test]
    fn test_tags_list_route() {
        assert_eq!(
            classify("/v2/team/app/tags/list"),
            Some(OciRoute::TagsList {
                name: "team/app".to_owned(),
            })
        );
    }

    #[test]
    fn test_trailing_slash_is_tolerated() {
        assert_eq!(
            classify("/v2/app/tags/list/"),
            Some(OciRoute::TagsList { name: "app".to_owned() })
        );
    }

    #[test]
    fn test_unknown_verb_is_rejected() {
        assert_eq!(classify("/v2/app/frobnicate/latest"), None);
    }

    #[test]
    fn test_missing_v2_prefix_is_rejected() {
        assert_eq!(classify("/simple/app/manifests/latest"), None);
    }

    #[test]
    fn test_empty_name_is_rejected() {
        assert_eq!(classify("/v2/manifests/latest"), None);
    }

    #[test]
    fn test_double_slash_is_rejected() {
        assert_eq!(classify("/v2/app//manifests/latest"), None);
    }

    #[test]
    fn test_dot_dot_name_component_is_rejected() {
        assert_eq!(classify("/v2/../secret/manifests/latest"), None);
    }

    #[test]
    fn test_uppercase_name_is_rejected() {
        assert_eq!(classify("/v2/App/manifests/latest"), None);
    }

    #[test]
    fn test_bad_tag_is_rejected() {
        assert_eq!(classify("/v2/app/manifests/-bad"), None);
    }

    #[test]
    fn test_bad_digest_is_rejected() {
        assert_eq!(classify("/v2/app/blobs/sha256:"), None);
    }

    #[test]
    fn test_too_short_path_is_rejected() {
        assert_eq!(classify("/v2/app"), None);
    }

    #[test]
    fn test_valid_tag_edge_cases() {
        assert!(!valid_tag(""));
        assert!(valid_tag("_leading-underscore"));
        assert!(!valid_tag(&"x".repeat(129)));
        assert!(valid_tag(&"x".repeat(128)));
    }
}
