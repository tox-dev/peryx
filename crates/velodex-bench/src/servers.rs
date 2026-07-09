//! Neutral server lifecycle: how a server starts, where it is reached, and how readiness is probed.
//!
//! The concrete servers under test and their index-URL shapes are per-ecosystem definitions; this
//! module only spawns, health-checks, and tears them down.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

/// How long a server gets to answer its first request (uvx may resolve an environment first).
const START_TIMEOUT: Duration = Duration::from_mins(3);

/// One index server under test; every field is filled in by a per-ecosystem definition.
pub struct Server {
    pub name: &'static str,
    pub homepage: &'static str,
    /// The base URL a client points at, given the port the server listens on: a `PyPI` simple index
    /// (`http://host/root/pypi/simple/`) or an `OCI` registry prefix (`http://host/dockerhub/`).
    pub base_url: fn(u16) -> String,
    /// The readiness URL derived from the base, hit until any HTTP status answers: a `PyPI` project
    /// page, an `OCI` `/v2/` root.
    pub probe: fn(&str) -> String,
    /// How to spawn the server; `None` for a party that runs no process (a direct baseline).
    pub command: Option<fn(u16, &Path) -> Command>,
    /// One-time preparation before the first spawn (init a datadir, write a config).
    pub setup: Option<fn(u16, &Path) -> anyhow::Result<()>>,
    /// Teardown after the spawned process is killed, keyed by port. A container competitor detaches
    /// from the process that launched it, so killing that process is not enough; this removes it.
    pub teardown: Option<fn(u16)>,
}

/// A started server: where to reach it and the process behind it (none for direct).
pub struct Active {
    pub url: String,
    process: Option<Child>,
    log: Option<PathBuf>,
    probe_url: String,
    port: u16,
    teardown: Option<fn(u16)>,
}

impl Active {
    /// The root process's id, when a server runs at all.
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Child::id)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
        if let Some(teardown) = self.teardown {
            teardown(self.port);
        }
    }
}

impl Server {
    /// Start this server against `state` and wait until it answers.
    ///
    /// # Errors
    /// Returns an error when the server exits early or never becomes ready; includes its log tail.
    pub async fn start(&self, state: &Path, client: &reqwest::Client) -> anyhow::Result<Active> {
        let port = free_port()?;
        let url = (self.base_url)(port);
        let probe_url = (self.probe)(&url);
        let Some(command) = self.command else {
            return Ok(Active {
                url,
                process: None,
                log: None,
                probe_url,
                port,
                teardown: None,
            });
        };
        if let Some(setup) = self.setup {
            setup(port, state)?;
        }
        let log = state.join("server.log");
        let sink = std::fs::File::create(&log)?;
        let process = command(port, state)
            .stdout(Stdio::from(sink.try_clone()?))
            .stderr(Stdio::from(sink))
            .spawn()
            .with_context(|| format!("{} did not start", self.name))?;
        let mut active = Active {
            url,
            process: Some(process),
            log: Some(log),
            probe_url,
            port,
            teardown: self.teardown,
        };
        active.wait_ready(client).await.with_context(|| {
            let tail = active
                .log
                .as_ref()
                .and_then(|log| std::fs::read_to_string(log).ok())
                .unwrap_or_default();
            format!("{}; server log tail:\n{}", self.name, last_chars(&tail, 2000))
        })?;
        Ok(active)
    }
}

impl Active {
    async fn wait_ready(&mut self, client: &reqwest::Client) -> anyhow::Result<()> {
        let probe = self.probe_url.clone();
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(process) = self.process.as_mut()
                && let Some(status) = process.try_wait()?
            {
                bail!("server exited early with {status}");
            }
            // Any HTTP status means the server is up and routing; only transport errors retry.
            match client.get(&probe).timeout(Duration::from_secs(30)).send().await {
                Ok(_) => return Ok(()),
                Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
            }
        }
        bail!("server never answered at {probe}")
    }
}

fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn last_chars(text: &str, count: usize) -> &str {
    let start = text.len().saturating_sub(count);
    let boundary = (start..text.len())
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(0);
    &text[boundary..]
}
