//! [`Drop`] reaps the managed proxy process. Blocking control requests keep the harness synchronous.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::NamedTempFile;

use super::{
    EVENT_TIMEOUT, HarnessError, StartupSignal, free_port, http_client, kill_group, spawn_in_group, spawn_with_events,
    wait_for_line, wait_for_startup,
};

const CONTROL_HOST: &str = "127.0.0.1";
const START_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_CONTROL_OWNER: AtomicU64 = AtomicU64::new(0);

/// A running `toxiproxy-server` and a client bound to its control API.
pub struct Toxiproxy {
    server: Child,
    control: String,
    http: reqwest::blocking::Client,
    next: u32,
    graceful_shutdown: bool,
    control_owned: bool,
}

impl Toxiproxy {
    /// Spawn `toxiproxy-server` on a free control port and wait until its API answers.
    /// Process scheduling uses the shared event deadlock guard; the start deadline begins at the first child event.
    ///
    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the binary is absent or its API does not come up within
    /// the start deadline.
    pub fn start() -> Result<Self, HarnessError> {
        Self::spawn(Path::new("toxiproxy-server"), START_TIMEOUT, EVENT_TIMEOUT, false)
    }

    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the binary is absent, misses `deadlock_guard` before its first event, or
    /// misses `behavior_timeout` after that event.
    pub fn start_with(
        binary: impl AsRef<Path>,
        behavior_timeout: Duration,
        deadlock_guard: Duration,
    ) -> Result<Self, HarnessError> {
        if behavior_timeout.is_zero() {
            return Err(HarnessError::Toxiproxy(
                "control API did not start within 0ns; process was not spawned".to_owned(),
            ));
        }
        Self::spawn(binary.as_ref(), behavior_timeout, deadlock_guard, true)
    }

    fn spawn(
        binary: &Path,
        behavior_timeout: Duration,
        deadlock_guard: Duration,
        graceful_shutdown: bool,
    ) -> Result<Self, HarnessError> {
        let _allocation_lock = control_allocation_lock()?;
        let control_port = free_port();
        let control = format!("http://{CONTROL_HOST}:{control_port}");
        let control_owner = format!(
            "peryx-control-{}-{}",
            std::process::id(),
            NEXT_CONTROL_OWNER.fetch_add(1, Ordering::Relaxed),
        );
        let mut ownership_config = NamedTempFile::new()?;
        write_ownership_config(&mut ownership_config, &control_owner)?;
        ownership_config.flush()?;
        let mut command = Command::new(binary);
        command
            .args(["-host", CONTROL_HOST, "-port", &control_port.to_string()])
            .arg("-config")
            .arg(ownership_config.path())
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
            control_owned: false,
        };
        let mut first_event_is_startup = false;
        Self::require_event(
            wait_for_startup(&mut toxiproxy.server, &events, deadlock_guard, |event| {
                first_event_is_startup = event.contains("Starting Toxiproxy HTTP server");
                true
            }),
            deadlock_guard,
            "an event",
        )?;
        if !first_event_is_startup {
            Self::require_event(
                wait_for_startup(&mut toxiproxy.server, &events, behavior_timeout, |line| {
                    line.contains("Starting Toxiproxy HTTP server")
                }),
                behavior_timeout,
                "its startup signal",
            )?;
        }
        // Toxiproxy logs startup before binding the socket.
        let deadline = Instant::now() + behavior_timeout;
        let mut last_failure = "startup signal arrived before the control API".to_owned();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Toxiproxy(format!(
                    "control API did not accept requests within {behavior_timeout:?}: {last_failure}"
                )));
            }
            match toxiproxy
                .http
                .get(format!("{}/version", toxiproxy.control))
                .timeout(remaining.min(Duration::from_millis(100)))
                .send()
            {
                Ok(response) => {
                    let status = response.status();
                    match status.as_u16() {
                        200..=299 => {
                            toxiproxy.verify_control_ownership(&control_owner)?;
                            return Ok(toxiproxy);
                        }
                        404 => last_failure = format!("control API returned {status}"),
                        _ => return Err(HarnessError::Toxiproxy(format!("control API returned {status}"))),
                    }
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

    fn verify_control_ownership(&mut self, owner: &str) -> Result<(), HarnessError> {
        let ownership_verified = self
            .http
            .get(format!("{}/proxies/{owner}", self.control))
            .send()
            .is_ok_and(|response| response.status().is_success());
        if !ownership_verified {
            return Err(HarnessError::Toxiproxy(
                "control API does not belong to the spawned process".to_owned(),
            ));
        }
        self.control_owned = true;
        Ok(())
    }

    fn require_event(
        signal: std::io::Result<StartupSignal>,
        within: Duration,
        expected: &str,
    ) -> Result<(), HarnessError> {
        match signal.map_err(|error| HarnessError::Toxiproxy(format!("read control process event: {error}")))? {
            StartupSignal::Matched => Ok(()),
            StartupSignal::Exited(status) => Err(HarnessError::Toxiproxy(format!(
                "control process exited before its startup signal; process status: {status}"
            ))),
            StartupSignal::TimedOut => Err(HarnessError::Toxiproxy(format!(
                "control process did not emit {expected} within {within:?}; process status: None"
            ))),
        }
    }

    /// # Errors
    /// Returns [`HarnessError::Toxiproxy`] when the server rejects the proxy.
    pub fn proxy(&mut self, upstream: &str) -> Result<Proxy, HarnessError> {
        self.next += 1;
        let name = format!("peryx-{}", self.next);
        let response = self.post(
            "/proxies",
            &json!({ "name": name, "listen": format!("{CONTROL_HOST}:0"), "upstream": upstream, "enabled": true }),
        )?;
        let listen = response["listen"]
            .as_str()
            .ok_or_else(|| HarnessError::Toxiproxy("POST /proxies omitted its bound address".to_owned()))?
            .to_owned();
        Ok(Proxy {
            listen,
            name,
            control: self.control.clone(),
            http: self.http.clone(),
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, HarnessError> {
        let response = self
            .http
            .post(format!("{}{path}", self.control))
            .json(body)
            .send()
            .map_err(|error| HarnessError::Toxiproxy(format!("POST {path}: {error}")))?;
        if response.status().is_success() {
            response
                .json()
                .map_err(|error| HarnessError::Toxiproxy(format!("decode POST {path}: {error}")))
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
        if self.graceful_shutdown
            && self.control_owned
            && self
                .http
                .post(format!("{}/shutdown", self.control))
                .send()
                .is_ok_and(|response| response.status().is_success())
        {
            let _ = self.server.wait();
            return;
        }
        kill_group(&mut self.server);
    }
}

fn control_allocation_lock() -> Result<File, HarnessError> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(std::env::temp_dir().join("peryx-toxiproxy-control.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn write_ownership_config(writer: &mut impl std::io::Write, owner: &str) -> std::io::Result<()> {
    let ownership = json!([{ "name": owner, "upstream": "127.0.0.1:1", "enabled": false }]).to_string();
    writer.write_all(ownership.as_bytes())
}

pub(super) fn run_process_fixture() -> std::process::ExitCode {
    #[cfg(unix)]
    if let Some(mode) = std::env::args_os().nth(1)
        && matches!(
            mode.to_str(),
            Some("toxiproxy-config-write" | "toxiproxy-config-write-error")
        )
    {
        let (reader, writer) = nix::unistd::pipe().expect("create ownership config pipe");
        let _reader = (mode == "toxiproxy-config-write").then_some(reader);
        let mut writer = File::from(writer);
        return match write_ownership_config(&mut writer, "fixture-owner") {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("toxiproxy ownership config write failed: {:?}", error.kind());
                std::process::ExitCode::FAILURE
            }
        };
    }
    super::process_fixture::run()
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
