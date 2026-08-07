//! Neutral view models the web UI renders.
//!
//! The UI is ecosystem-agnostic: it lays out a page but knows nothing about artifact formats or
//! protocol headers. Each ecosystem crate turns its own format into these neutral shapes, and the web
//! crate renders them. The models are pure serde with no rendering or I/O, so they cross the
//! server/browser boundary and pull no UI toolkit into an ecosystem crate.
//!
//! The metadata panel is a list of [`UiBlock`]s - a small vocabulary of presentation primitives keyed
//! by *shape* (a key/value, a chip set, a link list), never by ecosystem. An ecosystem composes those
//! primitives to describe its own format, so a new ecosystem adds no field here and no branch in the
//! web crate. [`UiBlock`] is `#[non_exhaustive]`: a genuinely new primitive is one additive variant
//! plus one match arm in the renderer, and the renderer's catch-all keeps an unknown block from
//! silently rendering nothing. This is the server-driven-UI shape Airbnb's section union and Sanity's
//! Portable Text use, sized down to what a package page needs.

use serde::{Deserialize, Serialize};

/// A project's descriptive metadata, ready for a page to render without knowing the ecosystem it came
/// from. An ecosystem driver fills what its format has; the rest stay empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMeta {
    /// The newest version, when the format names one distinctly from the file list.
    pub version: Option<String>,
    /// A one-line summary shown under the title.
    pub summary: Option<String>,
    /// The long description rendered to sanitized HTML, produced on the server so the browser shows it
    /// without running the renderer. Rendering reStructuredText in the browser can abort the
    /// WebAssembly module on constructs the renderer never implemented, and that abort cannot be
    /// caught there, so the render happens once, in the ecosystem driver, where a panic is recoverable.
    pub description: Option<RenderedDescription>,
    /// The metadata-panel blocks, in display order. Each is a neutral presentation primitive an
    /// ecosystem filled; the page renders the vocabulary without knowing which format produced it.
    pub blocks: Vec<UiBlock>,
}

/// A description rendered to safe HTML, with the message to show when rendering fell back to plain
/// text. The ecosystem driver produces it so the renderer runs server-side, in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RenderedDescription {
    pub html: String,
    pub notice: Option<String>,
}

/// One block of a metadata panel: a presentation primitive keyed by shape, not by ecosystem.
///
/// `#[non_exhaustive]`, so a new primitive is additive - a variant here plus a match arm in the web
/// renderer, whose catch-all keeps an unrecognized block from rendering as a blank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum UiBlock {
    /// A single labelled value (requires-python, license, author).
    KeyValue { label: String, value: String },
    /// A labelled set of short values shown as chips (keywords, dependencies).
    Chips { label: String, values: Vec<String> },
    /// A labelled list of links (`(text, url)` pairs, such as project URLs).
    Links {
        label: String,
        links: Vec<(String, String)>,
    },
    /// A labelled set of named groups, each a list of values (trove classifiers by category).
    Groups {
        label: String,
        groups: Vec<(String, Vec<String>)>,
    },
}

/// A project's publish status, when its index flags the project as archived, quarantined, or
/// deprecated.
///
/// The ecosystem driver fills it only for a flagged project, so an active or unmarked one carries
/// `None` and the page shows no badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiProjectStatus {
    /// The status marker, lowercased (`archived`, `quarantined`, `deprecated`). It names the badge and
    /// keys its style, the way the ecosystem and kind chips do, so a marker the page has no style for
    /// still renders as a plain badge.
    pub marker: String,
    /// The publisher's explanation for the status. Package-supplied text, so the page escapes it.
    pub reason: Option<String>,
}

/// A project page: the files of one project on one index, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiProject {
    pub name: String,
    /// The publish status to flag beside the heading, or `None` for a project served as usual. Boxed so
    /// this rare field does not enlarge the shared `UiProjectView` for every project.
    pub status: Option<Box<UiProjectStatus>>,
    pub versions: Vec<UiRelease>,
    pub files: Vec<UiFile>,
    /// A client command containing `<origin>`, replaced by the browser's current HTTP origin.
    pub client_command: Option<String>,
}

/// One release of a project: a version and the yank state its files give it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRelease {
    pub version: String,
    /// Whether the publisher yanked the whole release. A release keeping one usable file is active.
    pub yanked: bool,
    /// The reasons the publisher gave, distinct and in the order the index lists them. Empty when the
    /// release is active or the publisher gave no reason.
    pub yanked_reasons: Vec<String>,
}

/// Where a file's artifact bytes came from, mirroring the storage placement source.
///
/// Intrinsic: caching or evicting the bytes never changes it, only a different artifact taking the
/// digest's place. The package page pairs it with a [`UiByteAvailability`] so a reader distinguishes
/// an upload from a proxied mirror even when both are served from local storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiArtifactSource {
    /// Published into this instance. No upstream can resupply the bytes once they are lost.
    Hosted,
    /// Cached from an upstream index. A local miss can be answered by re-fetching from upstream.
    Proxy,
    /// Produced by this instance, such as a rendered index page or a derived metadata sibling.
    Generated,
}

impl UiArtifactSource {
    /// The lowercase word a badge shows and its stable `snake_case` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Proxy => "proxy",
            Self::Generated => "generated",
        }
    }
}

/// Whether this instance can serve a file's bytes right now, mirroring the storage placement
/// projection.
///
/// Orthogonal to [`UiArtifactSource`]: a proxied file can be locally cached or not, and a hosted file
/// is local until its bytes are lost. Neither says anything about yank, policy, trash, or revocation,
/// which the page composes on top rather than folds in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiByteAvailability {
    /// The configured storage holds verified bytes; a read serves them without an upstream fetch.
    Local,
    /// No local bytes, but a known upstream can supply them.
    RemoteOnly,
    /// No local bytes and no upstream to supply them.
    Unavailable,
}

impl UiByteAvailability {
    /// The stable `snake_case` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::RemoteOnly => "remote_only",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The client-facing status of one admitted write, the label an operations-health view shows.
///
/// A finalized write reads [`Published`](Self::Published) and one that gave up [`Failed`](Self::Failed),
/// each terminal. A write still in flight reads [`Pending`](Self::Pending) until its retention deadline
/// passes, after which it reads [`Expired`](Self::Expired) without ever finalizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiOperationStatus {
    /// Admitted and in flight; no terminal result and still within its retention deadline.
    Pending,
    /// Finalized at the home. Terminal.
    Published,
    /// Gave up before finalizing. Terminal.
    Failed,
    /// Never finalized and outlived its retention deadline.
    Expired,
}

impl UiOperationStatus {
    /// The stable `snake_case` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Published => "published",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    /// Derive the client-facing status from a record's durable fields at `now`.
    ///
    /// Matches the write-status resource's rule: a finalized write is [`Published`](Self::Published) and
    /// one that gave up [`Failed`](Self::Failed), each terminal and independent of `now`; a write that
    /// reached neither reads [`Expired`](Self::Expired) once `now` passes its retention deadline, otherwise
    /// [`Pending`](Self::Pending).
    #[must_use]
    pub const fn derive(published: bool, failed: bool, expiry: Option<i64>, now: i64) -> Self {
        if published {
            Self::Published
        } else if failed {
            Self::Failed
        } else if let Some(expiry) = expiry {
            if now >= expiry { Self::Expired } else { Self::Pending }
        } else {
            Self::Pending
        }
    }
}

/// How peryx obtained an artifact's provenance, which bounds what it can say about it.
///
/// `Hosted` provenance was uploaded here, so peryx bound every attestation to this exact
/// distribution (filename and sha256) before publishing and can summarize the stored document.
/// `Mirrored` provenance is a claim an upstream index advertised: peryx relays that the claim exists
/// without fetching or reading the document, so it carries no per-attestation records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiProvenanceSource {
    /// Uploaded into this instance; peryx enforced the subject binding at upload.
    Hosted,
    /// Advertised by an upstream index; peryx relays the claim and never verified or read it.
    Mirrored,
}

impl UiProvenanceSource {
    /// The stable `snake_case` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Mirrored => "mirrored",
        }
    }
}

/// Whether an attestation's in-toto subject binds to the distribution it rides on.
///
/// The binding, not any signature, is what peryx checks: a `Matched` subject names this file's
/// sha256 (and, when it gives one, its filename). `Mismatched` names a different artifact, and
/// `Unknown` covers a statement peryx could not read a subject from. peryx never verifies the
/// Sigstore signature, so a match says the bundle was issued for this file, not that it is genuine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSubjectMatch {
    /// A subject digest equals this file's sha256 and any named filename matches.
    Matched,
    /// A subject digest equals this file's sha256 but names a different filename, or no subject
    /// digest matches at all.
    Mismatched,
    /// The statement carried no readable subject to compare.
    Unknown,
}

impl UiSubjectMatch {
    /// The stable `snake_case` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Mismatched => "mismatched",
            Self::Unknown => "unknown",
        }
    }
}

/// One attestation as the provenance panel shows it: plain, escaped data derived from stored
/// metadata, never from a live signature check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiAttestation {
    /// The in-toto `predicateType` the statement declares, when it names one. Untrusted text the
    /// renderer escapes and bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate_type: Option<String>,
    /// Whether the attestation's subject binds to this distribution.
    pub subject: UiSubjectMatch,
}

/// Artifact provenance as the project page renders it.
///
/// Derived from digest-indexed metadata read from local storage: the panel neither fetches an
/// upstream document nor verifies a signature, so it states what the bundle claims and how peryx
/// obtained it, never that any attestation is trustworthy. `attestations` is filled only for hosted
/// provenance peryx read locally; it stays empty for a mirrored claim and for a hosted document that
/// could not be read (`malformed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiProvenance {
    /// How peryx obtained the provenance, hosted or mirrored.
    pub source: UiProvenanceSource,
    /// The per-attestation records, or empty for a mirrored claim or an unreadable document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<UiAttestation>,
    /// Whether a hosted document was present but could not be summarized.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malformed: bool,
}

/// One downloadable file as the project page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFile {
    pub filename: String,
    /// The declared release this file belongs to, after ecosystem-specific version matching. `None`
    /// keeps a file visible when its filename is malformed, its release is undeclared, or several
    /// declared releases normalize to the same identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: bool,
    pub yanked_reason: Option<String>,
    pub has_metadata: bool,
    pub browsable: bool,
    /// The configured upstream source that advertised this artifact, when routing is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The provenance URL this file advertises, exactly as the index published it. `None` when
    /// the file names no provenance, spells it as an explicit `null`, or gives an empty URL.
    ///
    /// This carries the advertised location only: peryx neither fetches the document nor verifies the
    /// attestation it wraps, so its presence attests that the publisher claimed provenance, not that
    /// any signature was checked. The renderer applies the page's URL-scheme and external-link policy
    /// before it becomes a link, so an unsafe value is dropped without hiding the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// The rendered provenance panel for this file, when it advertises provenance. The
    /// driver fills it from digest-indexed metadata read locally, so it summarizes hosted
    /// attestations and flags a mirrored claim without fetching or verifying anything. `None` when
    /// the file advertises no provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_detail: Option<UiProvenance>,
    /// Where the file's bytes came from: an upload, a proxied mirror, or a generated sibling.
    pub source: UiArtifactSource,
    /// Whether this instance can serve the file's bytes now, independent of its source.
    pub availability: UiByteAvailability,
}

/// What a project-level browse request renders as.
///
/// Chosen by the ecosystem driver so the web crate dispatches without naming a format. A file-based
/// ecosystem returns its file listing and descriptive metadata; a registry returns its list of
/// references, each resolving to a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum UiProjectView {
    /// A file listing with descriptive metadata.
    Files { project: UiProject, meta: UiMeta },
    /// A list of named references, each resolving to a manifest.
    References { names: Vec<String> },
}

/// One referenced content item in a manifest view.
///
/// A primary blob, a listed blob, or a per-platform child of an index: its digest, size, and content
/// type, an optional platform tag, and whether the web browser can list its contents. The driver
/// decides `browsable`, so shared code never inspects a content type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiArtifactRef {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    /// `os/architecture` when this entry is a per-platform child of an index.
    pub platform: Option<String>,
    /// Whether the layer browser can list this entry's contents.
    pub browsable: bool,
}

/// Browser-facing fields for an ecosystem-owned multipart upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiUploadSpec {
    pub endpoint: String,
    pub form_field: String,
    pub authorization_username: Option<String>,
    pub token_label: String,
    pub file_label: String,
    pub accept: String,
    pub help: String,
    pub allowed_suffixes: Vec<String>,
}

/// A manifest view, neutral so the web crate renders it without parsing any wire format.
///
/// A content type and total size, an optional primary item (a config) and a list of referenced items
/// (layers), or a flag that it is an index of per-platform child manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiManifest {
    pub media_type: String,
    pub is_index: bool,
    pub config: Option<UiArtifactRef>,
    /// Listed items: the layers of a manifest, or the per-platform children of an index.
    pub entries: Vec<UiArtifactRef>,
    pub total_size: u64,
    /// A client command containing `<host>`, replaced by the browser's current host.
    pub client_command: Option<String>,
}

/// One member of a nested content item (a distribution archive or an image layer), as a browser lists
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiMember {
    pub path: String,
    pub size: u64,
    pub kind: String,
    pub previewable: bool,
}

/// One rendered chunk of a nested content member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMemberChunk {
    pub text: String,
    pub size: Option<u64>,
    pub offset: u64,
    pub next_offset: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{
        UiArtifactSource, UiAttestation, UiByteAvailability, UiFile, UiOperationStatus, UiProvenance,
        UiProvenanceSource, UiSubjectMatch,
    };

    #[test]
    fn test_provenance_source_and_subject_match_round_trip_snake_case() {
        for (source, wire) in [
            (UiProvenanceSource::Hosted, "\"hosted\""),
            (UiProvenanceSource::Mirrored, "\"mirrored\""),
        ] {
            assert_eq!(serde_json::to_string(&source).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiProvenanceSource>(wire).unwrap(), source);
            assert_eq!(format!("\"{}\"", source.as_str()), wire);
        }
        for (subject, wire) in [
            (UiSubjectMatch::Matched, "\"matched\""),
            (UiSubjectMatch::Mismatched, "\"mismatched\""),
            (UiSubjectMatch::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&subject).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiSubjectMatch>(wire).unwrap(), subject);
            assert_eq!(format!("\"{}\"", subject.as_str()), wire);
        }
    }

    #[test]
    fn test_provenance_omits_empty_attestations_and_absent_malformed() {
        let provenance = UiProvenance {
            source: UiProvenanceSource::Mirrored,
            attestations: Vec::new(),
            malformed: false,
        };
        let json = serde_json::to_string(&provenance).unwrap();
        assert_eq!(json, r#"{"source":"mirrored"}"#);
        assert_eq!(serde_json::from_str::<UiProvenance>(&json).unwrap(), provenance);
    }

    #[test]
    fn test_provenance_carries_attestations_and_malformed_on_the_wire() {
        let provenance = UiProvenance {
            source: UiProvenanceSource::Hosted,
            attestations: vec![UiAttestation {
                predicate_type: Some("https://docs.alpha.org/attestations/publish/v1".to_owned()),
                subject: UiSubjectMatch::Matched,
            }],
            malformed: true,
        };
        let json = serde_json::to_string(&provenance).unwrap();
        assert!(json.contains(r#""source":"hosted""#), "{json}");
        assert!(json.contains(r#""subject":"matched""#), "{json}");
        assert!(json.contains(r#""malformed":true"#), "{json}");
        assert_eq!(serde_json::from_str::<UiProvenance>(&json).unwrap(), provenance);
    }

    #[test]
    fn test_attestation_omits_an_absent_predicate_type() {
        let attestation = UiAttestation {
            predicate_type: None,
            subject: UiSubjectMatch::Unknown,
        };
        let json = serde_json::to_string(&attestation).unwrap();
        assert!(!json.contains("predicate_type"), "{json}");
        assert_eq!(serde_json::from_str::<UiAttestation>(&json).unwrap(), attestation);
    }

    #[test]
    fn test_source_and_availability_round_trip_snake_case() {
        for (source, wire) in [
            (UiArtifactSource::Hosted, "\"hosted\""),
            (UiArtifactSource::Proxy, "\"proxy\""),
            (UiArtifactSource::Generated, "\"generated\""),
        ] {
            assert_eq!(serde_json::to_string(&source).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiArtifactSource>(wire).unwrap(), source);
            assert_eq!(format!("\"{}\"", source.as_str()), wire);
        }
        for (availability, wire) in [
            (UiByteAvailability::Local, "\"local\""),
            (UiByteAvailability::RemoteOnly, "\"remote_only\""),
            (UiByteAvailability::Unavailable, "\"unavailable\""),
        ] {
            assert_eq!(serde_json::to_string(&availability).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiByteAvailability>(wire).unwrap(), availability);
            assert_eq!(format!("\"{}\"", availability.as_str()), wire);
        }
    }

    #[test]
    fn test_operation_status_round_trips_snake_case() {
        for (status, wire) in [
            (UiOperationStatus::Pending, "\"pending\""),
            (UiOperationStatus::Published, "\"published\""),
            (UiOperationStatus::Failed, "\"failed\""),
            (UiOperationStatus::Expired, "\"expired\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiOperationStatus>(wire).unwrap(), status);
            assert_eq!(format!("\"{}\"", status.as_str()), wire);
        }
    }

    #[test]
    fn test_operation_status_derives_from_the_durable_fields() {
        // Terminal states are independent of the clock; a pending write reads expired only once the clock
        // reaches its retention deadline.
        assert_eq!(
            UiOperationStatus::derive(true, false, Some(10), 5),
            UiOperationStatus::Published
        );
        assert_eq!(
            UiOperationStatus::derive(false, true, None, 5),
            UiOperationStatus::Failed
        );
        assert_eq!(
            UiOperationStatus::derive(false, false, Some(10), 10),
            UiOperationStatus::Expired
        );
        assert_eq!(
            UiOperationStatus::derive(false, false, Some(10), 9),
            UiOperationStatus::Pending
        );
        assert_eq!(
            UiOperationStatus::derive(false, false, None, 9),
            UiOperationStatus::Pending
        );
    }

    #[test]
    fn test_ui_file_carries_source_and_availability_on_the_wire() {
        let file = UiFile {
            filename: "pkg-1.0-py3-none-any.bin".to_owned(),
            release: Some("1.0".to_owned()),
            url: "/alpha/files/aa/pkg-1.0-py3-none-any.bin".to_owned(),
            sha256: "aa".to_owned(),
            size: Some(10),
            upload_time: None,
            yanked: false,
            yanked_reason: None,
            has_metadata: false,
            upstream: Some("mirror".to_owned()),
            provenance: Some("https://alpha.example/files/aa/pkg-1.0-py3-none-any.bin.provenance".to_owned()),
            provenance_detail: Some(UiProvenance {
                source: UiProvenanceSource::Mirrored,
                attestations: Vec::new(),
                malformed: false,
            }),
            source: UiArtifactSource::Proxy,
            availability: UiByteAvailability::RemoteOnly,
            browsable: true,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"source\":\"proxy\""), "{json}");
        assert!(json.contains("\"availability\":\"remote_only\""), "{json}");
        assert!(json.contains("\"browsable\":true"), "{json}");
        assert!(json.contains("\"upstream\":\"mirror\""), "{json}");
        assert!(json.contains("\"release\":\"1.0\""), "{json}");
        assert!(
            json.contains("\"provenance\":\"https://alpha.example/files/aa/pkg-1.0-py3-none-any.bin.provenance\""),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap(), file);
    }

    #[test]
    fn test_ui_file_omits_absent_provenance_from_the_wire() {
        let file = UiFile {
            filename: "pkg-1.0-py3-none-any.bin".to_owned(),
            release: None,
            url: "/alpha/files/aa/pkg-1.0-py3-none-any.bin".to_owned(),
            sha256: "aa".to_owned(),
            size: None,
            upload_time: None,
            yanked: false,
            yanked_reason: None,
            has_metadata: false,
            upstream: None,
            provenance: None,
            provenance_detail: None,
            source: UiArtifactSource::Hosted,
            availability: UiByteAvailability::Unavailable,
            browsable: false,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(!json.contains("provenance"), "{json}");
        assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap().provenance, None);
    }

    #[test]
    fn test_ui_file_defaults_an_omitted_release_to_unassociated() {
        let file: UiFile = serde_json::from_value(serde_json::json!({
            "filename": "notes.txt",
            "url": "/files/notes.txt",
            "sha256": "aa",
            "size": null,
            "upload_time": null,
            "yanked": false,
            "yanked_reason": null,
            "has_metadata": false,
            "browsable": false,
            "source": "proxy",
            "availability": "remote_only",
        }))
        .unwrap();

        assert_eq!(file.release, None);
    }
}
