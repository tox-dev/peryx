use std::sync::{Arc, Mutex};

use super::{DistributedOperationObserver, operation_telemetry, root_traceparent, sampled, samples, trace_entropy};
use crate::{AuthorityEpoch, BlobReference, Change, MetadataMutation, OperationEnvelope, OperationKind, TraceContext};
use peryx_ha::{OperationObservation, OperationObserver as _};

const SAMPLED_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const UNSAMPLED_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

fn secret_change() -> Change {
    Change {
        serial: 7,
        event: b"credential=hunter2".to_vec(),
        metadata: vec![MetadataMutation::Put {
            key: "/var/lib/peryx/private/path".to_owned(),
            value: b"secret-digest-map".to_vec(),
        }],
        blobs: vec![BlobReference {
            sha256: "a".repeat(64),
            size: 1024,
        }],
    }
}

fn traced(traceparent: Option<&str>) -> OperationEnvelope {
    OperationEnvelope {
        trace: traceparent.map(|traceparent| TraceContext {
            traceparent: traceparent.to_owned(),
            tracestate: None,
        }),
        ..OperationEnvelope::current("primary-a", AuthorityEpoch(3), OperationKind::Publish, secret_change())
    }
}

#[test]
fn test_telemetry_carries_the_identity_kind_and_traceparent() {
    let telemetry = operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT)));

    assert_eq!(telemetry.source, "primary-a");
    assert_eq!(telemetry.epoch, 3);
    assert_eq!(telemetry.serial, 7);
    assert_eq!(telemetry.kind, "publish");
    assert_eq!(telemetry.traceparent.as_deref(), Some(SAMPLED_TRACEPARENT));
    assert!(telemetry.sampled);
}

#[test]
fn test_telemetry_drops_the_change_payload() {
    let rendered = serde_json::to_string(&operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT)))).unwrap();

    for secret in [
        "credential=hunter2",
        "hunter2",
        "secret-digest-map",
        "/var/lib/peryx/private/path",
        &"a".repeat(64),
    ] {
        assert!(
            !rendered.contains(secret),
            "telemetry leaked a payload value: {secret}\n{rendered}"
        );
    }
}

#[test]
fn test_an_untraced_operation_omits_the_traceparent_and_is_not_sampled() {
    let telemetry = operation_telemetry(&traced(None));

    assert_eq!(telemetry.traceparent, None);
    assert!(!telemetry.sampled);
    let rendered = serde_json::to_string(&telemetry).unwrap();
    assert!(
        !rendered.contains("traceparent"),
        "an absent traceparent is omitted: {rendered}"
    );
}

#[test]
fn test_sampled_reads_the_traceparent_flag() {
    assert!(sampled(Some(SAMPLED_TRACEPARENT)));
    assert!(!sampled(Some(UNSAMPLED_TRACEPARENT)));
    assert!(!sampled(None));
}

#[test]
fn test_sampled_rejects_a_malformed_flags_field() {
    assert!(
        !sampled(Some("00-abc-def")),
        "a non-two-digit flags field is not sampled"
    );
    assert!(
        !sampled(Some("00-trace-parent-zz")),
        "a non-hex flags field is not sampled"
    );
    assert!(
        !sampled(Some("nodashes")),
        "a string with no flags field is not sampled"
    );
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn recorded(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

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
    let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
    tracing::subscriber::with_default(subscriber, body);
    std::io::Write::flush(&mut capture.clone()).unwrap();
    capture.recorded()
}

#[test]
fn test_emit_records_a_sampled_operation_without_its_payload() {
    let recorded = captured(|| operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT))).emit());

    assert!(
        recorded.contains("availability operation"),
        "the event is recorded: {recorded}"
    );
    assert!(recorded.contains("primary-a"), "the identity is present: {recorded}");
    assert!(
        recorded.contains(SAMPLED_TRACEPARENT),
        "the traceparent is present: {recorded}"
    );
    for secret in ["hunter2", "secret-digest-map", "/var/lib/peryx/private/path"] {
        assert!(
            !recorded.contains(secret),
            "emit leaked a payload value {secret}: {recorded}"
        );
    }
}

#[test]
fn test_emit_skips_an_unsampled_operation() {
    let recorded = captured(|| operation_telemetry(&traced(Some(UNSAMPLED_TRACEPARENT))).emit());

    assert!(
        recorded.is_empty(),
        "an unsampled operation records nothing: {recorded}"
    );
}

#[test]
fn test_distributed_observer_derives_a_root_trace_and_honors_sampling() {
    let operation = OperationObservation {
        source: "primary-b".to_owned(),
        epoch: peryx_ha::AuthorityEpoch(4),
        serial: 11,
        kind: peryx_ha::OperationKind::Delete,
    };
    let entropy = trace_entropy(&operation);
    let traceparent = root_traceparent(entropy, samples(entropy));

    assert_eq!(traceparent.len(), 55);
    assert_eq!(sampled(Some(&traceparent)), samples(entropy));
    let recorded = captured(|| DistributedOperationObserver.record(operation));
    assert_eq!(recorded.is_empty(), !samples(entropy));
}
