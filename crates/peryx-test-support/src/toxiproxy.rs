//! [`Drop`] reaps the managed proxy process. Blocking control requests keep the harness synchronous.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use super::{
    HarnessError, ProcessSignal, free_port, http_client, kill_group, spawn_in_group, spawn_with_events, wait_for_line,
};

const CONTROL_HOST: &str = "127.0.0.1";
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// A running `toxiproxy-server` and a client bound to its control API.
pub struct Toxiproxy {
    server: Child,
    control: String,
    http: reqwest::blocking::Client,
    next: u32,
    graceful_shutdown: bool,
}

impl Toxiproxy {
    /// Spawn `toxiproxy-server` on a free control port and wait until its API answers.
    ///
    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the binary is absent or its API does not come up within
    /// the start deadline.
    pub fn start() -> Result<Self, HarnessError> {
        Self::spawn(Path::new("toxiproxy-server"), START_TIMEOUT, false)
    }

    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the binary is absent or its API misses the deadline.
    pub fn start_with(binary: impl AsRef<Path>, timeout: Duration) -> Result<Self, HarnessError> {
        if timeout.is_zero() {
            return Err(HarnessError::Toxiproxy(
                "control API did not start within 0ns; process was not spawned".to_owned(),
            ));
        }
        Self::spawn(binary.as_ref(), timeout, true)
    }

    fn spawn(binary: &Path, timeout: Duration, graceful_shutdown: bool) -> Result<Self, HarnessError> {
        let deadline = Instant::now() + timeout;
        let control_port = free_port();
        let control = format!("http://{CONTROL_HOST}:{control_port}");
        let mut command = Command::new(binary);
        command
            .args(["-host", CONTROL_HOST, "-port", &control_port.to_string()])
            .stdin(Stdio::piped());
        spawn_in_group(&mut command);
        let (server, events) = spawn_with_events(&mut command, None, true)
            .map_err(|error| HarnessError::Toxiproxy(format!("spawn toxiproxy-server (is it installed?): {error}")))?;
        let mut toxiproxy = Self {
            server,
            control,
            http: http_client(Duration::from_secs(2)),
            next: 0,
            graceful_shutdown,
        };
        match wait_for_line(&events, deadline.saturating_duration_since(Instant::now()), |line| {
            line.contains("Starting Toxiproxy HTTP server")
        }) {
            ProcessSignal::Matched => {}
            ProcessSignal::Closed => {
                let status = toxiproxy.server.wait().expect("reap toxiproxy child");
                return Err(HarnessError::Toxiproxy(format!(
                    "control process exited before its startup signal; process status: {status}"
                )));
            }
            ProcessSignal::TimedOut => {
                return Err(HarnessError::Toxiproxy(format!(
                    "control API did not start within {timeout:?}; process status: {:?}",
                    toxiproxy.server.try_wait().expect("read toxiproxy child status")
                )));
            }
        }
        // Toxiproxy logs startup before binding the socket.
        let mut last_failure = "startup signal arrived before the control API".to_owned();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Toxiproxy(format!(
                    "control API did not accept requests within {timeout:?}: {last_failure}"
                )));
            }
            match toxiproxy
                .http
                .get(format!("{}/version", toxiproxy.control))
                .timeout(remaining)
                .send()
            {
                Ok(response) if response.status().is_success() => return Ok(toxiproxy),
                Ok(response) => {
                    return Err(HarnessError::Toxiproxy(format!(
                        "control API returned {} after its startup signal",
                        response.status()
                    )));
                }
                Err(error) => last_failure = error.to_string(),
            }
            let _ = wait_for_line(
                &events,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
                |_| false,
            );
            if let Some(status) = toxiproxy.server.try_wait().expect("read toxiproxy child status") {
                return Err(HarnessError::Toxiproxy(format!(
                    "control process exited before accepting requests; process status: {status}"
                )));
            }
        }
    }

    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the server rejects the proxy.
    pub fn proxy(&mut self, upstream: &str) -> Result<Proxy, HarnessError> {
        self.next += 1;
        let name = format!("peryx-{}", self.next);
        let listen = format!("{CONTROL_HOST}:{}", free_port());
        self.post(
            "/proxies",
            &json!({ "name": name, "listen": listen, "upstream": upstream, "enabled": true }),
        )?;
        Ok(Proxy {
            listen,
            name,
            control: self.control.clone(),
            http: self.http.clone(),
        })
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> Result<(), HarnessError> {
        let response = self
            .http
            .post(format!("{}{path}", self.control))
            .json(body)
            .send()
            .map_err(|error| HarnessError::Toxiproxy(format!("POST {path}: {error}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(HarnessError::Toxiproxy(format!(
                "POST {path} returned {}",
                response.status()
            )))
        }
    }

    /// Whether the control API still answers, so a test can prove it detects a dead proxy server.
    #[must_use]
    pub fn control_is_up(&self) -> bool {
        self.http.get(format!("{}/version", self.control)).send().is_ok()
    }

    /// Kill the proxy server, so a test can observe a proxy failure.
    pub fn kill(&mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if matches!(self.server.try_wait(), Ok(Some(_))) {
            return;
        }
        if self.graceful_shutdown && self.http.post(format!("{}/shutdown", self.control)).send().is_ok() {
            let _ = self.server.wait();
            return;
        }
        kill_group(&mut self.server);
    }
}

impl Drop for Toxiproxy {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One proxy in front of a node's socket. Clients dial [`endpoint`](Proxy::endpoint); the harness cuts or
/// slows the link through the control API.
pub struct Proxy {
    listen: String,
    name: String,
    control: String,
    http: reqwest::blocking::Client,
}

impl Proxy {
    /// The `host:port` a client connects to instead of the node directly.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.listen
    }

    /// Cut the link: the proxy stops forwarding, so a connection through it fails. This is a partition.
    ///
    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the control call fails.
    pub fn partition(&self) -> Result<(), HarnessError> {
        self.set_enabled(false)
    }

    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the control call fails.
    pub fn heal(&self) -> Result<(), HarnessError> {
        self.set_enabled(true)
    }

    /// Add `delay` of latency in both directions, so a test can pause a link without cutting it.
    ///
    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the control call fails.
    pub fn pause(&self, delay: Duration) -> Result<(), HarnessError> {
        let response = self
            .http
            .post(format!("{}/proxies/{}/toxics", self.control, self.name))
            .json(&json!({
                "name": "pause", "type": "latency", "stream": "downstream",
                "attributes": { "latency": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX) },
            }))
            .send()
            .map_err(|error| HarnessError::Toxiproxy(format!("add latency toxic: {error}")))?;
        response
            .status()
            .is_success()
            .then_some(())
            .ok_or_else(|| HarnessError::Toxiproxy(format!("add latency toxic returned {}", response.status())))
    }

    /// Remove the latency toxic a [`pause`](Proxy::pause) added, so the link runs at full speed again.
    ///
    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the control call fails.
    pub fn resume(&self) -> Result<(), HarnessError> {
        let response = self
            .http
            .delete(format!("{}/proxies/{}/toxics/pause", self.control, self.name))
            .send()
            .map_err(|error| HarnessError::Toxiproxy(format!("remove latency toxic: {error}")))?;
        response
            .status()
            .is_success()
            .then_some(())
            .ok_or_else(|| HarnessError::Toxiproxy(format!("remove latency toxic returned {}", response.status())))
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), HarnessError> {
        let response = self
            .http
            .post(format!("{}/proxies/{}", self.control, self.name))
            .json(&json!({ "enabled": enabled }))
            .send()
            .map_err(|error| HarnessError::Toxiproxy(format!("toggle proxy: {error}")))?;
        response
            .status()
            .is_success()
            .then_some(())
            .ok_or_else(|| HarnessError::Toxiproxy(format!("toggle proxy returned {}", response.status())))
    }
}
