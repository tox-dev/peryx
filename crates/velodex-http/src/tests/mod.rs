use std::sync::{Arc, Mutex};

mod api_tests;
mod archive_tests;
mod fanout_tests;
mod http_tests;
mod metrics_tests;
mod rate_limit_tests;
mod refresh_tests;
mod search_tests;
mod serve_tests;
mod stream_tests;
mod upload_tests;

#[derive(Clone, Default)]
struct LogCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl LogCapture {
    fn install(&self) -> tracing::dispatcher::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .with_writer(self.clone())
                .finish(),
        )
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

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(self.bytes.clone())
    }
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn field<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event["fields"][name].as_str()
}
