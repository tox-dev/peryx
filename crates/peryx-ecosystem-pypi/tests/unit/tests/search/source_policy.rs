use super::support::*;
use crate::PypiIndexer;
use crate::policy::FallbackMode;
use crate::tests::http::policy as index_policy;
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_search::{IndexerCtx, SearchDocument, SearchDocumentProvider as _};

const VIRTUAL_ROUTE: &str = "root/pypi";
const HOSTED_FILE: &str = "requests-1.0-py3-none-any.whl";
const CACHED_FILE: &str = "requests-2.0-py3-none-any.whl";

/// A cached mirror, a hosted index, and a virtual repository over both, so a project can be held by
/// either member or by both at once.
fn virtual_state(overlay: Policy) -> (tempfile::TempDir, Arc<AppState>) {
    state_with(overlay, false)
}

/// The same three indexes, except the virtual repository reaches the mirror through an intermediate
/// virtual member listed ahead of the hosted one.
fn nested_virtual_state(overlay: Policy) -> (tempfile::TempDir, Arc<AppState>) {
    state_with(overlay, true)
}

fn state_with(overlay: Policy, nested: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::new("https://example.test/simple/").unwrap(),
                offline: false,
            },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        },
        Index {
            name: "root-pypi".to_owned(),
            route: VIRTUAL_ROUTE.to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: if nested { vec![3, 1] } else { vec![0, 1] },
                write_target: Some(1),
            },
            policy: overlay,
            acl: peryx_identity::IndexAcl::default(),
        },
    ];
    if nested {
        indexes.push(Index {
            name: "inner".to_owned(),
            route: "inner".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![0],
                write_target: None,
            },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        });
    }
    (dir, crate::tests::wired(AppState::new(meta, blobs, 60, indexes)))
}

fn put_cached_release(state: &ServingState, normalized: &str, filename: &str, sha256: &Digest) {
    put_cached_package(
        state,
        &format!("pypi/{normalized}"),
        "pypi",
        normalized,
        &ProjectDetail {
            meta: Meta::default(),
            name: normalized.to_owned(),
            versions: vec!["2.0".to_owned()],
            files: vec![file_with_hash(filename, sha256.as_str(), None)],
        },
    );
}

fn put_metadata_summary(state: &ServingState, artifact_sha256: &str, summary: &str) {
    let text = format!("Metadata-Version: 2.1\nName: requests\nVersion: 2.0\nSummary: {summary}\n");
    let digest = state.blobs.blocking().put_bytes(text.as_bytes()).unwrap();
    state.meta.put_metadata(artifact_sha256, digest.as_str()).unwrap();
}

fn documents(state: &ServingState) -> Vec<SearchDocument> {
    PypiIndexer
        .documents(&IndexerCtx {
            indexes: &state.indexes,
            meta: &state.meta,
            blobs: &state.blobs,
        })
        .unwrap()
}

fn virtual_document(state: &ServingState, resource_key: &str) -> Option<SearchDocument> {
    documents(state)
        .into_iter()
        .find(|document| document.route == VIRTUAL_ROUTE && document.resource_key == resource_key)
}

/// Which of the two members' filenames the indexed text advertises, in a fixed order so a test can
/// pin both what the document carries and what it withholds in one assertion.
fn advertised_files(document: &SearchDocument) -> Vec<&'static str> {
    [HOSTED_FILE, CACHED_FILE]
        .into_iter()
        .filter(|filename| document.text.contains(filename))
        .collect()
}

/// A hosted `requests-1.0` and a cached `requests-2.0`, the shape every fallback mode decides
/// between. The cached release carries the newer metadata, so a document that merged it in keeps its
/// summary.
fn put_both_members(state: &ServingState) -> Digest {
    put_uploaded_package_with_metadata(
        state,
        "requests",
        "Metadata-Version: 2.1\nName: requests\nVersion: 1.0\nSummary: Internal build\n",
        None,
    );
    let cached_digest = Digest::of(b"requests cached release");
    put_cached_release(state, "requests", CACHED_FILE, &cached_digest);
    put_metadata_summary(state, cached_digest.as_str(), "Upstream release");
    cached_digest
}

fn protected_overlay() -> Policy {
    index_policy(|_, pypi| pypi.protected_names = vec!["mycorp-*".to_owned()])
}

#[rstest::rstest]
#[case::protected_name(protected_overlay())]
#[case::no_fallback(index_policy(|_, pypi| pypi.fallback_mode = FallbackMode::NoFallback))]
fn test_search_keeps_an_excluded_cached_project_off_the_virtual_route(#[case] overlay: Policy) {
    let (_dir, state) = virtual_state(overlay);
    put_cached_release(
        &state.serving,
        "mycorp-tool",
        "mycorp_tool-2.0-py3-none-any.whl",
        &Digest::of(b"mycorp tool"),
    );

    let routes: Vec<String> = documents(&state.serving)
        .into_iter()
        .filter(|document| document.resource_key == "mycorp-tool")
        .map(|document| document.route)
        .collect();

    assert_eq!(
        routes,
        ["pypi"],
        "a member the virtual route may not reach still owns the project on its own route"
    );
}

#[test]
fn test_search_fallback_merges_both_members_for_an_unprotected_name() {
    let (_dir, state) = virtual_state(protected_overlay());
    put_both_members(&state.serving);

    let document = virtual_document(&state.serving, "requests").expect("fallback indexes the merged project");

    assert_eq!(advertised_files(&document), [HOSTED_FILE, CACHED_FILE]);
}

#[rstest::rstest]
#[case::private_first(FallbackMode::PrivateFirst)]
#[case::no_fallback(FallbackMode::NoFallback)]
fn test_search_virtual_document_carries_hosted_files_alone(#[case] mode: FallbackMode) {
    let (_dir, state) = virtual_state(index_policy(|_, pypi| pypi.fallback_mode = mode));
    put_both_members(&state.serving);

    let document = virtual_document(&state.serving, "requests").expect("the hosted member still holds the project");

    assert_eq!(advertised_files(&document), [HOSTED_FILE]);
}

#[test]
fn test_search_private_first_summary_comes_from_the_hosted_release() {
    let (_dir, state) = virtual_state(index_policy(|_, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst));
    put_both_members(&state.serving);

    let document = virtual_document(&state.serving, "requests").expect("the hosted member still holds the project");

    assert_eq!(document.summary.as_deref(), Some("Internal build"));
}

#[test]
fn test_search_private_first_availability_ignores_a_shadowed_cached_release() {
    let (_dir, state) = virtual_state(index_policy(|_, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst));
    let cached_digest = put_both_members(&state.serving);
    state
        .serving
        .meta
        .put_artifact_placement(
            Digest::of(HOSTED_FILE.as_bytes()).as_str(),
            &ArtifactPlacement::record(ArtifactSource::Hosted, false),
        )
        .unwrap();
    state
        .serving
        .meta
        .put_artifact_placement(
            cached_digest.as_str(),
            &ArtifactPlacement::record(ArtifactSource::Proxy, true),
        )
        .unwrap();

    let document = virtual_document(&state.serving, "requests").expect("the hosted member still holds the project");

    assert!(
        !document.available_locally,
        "an evicted hosted release is not local just because the shadowed upstream copy is"
    );
}

#[test]
fn test_search_private_first_withholds_a_cache_below_a_nested_member() {
    let (_dir, state) = nested_virtual_state(index_policy(|_, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst));
    put_both_members(&state.serving);

    let document = virtual_document(&state.serving, "requests").expect("the hosted member still holds the project");

    assert_eq!(advertised_files(&document), [HOSTED_FILE]);
}

#[test]
fn test_search_no_fallback_keeps_the_hosted_leaf_of_a_nested_member() {
    let (_dir, state) = nested_virtual_state(index_policy(|_, pypi| pypi.fallback_mode = FallbackMode::NoFallback));
    put_both_members(&state.serving);

    let document = virtual_document(&state.serving, "requests").expect("the hosted member still holds the project");

    assert_eq!(advertised_files(&document), [HOSTED_FILE]);
}
