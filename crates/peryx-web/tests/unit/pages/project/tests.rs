use leptos::prelude::*;
use rstest::rstest;

use peryx_core::{
    UiArtifactSource, UiAttestation, UiByteAvailability, UiFile, UiProvenance, UiProvenanceSource, UiSubjectMatch,
};

use super::{UiProjectStatus, file_row, placement_badges, project_status_badge, provenance_panel};

fn file(filename: &str) -> UiFile {
    UiFile {
        filename: filename.to_owned(),
        release: Some("1.0".to_owned()),
        url: format!("/alpha/files/aa/{filename}"),
        sha256: "aa".repeat(32),
        size: None,
        upload_time: None,
        yanked: false,
        yanked_reason: None,
        has_metadata: false,
        upstream: None,
        provenance: None,
        provenance_detail: None,
        source: UiArtifactSource::Hosted,
        availability: UiByteAvailability::Local,
        browsable: true,
    }
}

fn hosted_provenance(predicate_type: Option<&str>, subject: UiSubjectMatch) -> UiProvenance {
    UiProvenance {
        source: UiProvenanceSource::Hosted,
        attestations: vec![UiAttestation {
            predicate_type: predicate_type.map(str::to_owned),
            subject,
        }],
        malformed: false,
    }
}

#[rstest]
#[case::archived("archived")]
#[case::quarantined("quarantined")]
#[case::deprecated("deprecated")]
fn test_project_status_badge_renders_each_marker(#[case] marker: &str) {
    let status = UiProjectStatus {
        marker: marker.to_owned(),
        reason: None,
    };
    let html = project_status_badge(status).to_html();
    assert!(
        html.contains(&format!(r#"<span class="badge status-{marker}">{marker}</span>"#)),
        "{html}"
    );
    assert!(!html.contains("status-reason"), "{html}");
}

#[test]
fn test_project_status_badge_escapes_a_package_supplied_reason() {
    let status = UiProjectStatus {
        marker: "quarantined".to_owned(),
        reason: Some(r"<script>pwn</script>".to_owned()),
    };
    let html = project_status_badge(status).to_html();
    assert!(html.contains(r#"class="status-reason""#), "{html}");
    assert!(html.contains("&lt;script&gt;pwn&lt;/script&gt;"), "{html}");
    assert!(!html.contains("<script>"), "{html}");
}

#[test]
fn test_file_row_names_the_routed_upstream() {
    let mut file = file("flask-1.0.bin");
    file.upstream = Some("corporate".to_owned());
    file.source = UiArtifactSource::Proxy;
    file.availability = UiByteAvailability::RemoteOnly;
    let html = file_row("alpha", "flask", &file).to_html();
    assert!(html.contains(r#"title="Upstream source""#), "{html}");
    assert!(html.contains(">corporate</span>"), "{html}");
}

#[rstest]
#[case::hosted(
    UiArtifactSource::Hosted,
    UiByteAvailability::Local,
    ">hosted</span>",
    ">local</span>"
)]
#[case::cached(UiArtifactSource::Proxy, UiByteAvailability::Local, ">proxy</span>", ">local</span>")]
#[case::remote_only(
    UiArtifactSource::Proxy,
    UiByteAvailability::RemoteOnly,
    ">proxy</span>",
    ">remote-only</span>"
)]
#[case::unavailable(
    UiArtifactSource::Hosted,
    UiByteAvailability::Unavailable,
    ">hosted</span>",
    ">unavailable</span>"
)]
#[case::generated(
    UiArtifactSource::Generated,
    UiByteAvailability::Local,
    ">generated</span>",
    ">local</span>"
)]
fn test_placement_badges_render_distinct_labelled_text(
    #[case] source: UiArtifactSource,
    #[case] availability: UiByteAvailability,
    #[case] source_text: &str,
    #[case] availability_text: &str,
) {
    let html = placement_badges(source, availability).to_html();
    assert!(html.contains(source_text), "{html}");
    assert!(html.contains(availability_text), "{html}");
    // Each chip names its dimension for a screen reader and carries no colour-only meaning.
    assert!(html.contains(r#"aria-label="source:"#), "{html}");
    assert!(html.contains(r#"aria-label="availability:"#), "{html}");
}

#[test]
fn test_file_row_keeps_both_yank_reason_and_availability() {
    let mut file = file("flask-1.0.bin");
    file.source = UiArtifactSource::Proxy;
    file.availability = UiByteAvailability::RemoteOnly;
    file.yanked = true;
    file.yanked_reason = Some("broken build".to_owned());
    let html = file_row("alpha", "flask", &file).to_html();
    assert!(html.contains("yanked-badge"), "{html}");
    assert!(html.contains(">broken build</span>"), "{html}");
    assert!(html.contains(">remote-only</span>"), "{html}");
    assert!(html.contains(r#"class="badge placement-source src-proxy""#), "{html}");
}

#[test]
fn test_provenance_panel_summarizes_a_hosted_document() {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("/alpha/files/aa/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(UiProvenance {
        source: UiProvenanceSource::Hosted,
        attestations: vec![
            UiAttestation {
                predicate_type: Some("https://docs.alpha.org/attestations/publish/v1".to_owned()),
                subject: UiSubjectMatch::Matched,
            },
            UiAttestation {
                predicate_type: Some("https://slsa.dev/provenance/v1".to_owned()),
                subject: UiSubjectMatch::Matched,
            },
        ],
        malformed: false,
    });
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(html.contains("<details class=\"provenance-panel\">"), "{html}");
    assert!(html.contains(r#"aria-label="provenance source: hosted""#), "{html}");
    assert!(html.contains(">binding verified</span>"), "{html}");
    assert!(html.contains(">subject matched</span>"), "{html}");
    assert!(html.contains(">2 attestations</p>"), "{html}");
    assert!(
        html.contains(">https://docs.alpha.org/attestations/publish/v1</code>"),
        "{html}"
    );
    assert!(html.contains(">https://slsa.dev/provenance/v1</code>"), "{html}");
}

#[test]
fn test_provenance_panel_links_a_hosted_document_without_the_external_relationship() {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("/alpha/files/aa/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(hosted_provenance(None, UiSubjectMatch::Matched));
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(
        html.contains(
            r#"<a href="/alpha/files/aa/flask-1.0.bin.provenance" class="provenance-doc">provenance document</a>"#
        ),
        "{html}"
    );
    assert!(!html.contains("nofollow"), "{html}");
    assert!(html.contains(">no predicate type</span>"), "{html}");
}

#[test]
fn test_provenance_panel_reports_a_mirrored_claim_as_unverified() {
    let mut file = file("flask-1.0.bin");
    file.source = UiArtifactSource::Proxy;
    file.provenance = Some("https://alpha.example/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(UiProvenance {
        source: UiProvenanceSource::Mirrored,
        attestations: Vec::new(),
        malformed: false,
    });
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(html.contains(r#"aria-label="provenance source: mirrored""#), "{html}");
    assert!(html.contains(">unverified claim</span>"), "{html}");
    assert!(html.contains("neither fetched nor verified"), "{html}");
    assert!(
        html.contains(r#"rel="external nofollow noopener noreferrer" class="provenance-doc""#),
        "{html}"
    );
    assert!(!html.contains("<li class=\"attestation\">"), "{html}");
}

#[test]
fn test_provenance_panel_reports_an_unreadable_document() {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("/alpha/files/aa/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(UiProvenance {
        source: UiProvenanceSource::Hosted,
        attestations: Vec::new(),
        malformed: true,
    });
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(html.contains(">unreadable</span>"), "{html}");
    assert!(html.contains("could not read the stored provenance document"), "{html}");
}

#[rstest]
#[case::mismatched(UiSubjectMatch::Mismatched, ">subject mismatch</span>")]
#[case::unknown(UiSubjectMatch::Unknown, ">subject unknown</span>")]
fn test_provenance_panel_labels_each_subject_binding(#[case] subject: UiSubjectMatch, #[case] expected: &str) {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("/alpha/files/aa/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(hosted_provenance(Some("https://slsa.dev/provenance/v1"), subject));
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(html.contains(expected), "{html}");
    assert!(html.contains(r#"aria-label="subject binding:"#), "{html}");
}

#[test]
fn test_provenance_panel_escapes_an_untrusted_predicate_type() {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("/alpha/files/aa/flask-1.0.bin.provenance".to_owned());
    file.provenance_detail = Some(hosted_provenance(
        Some(r"<script>pwn()</script>"),
        UiSubjectMatch::Matched,
    ));
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;pwn()&lt;/script&gt;"), "{html}");
}

#[test]
fn test_provenance_panel_drops_an_unsafe_document_url_but_keeps_the_panel() {
    let mut file = file("flask-1.0.bin");
    file.provenance = Some("javascript:alert(1)".to_owned());
    file.provenance_detail = Some(hosted_provenance(None, UiSubjectMatch::Matched));
    let html = provenance_panel(&file).unwrap().to_html();
    assert!(html.contains("provenance-panel"), "{html}");
    assert!(!html.contains("provenance-doc"), "{html}");
    assert!(!html.contains("javascript:"), "{html}");
}

#[test]
fn test_file_row_renders_no_provenance_panel_without_a_detail() {
    let file = file("flask-1.0.bin");
    let html = file_row("alpha", "flask", &file).to_html();
    assert!(!html.contains("provenance-panel"), "{html}");
    assert!(html.contains("flask-1.0.bin"), "{html}");
}
