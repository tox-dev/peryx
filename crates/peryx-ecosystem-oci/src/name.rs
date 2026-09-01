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
    /// `PUT /v2/<name>/manifests/<reference>/restore`.
    ManifestRestore { name: String, reference: Reference },
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

pub struct ImageReference {
    pub repository: String,
    pub reference: Reference,
}

/// The scheme keeps OCI repository authorities in a distinct keyspace.
const AUTHORITY_SCHEME: &str = "oci:";

/// The canonical authority key of an OCI repository: its repository path under [`AUTHORITY_SCHEME`].
///
/// The path is preserved verbatim, only prefixed, so distinct repository paths keep distinct keys and
/// two repositories never share a home. The `repository` passed here is the index-route-stripped
/// repository name - the same string the manifest write path homes on its first publish.
#[must_use]
pub fn authority_key(repository: &str) -> String {
    format!("{AUTHORITY_SCHEME}{repository}")
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
    let [.., _, _] = segments.as_slice() else {
        return None;
    };
    let len = segments.len();
    if len >= 3 && segments[len - 3] == "manifests" && segments[len - 1] == "restore" {
        return Some(OciRoute::ManifestRestore {
            name: join_name(&segments[..len - 3])?,
            reference: parse_reference(segments[len - 2])?,
        });
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
        // The referrers digest is validated in the handler, not here: a malformed one must draw a
        // `400 DIGEST_INVALID` per the spec, which a route that returns `None` (a `404`) cannot.
        "referrers" => Some(OciRoute::Referrers {
            name: join_name(&segments[..len - 2])?,
            digest: tail.to_owned(),
        }),
        _ => None,
    }
}

/// Join validated name components back into the repository name, rejecting an empty name.
fn join_name(components: &[&str]) -> Option<String> {
    let name = components.join("/");
    valid_repository(&name).then_some(name)
}

fn valid_repository(repository: &str) -> bool {
    !repository.is_empty() && repository.len() <= 255 && repository.split('/').all(valid_name_component)
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

/// Keep colon-bearing values as digest candidates so the handler can report `DIGEST_INVALID`.
pub fn parse_reference(reference: &str) -> Option<Reference> {
    if reference.contains(':') {
        Some(Reference::Digest(reference.to_owned()))
    } else if valid_tag(reference) {
        Some(Reference::Tag(reference.to_owned()))
    } else {
        None
    }
}

pub fn valid_manifest_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

pub fn parse_image_reference(raw: &str) -> Option<ImageReference> {
    let (repository, reference) = if let Some((repository, digest)) = raw.split_once('@') {
        if !valid_content_digest(digest) {
            return None;
        }
        (repository, Reference::Digest(digest.to_owned()))
    } else {
        let component_start = raw.rfind('/').map_or(0, |index| index + 1);
        if let Some(colon) = raw[component_start..].rfind(':') {
            let split = component_start + colon;
            let tag = &raw[split + 1..];
            if !valid_tag(tag) {
                return None;
            }
            (&raw[..split], Reference::Tag(tag.to_owned()))
        } else {
            (raw, Reference::Tag("latest".to_owned()))
        }
    };
    let has_authority = repository
        .split_once('/')
        .is_some_and(|(first, _)| first == "localhost" || first.contains('.') || first.contains(':'));
    (valid_repository(repository) && !has_authority).then(|| ImageReference {
        repository: repository.to_owned(),
        reference,
    })
}

fn valid_tag(tag: &str) -> bool {
    let mut bytes = tag.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    tag.len() <= 128
        && (first.is_ascii_alphanumeric() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

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

/// Whether `digest` is a syntactically valid content digest per the image-spec grammar, enforcing the
/// fixed lowercase-hex length of the registered `sha256`/`sha512` algorithms. The referrers API must
/// answer `400 DIGEST_INVALID` for a malformed `<digest>`, so it validates here rather than routing a
/// bad digest into the store as an empty lookup that would answer `200`.
#[must_use]
pub fn valid_content_digest(digest: &str) -> bool {
    let Some((algorithm, encoded)) = digest.split_once(':') else {
        return false;
    };
    let component = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    };
    let algorithm_ok = algorithm.split(['+', '.', '_', '-']).all(component);
    let encoded_ok = !encoded.is_empty()
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'=' | b'_' | b'-'));
    if !algorithm_ok || !encoded_ok {
        return false;
    }
    // A registered algorithm has a fixed lowercase-hex encoding, so an off-length or non-hex one is
    // malformed; an unregistered algorithm keeps only the general grammar peryx cannot second-guess.
    let lower_hex = |len: usize| {
        encoded.len() == len
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    };
    match algorithm {
        "sha256" => lower_hex(64),
        "sha512" => lower_hex(128),
        _ => true,
    }
}

/// The OCI referrers tag-schema name for a subject `digest`: the algorithm truncated to 32 characters,
/// a `-`, then the encoded portion truncated to 64, with every character a tag disallows replaced by
/// `-`. A registry predating the referrers API publishes a subject's referrers as an image index under
/// this tag, so a pull-through proxy falls back to it when the referrers API answers `404`.
#[must_use]
pub fn referrers_tag(digest: &str) -> String {
    let (algorithm, encoded) = digest.split_once(':').unwrap_or((digest, ""));
    format!("{}-{}", tag_component(algorithm, 32), tag_component(encoded, 64))
}

fn tag_component(part: &str, limit: usize) -> String {
    part.chars()
        .take(limit)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/name/tests.rs"]
mod tests;
