use std::sync::{Arc, Mutex};

use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_ha::{AuthorityEpoch, OperationKind};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{SAMPLE_DENOMINATOR, SAMPLE_NUMERATOR, open_ingress_trace, root_traceparent, samples, trace_entropy};
use crate::state::AppState;

fn topology(local: Option<&str>) -> TopologyConfig {
    TopologyConfig {
        mode: TopologyMode::Dc,
        group: Some("group".to_owned()),
        members: vec![TopologyMember {
            node: "writer".to_owned(),
            dc: "east-1".to_owned(),
            address: "writer:8080".to_owned(),
            role: NodeRole::Writer,
        }],
        local_node: local.map(ToOwned::to_owned),
    }
}

fn state_with(topology: TopologyConfig) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    state.set_availability_topology(topology);
    (dir, state)
}

/// The first `(epoch, kind)` whose ingress trace under `source` samples as `want`, so a test drives the
/// wrapper's sampled and unsampled arms with a concrete identity rather than a hard-coded guess.
fn identity_that_samples(source: &str, want: bool) -> (u64, OperationKind) {
    (0..1_000)
        .map(|epoch| (epoch, OperationKind::Publish))
        .find(|(epoch, kind)| open_ingress_trace(source, AuthorityEpoch(*epoch), 0, *kind).sampled == want)
        .expect("a small identity sweep covers both sampled outcomes")
}

#[test]
fn test_samples_splits_on_the_head_ratio() {
    assert!(samples(0), "a zero remainder falls inside the sampled slice");
    assert!(
        !samples(SAMPLE_NUMERATOR),
        "the first identity past the slice is not sampled"
    );
    assert!(!samples(SAMPLE_DENOMINATOR - 1), "the slice is a strict lower band");
}

#[test]
fn test_root_traceparent_is_a_well_formed_sampled_identifier() {
    let parent = root_traceparent(0, true);
    let fields = parent.split('-').collect::<Vec<_>>();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0], "00");
    assert_eq!(fields[1].len(), 32);
    assert_eq!(fields[2].len(), 16);
    assert_eq!(fields[3], "01");
    assert!(
        fields[1].bytes().any(|byte| byte != b'0'),
        "the trace-id is never all zero"
    );
    assert!(
        fields[2].bytes().any(|byte| byte != b'0'),
        "the span-id is never all zero"
    );
}

#[test]
fn test_root_traceparent_clears_the_flag_when_unsampled() {
    assert!(root_traceparent(0, false).ends_with("-00"));
}

#[test]
fn test_trace_entropy_is_stable_and_identity_scoped() {
    let one = trace_entropy("writer", AuthorityEpoch(3), 7, OperationKind::Publish);
    assert_eq!(
        one,
        trace_entropy("writer", AuthorityEpoch(3), 7, OperationKind::Publish),
        "the same operation folds to the same trace so a replay joins its trace",
    );
    for other in [
        trace_entropy("other", AuthorityEpoch(3), 7, OperationKind::Publish),
        trace_entropy("writer", AuthorityEpoch(4), 7, OperationKind::Publish),
        trace_entropy("writer", AuthorityEpoch(3), 8, OperationKind::Publish),
        trace_entropy("writer", AuthorityEpoch(3), 7, OperationKind::Delete),
    ] {
        assert_ne!(one, other, "a distinct operation opens a distinct trace");
    }
}

#[test]
fn test_open_ingress_trace_names_the_operation_and_opens_its_root() {
    let telemetry = open_ingress_trace("writer", AuthorityEpoch(3), 7, OperationKind::Publish);
    assert_eq!(telemetry.source, "writer");
    assert_eq!(telemetry.epoch, 3);
    assert_eq!(telemetry.serial, 7);
    assert_eq!(telemetry.kind, "publish");
    let parent = telemetry.traceparent.as_deref().unwrap();
    assert_eq!(
        telemetry.sampled,
        parent.ends_with("-01"),
        "the recorded verdict is the traceparent's own flag, not a second copy",
    );
}

/// A `tracing` writer that appends every formatted event to a shared buffer so a test reads back what
/// the wrapper recorded.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn captured(body: impl FnOnce()) -> String {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    std::io::Write::flush(&mut capture.clone()).unwrap();
    let recorded = capture.0.lock().unwrap().clone();
    String::from_utf8(recorded).unwrap()
}

#[test]
fn test_record_operation_trace_records_a_sampled_publish() {
    let (_dir, state) = state_with(topology(Some("writer")));
    let (epoch, kind) = identity_that_samples("writer", true);
    let recorded = captured(|| state.record_operation_trace(kind, epoch));
    assert!(
        recorded.contains("availability operation"),
        "a sampled publish records the event: {recorded}",
    );
    assert!(
        recorded.contains("writer"),
        "the source identity is present: {recorded}"
    );
}

#[test]
fn test_record_operation_trace_stays_silent_for_an_unsampled_publish() {
    let (_dir, state) = state_with(topology(Some("writer")));
    let (epoch, kind) = identity_that_samples("writer", false);
    let recorded = captured(|| state.record_operation_trace(kind, epoch));
    assert!(recorded.is_empty(), "an unsampled publish records nothing: {recorded}");
}

#[test]
fn test_record_operation_trace_falls_back_to_the_standalone_source() {
    let (_dir, state) = state_with(topology(None));
    let sampled = (0..1_000)
        .find(|epoch| open_ingress_trace("standalone", AuthorityEpoch(*epoch), 0, OperationKind::Publish).sampled)
        .expect("a rosterless node still opens a sampled trace for some epoch");
    let recorded = captured(|| state.record_operation_trace(OperationKind::Publish, sampled));
    assert!(
        recorded.contains("standalone"),
        "a rosterless node authors under the standalone source: {recorded}",
    );
}

#[test]
fn test_record_operation_trace_keeps_the_change_payload_out() {
    let (_dir, state) = state_with(topology(Some("writer")));
    let (epoch, kind) = identity_that_samples("writer", true);
    let recorded = captured(|| state.record_operation_trace(kind, epoch));
    for absent in ["credential", "password", "secret", "/var/lib", "sha256:"] {
        assert!(
            !recorded.contains(absent),
            "a payload value leaked into the event: {absent}"
        );
    }
}

#[test]
fn test_record_operation_trace_names_the_committed_serial() {
    let (_dir, state) = state_with(topology(Some("writer")));
    for _ in 0..3 {
        state.meta.next_serial().unwrap();
    }
    let head = state.meta.current_serial().unwrap();
    let sampled = (0..1_000)
        .find(|epoch| open_ingress_trace("writer", AuthorityEpoch(*epoch), head, OperationKind::Publish).sampled)
        .expect("some epoch samples the committed head");
    let recorded = captured(|| state.record_operation_trace(OperationKind::Publish, sampled));
    assert_eq!(head, 3);
    assert!(
        recorded.contains("operation.serial=3"),
        "the event names the committed journal head as the operation serial: {recorded}",
    );
}
