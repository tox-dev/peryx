//! The index servers under test: how each starts, where its simple index lives, and its docs.
//!
//! Competitors run from their published packages via `uvx`; nothing is installed globally.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::report::repo_root;

/// How long a server gets to answer its first request (uvx may resolve an environment first).
const START_TIMEOUT: Duration = Duration::from_mins(3);

/// One index server under test.
pub struct Server {
    pub name: &'static str,
    pub homepage: &'static str,
    simple_url: fn(u16) -> String,
    command: Option<fn(u16, &Path) -> Command>,
    setup: Option<fn(u16, &Path) -> anyhow::Result<()>>,
}

/// Every party the tables compare, `direct` being the no-proxy baseline.
pub fn all() -> Vec<Server> {
    vec![velodex(), direct(), devpi(), proxpi(), pypiserver(), pypicloud()]
}

fn velodex() -> Server {
    Server {
        name: "velodex",
        homepage: "https://velodex.readthedocs.io/",
        simple_url: |port| format!("http://127.0.0.1:{port}/root/pypi/simple/"),
        command: Some(|port, state| {
            let mut command = Command::new(repo_root().join("target").join("release").join("velodex"));
            command
                .arg("serve")
                .args(["--host", "127.0.0.1"])
                .args(["--port", &port.to_string()])
                .arg("--data-dir")
                .arg(state);
            command
        }),
        setup: None,
    }
}

fn direct() -> Server {
    Server {
        name: "direct",
        homepage: "https://pypi.org/",
        simple_url: |_port| "https://pypi.org/simple/".to_owned(),
        command: None,
        setup: None,
    }
}

fn devpi() -> Server {
    Server {
        name: "devpi",
        homepage: "https://devpi.net/docs/",
        simple_url: |port| format!("http://127.0.0.1:{port}/root/pypi/+simple/"),
        command: Some(|port, state| {
            let mut command = Command::new("uvx");
            command
                .args(["--from", "devpi-server", "devpi-server"])
                .arg("--serverdir")
                .arg(state)
                .args(["--port", &port.to_string()]);
            command
        }),
        setup: Some(|_port, state| {
            let output = Command::new("uvx")
                .args(["--from", "devpi-server", "devpi-init", "--serverdir"])
                .arg(state)
                .output()
                .context("devpi-init did not start")?;
            if !output.status.success() {
                bail!("devpi-init failed:\n{}", String::from_utf8_lossy(&output.stderr));
            }
            Ok(())
        }),
    }
}

fn proxpi() -> Server {
    Server {
        name: "proxpi",
        homepage: "https://github.com/EpicWink/proxpi",
        simple_url: |port| format!("http://127.0.0.1:{port}/index/"),
        command: Some(|port, _state| {
            let mut command = Command::new("uvx");
            command
                .args(["--from", "proxpi", "--with", "gunicorn", "gunicorn"])
                .args(["--bind", &format!("127.0.0.1:{port}")])
                .args(["--workers", "4", "proxpi.server:app"]);
            command
        }),
        setup: None,
    }
}

fn pypiserver() -> Server {
    Server {
        name: "pypiserver",
        homepage: "https://github.com/pypiserver/pypiserver",
        simple_url: |port| format!("http://127.0.0.1:{port}/simple/"),
        command: Some(|port, state| {
            let mut command = Command::new("uvx");
            command
                .args(["--from", "pypiserver[passlib]", "pypi-server", "run"])
                .args(["-p", &port.to_string()])
                .args(["--fallback-url", "https://pypi.org/simple/"])
                .args(["-P", ".", "-a", "."])
                .arg(state);
            command
        }),
        setup: None,
    }
}

fn pypicloud() -> Server {
    Server {
        name: "pypicloud",
        homepage: "https://pypicloud.readthedocs.io/",
        simple_url: |port| format!("http://127.0.0.1:{port}/simple/"),
        command: Some(|_port, state| {
            let mut command = Command::new("uvx");
            command
                .args(["--python", "3.10", "--from", "pypicloud"])
                .args(["--with", "sqlalchemy<2", "--with", "waitress", "pserve"])
                .arg(state.join("pypicloud.ini"));
            command
        }),
        setup: Some(|port, state| {
            // pypicloud's `fallback = cache` mode is the closest analog to a read-through cache.
            let ini = format!(
                "[app:main]\n\
                     use = egg:pypicloud\n\
                     pyramid.reload_templates = False\n\
                     pypi.fallback = cache\n\
                     pypi.default_read = everyone\n\
                     pypi.cache_update = everyone\n\
                     pypi.storage = file\n\
                     storage.dir = {packages}\n\
                     db.url = sqlite:///{db}\n\
                     session.encrypt_key = {zeros}\n\
                     session.validate_key = {zeros}\n\
                     auth.admins =\n\
                     \n\
                     [server:main]\n\
                     use = egg:waitress#main\n\
                     host = 127.0.0.1\n\
                     port = {port}\n\
                     threads = 8\n",
                packages = state.join("packages").display(),
                db = state.join("db.sqlite").display(),
                zeros = "0".repeat(64),
            );
            std::fs::write(state.join("pypicloud.ini"), ini).context("pypicloud.ini")
        }),
    }
}

/// A started server: where to reach it and the process behind it (none for direct).
pub struct Active {
    pub url: String,
    process: Option<Child>,
    log: Option<PathBuf>,
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
    }
}

impl Server {
    /// Start this server against `state` and wait until it answers.
    ///
    /// # Errors
    /// Returns an error when the server exits early or never becomes ready; includes its log tail.
    pub async fn start(&self, state: &Path, client: &reqwest::Client) -> anyhow::Result<Active> {
        let port = free_port()?;
        let Some(command) = self.command else {
            return Ok(Active {
                url: (self.simple_url)(port),
                process: None,
                log: None,
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
            url: (self.simple_url)(port),
            process: Some(process),
            log: Some(log),
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
        let probe = format!("{}six/", self.url);
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
