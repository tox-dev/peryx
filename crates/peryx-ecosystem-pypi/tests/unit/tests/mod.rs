use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

mod archive;
mod changelog_tests;
mod conformance_tests;
mod description_tests;
mod fanout_tests;
mod filename_tests;
mod html_tests;
pub mod http;
mod legacy_json_tests;
mod metadata_tests;
mod metrics_tests;
mod name_tests;
mod policy_tests;
mod property_tests;
mod quota_tests;
mod rate_limit_tests;
mod refresh_tests;
mod replication_tests;
mod search;
mod serial_tests;
mod serve;
mod simple;
mod simple_client;
mod stream;
mod upload;
mod version_tests;
mod view_tests;
mod virtual_tests;
mod webhooks_tests;

thread_local! {
    static ACTIVE_CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

fn writer_acl(secret: impl Into<String>) -> peryx_identity::IndexAcl {
    use std::collections::BTreeSet;

    use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

// A permanent subscriber prevents thread-local tests from caching callsites as disabled process-wide.
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

// Test threads are reused, so a dropped capture must detach its buffer.
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

#[test]
fn test_log_writer_writes_and_flushes_the_active_capture() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut writer = LogWriter(Some(Arc::clone(&bytes)));

    assert_eq!(std::io::Write::write(&mut writer, b"event").unwrap(), 5);
    std::io::Write::flush(&mut writer).unwrap();
    assert_eq!(*bytes.lock().unwrap(), b"event");
}

pub fn field<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event["fields"][name].as_str()
}

fn wired(mut state: peryx_driver::AppState) -> Arc<peryx_driver::AppState> {
    install(&mut state);
    Arc::new(state)
}

pub fn install(state: &mut peryx_driver::AppState) {
    let plugins = plugin_registry();
    plugins.register_activated_capabilities(&mut state.capability_install_context());
    plugins
        .install_drivers(
            &mut state.runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
}

fn wired_distributed(mut state: peryx_driver::AppState) -> Arc<peryx_driver::AppState> {
    let plugins = plugin_registry();
    plugins.register_activated_capabilities(&mut state.capability_install_context());
    plugins
        .install_distributed_drivers(
            &mut state.distributed_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
    Arc::new(state)
}

fn plugin_registry() -> peryx_plugin_registry::PluginRegistry {
    peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap()
}

pub async fn administrator_header(state: &Arc<peryx_driver::AppState>) -> String {
    use base64::Engine as _;

    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .users
        .set_password(&user.id, "local password")
        .await
        .unwrap();
    state
        .serving
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
