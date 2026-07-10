use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

mod archive;
mod conformance_tests;
mod fanout_tests;
mod filename_tests;
mod html_tests;
mod http;
mod metadata_tests;
mod metrics_tests;
mod name_tests;
mod policy_tests;
mod rate_limit_tests;
mod refresh_tests;
mod search_tests;
mod serve;
mod simple;
mod stream;
mod upload_tests;
mod version_tests;
mod virtual_tests;
mod webhooks_tests;

thread_local! {
    /// The capture buffer for the test running on this thread, if it installed a [`LogCapture`].
    /// Events on threads with no active capture (other tests, background workers) route to nothing.
    static ACTIVE_CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
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
fn install_global_subscriber() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::INFO)
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
