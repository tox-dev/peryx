use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

mod archive;
mod changelog_tests;
mod conformance_tests;
mod description_tests;
mod fanout_tests;
mod filename_tests;
mod html_tests;
mod http;
mod legacy_json_tests;
mod metadata_tests;
mod metrics_tests;
mod name_tests;
mod policy_tests;
mod quota_tests;
mod rate_limit_tests;
mod refresh_tests;
mod search;
mod serial_tests;
mod serve;
mod simple;
mod simple_client;
mod stream;
mod upload;
mod version_tests;
mod virtual_tests;
mod webhooks_tests;

thread_local! {
    /// The capture buffer for the test running on this thread, if it installed a [`LogCapture`].
    /// Events on threads with no active capture (other tests, background workers) route to nothing.
    static ACTIVE_CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// An ACL whose one named token writes and deletes across every project, for tests that need a
/// hosted index that accepts uploads.
fn writer_acl(secret: impl Into<String>) -> peryx_identity::IndexAcl {
    use std::collections::BTreeSet;

    use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                projects: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

/// Install one process-global JSON subscriber the first time any test captures logs.
///
/// A single, permanent subscriber keeps tracing's per-callsite interest cache stable: every
/// `security_event` callsite stays enabled for the life of the test binary. The earlier design set a
/// *thread-local* subscriber per test, so a thread running a non-capturing test had no subscriber and,
/// if it hit a callsite first, cached it as `Interest::never()` process-wide, intermittently dropping
/// events from capturing tests on other threads under parallel runs. This subscriber instead routes
/// every event to the current thread's [`ACTIVE_CAPTURE`] buffer, so tests stay isolated without
/// poisoning the cache.
pub fn install_global_subscriber() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(ThreadLocalWriter)
            .init();
    });
}

#[derive(Clone, Default)]
pub struct LogCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl LogCapture {
    fn install(&self) -> CaptureGuard {
        install_global_subscriber();
        ACTIVE_CAPTURE.with(|slot| *slot.borrow_mut() = Some(self.bytes.clone()));
        CaptureGuard
    }

    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().expect("log capture lock").clone()).unwrap()
    }

    fn security_events(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["fields"]["security_event"].as_bool() == Some(true))
            .collect()
    }
}

/// Detaches this thread's capture buffer when a test's [`LogCapture`] goes out of scope, so later
/// events on the reused test thread are not appended to a finished test's buffer.
struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACTIVE_CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}

struct ThreadLocalWriter;

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for ThreadLocalWriter {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(ACTIVE_CAPTURE.with(|slot| slot.borrow().clone()))
    }
}

struct LogWriter(Option<Arc<Mutex<Vec<u8>>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(bytes) = &self.0 {
            bytes.lock().expect("log capture lock").extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn field<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event["fields"][name].as_str()
}

/// Wrap a freshly built [`AppState`](peryx_driver::AppState) in an `Arc` with the `PyPI` serving
/// driver and search indexer installed, exactly as the binary wires it at startup. Serving tests
/// build their state through this so requests dispatch through the real driver instead of the neutral
/// no-op defaults an unwired [`AppState`](peryx_driver::AppState) carries.
fn wired(mut state: peryx_driver::AppState) -> Arc<peryx_driver::AppState> {
    crate::install(&mut state);
    Arc::new(state)
}

/// Provision a server administrator on a wired state and return the Basic header authenticating as
/// it, so a test can read the operator- and administrator-class `/+status` fields the anonymous
/// document withholds.
pub async fn administrator_header(state: &Arc<peryx_driver::AppState>) -> String {
    use base64::Engine as _;

    let user = state.users.create("Alice").unwrap();
    state.users.set_password(&user.id, "local password").await.unwrap();
    state
        .authorization
        .grant(
            &user.id,
            peryx_identity::Role::Administrator,
            peryx_identity::GrantScope::Server,
        )
        .unwrap();
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("Alice:local password")
    )
}
