//! The `PyPI` index servers under test: how each starts, where its simple index lives, and its docs.
//!
//! Competitors run from their published packages via `uvx`; nothing is installed globally.

use std::process::Command;

use anyhow::{Context as _, bail};

use crate::report::peryx_binary;
use crate::servers::Server;

/// The always-present project every simple index answers for, used to probe readiness.
const PROBE: &str = "six/";

/// Every party the tables compare, `direct` being the no-proxy baseline.
pub fn all() -> Vec<Server> {
    vec![peryx(), direct(), devpi(), proxpi(), pypiserver(), pypicloud()]
}

fn peryx() -> Server {
    Server {
        name: "peryx",
        homepage: "https://peryx.readthedocs.io/",
        base_url: |port| format!("http://127.0.0.1:{port}/root/pypi/simple/"),
        probe: |base| format!("{base}{PROBE}"),
        command: Some(|port, state| {
            let mut command = Command::new(peryx_binary());
            command
                .arg("serve")
                .args(["--host", "127.0.0.1"])
                .args(["--port", &port.to_string()])
                .arg("--data-dir")
                .arg(state);
            command
        }),
        setup: None,
        teardown: None,
    }
}

fn direct() -> Server {
    Server {
        name: "direct",
        homepage: "https://pypi.org/",
        base_url: |_port| "https://pypi.org/simple/".to_owned(),
        probe: |base| format!("{base}{PROBE}"),
        command: None,
        setup: None,
        teardown: None,
    }
}

fn devpi() -> Server {
    Server {
        name: "devpi",
        homepage: "https://devpi.net/docs/",
        base_url: |port| format!("http://127.0.0.1:{port}/root/pypi/+simple/"),
        probe: |base| format!("{base}{PROBE}"),
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
        teardown: None,
    }
}

fn proxpi() -> Server {
    Server {
        name: "proxpi",
        homepage: "https://github.com/EpicWink/proxpi",
        base_url: |port| format!("http://127.0.0.1:{port}/index/"),
        probe: |base| format!("{base}{PROBE}"),
        command: Some(|port, _state| {
            let mut command = Command::new("uvx");
            command
                .args(["--from", "proxpi", "--with", "gunicorn", "gunicorn"])
                .args(["--bind", &format!("127.0.0.1:{port}")])
                .args(["--workers", "4", "proxpi.server:app"]);
            command
        }),
        setup: None,
        teardown: None,
    }
}

fn pypiserver() -> Server {
    Server {
        name: "pypiserver",
        homepage: "https://github.com/pypiserver/pypiserver",
        base_url: |port| format!("http://127.0.0.1:{port}/simple/"),
        probe: |base| format!("{base}{PROBE}"),
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
        teardown: None,
    }
}

fn pypicloud() -> Server {
    Server {
        name: "pypicloud",
        homepage: "https://pypicloud.readthedocs.io/",
        base_url: |port| format!("http://127.0.0.1:{port}/simple/"),
        probe: |base| format!("{base}{PROBE}"),
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
        teardown: None,
    }
}
