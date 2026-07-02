# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Benchmark velodex against direct PyPI and competing index servers.

Two workloads:

- **install**: time ``uv pip install`` and ``pip install`` of the top PyPI packages through each
  server, cold (fresh server state) and warm (the server keeps its cache, the client starts
  over). This is the number a user feels.
- **load**: request-level throughput with locust, one user and a concurrent swarm, against each
  warm server.

Results land as JSON feeds under ``site/data/bench/``; the documentation renders them as tinted
tables (best-in-row green to worst-in-row red) via the ``bench`` shortcode. Competitors run from
their published packages via ``uvx``; nothing is installed globally.

One command reproduces every table the documentation shows (velodex is built automatically when
the release binary is missing)::

    uv run tests/perf/run.py
"""

from __future__ import annotations

import argparse
import json
import logging
import math
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Final

sys.path.insert(0, str(Path(__file__).parent))
from packages import TOP_PACKAGES

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator

__all__ = ["main"]

_LOG: Final = logging.getLogger("bench")

REPO: Final = Path(__file__).resolve().parent.parent.parent
FEEDS: Final = REPO / "site" / "data" / "bench"
START_TIMEOUT: Final = 180.0
LADDER: Final = ("faster", "par", "mild", "slow", "veryslow", "worst")
# The row's best figure takes the green end and the rest log-interpolate toward red; the scale
# never compresses below an 8x span, so a near-parity row reads green throughout.
MIN_SPAN: Final = math.log(8.0)


@dataclass(frozen=True)
class Server:
    """One index server under test: how to start it, where its simple index lives, and its docs."""

    name: str
    homepage: str
    simple_url: Callable[[int], str]
    command: Callable[[int, Path], list[str]] | None
    setup: Callable[[int, Path], None] | None = None


def _pypicloud_config(port: int, state: Path) -> None:
    """pypicloud's `fallback = cache` mode is the closest analog to a read-through cache."""
    (state / "pypicloud.ini").write_text(
        f"""\
[app:main]
use = egg:pypicloud
pyramid.reload_templates = False
pypi.fallback = cache
pypi.default_read = everyone
pypi.cache_update = everyone
pypi.storage = file
storage.dir = {state / "packages"}
db.url = sqlite:///{state / "db.sqlite"}
session.encrypt_key = 0000000000000000000000000000000000000000000000000000000000000000
session.validate_key = 0000000000000000000000000000000000000000000000000000000000000000
auth.admins =

[server:main]
use = egg:waitress#main
host = 127.0.0.1
port = {port}
threads = 8
"""
    )


SERVERS: Final = (
    Server(
        name="velodex",
        homepage="https://velodex.readthedocs.io/",
        simple_url=lambda port: f"http://127.0.0.1:{port}/root/pypi/simple/",
        command=lambda port, state: [
            str(REPO / "target" / "release" / "velodex"),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--data-dir",
            str(state),
        ],
    ),
    Server(
        name="direct",
        homepage="https://pypi.org/",
        simple_url=lambda _port: "https://pypi.org/simple/",
        command=None,
    ),
    Server(
        name="devpi",
        homepage="https://devpi.net/docs/",
        simple_url=lambda port: f"http://127.0.0.1:{port}/root/pypi/+simple/",
        command=lambda port, state: [
            "uvx",
            "--from",
            "devpi-server",
            "devpi-server",
            "--serverdir",
            str(state),
            "--port",
            str(port),
        ],
    ),
    Server(
        name="proxpi",
        homepage="https://github.com/EpicWink/proxpi",
        simple_url=lambda port: f"http://127.0.0.1:{port}/index/",
        command=lambda port, _state: [
            "uvx",
            "--from",
            "proxpi",
            "--with",
            "gunicorn",
            "gunicorn",
            "--bind",
            f"127.0.0.1:{port}",
            "--workers",
            "4",
            "proxpi.server:app",
        ],
    ),
    Server(
        name="pypiserver",
        homepage="https://github.com/pypiserver/pypiserver",
        simple_url=lambda port: f"http://127.0.0.1:{port}/simple/",
        command=lambda port, state: [
            "uvx",
            "--from",
            "pypiserver[passlib]",
            "pypi-server",
            "run",
            "-p",
            str(port),
            "--fallback-url",
            "https://pypi.org/simple/",
            "-P",
            ".",
            "-a",
            ".",
            str(state),
        ],
    ),
    Server(
        name="pypicloud",
        homepage="https://pypicloud.readthedocs.io/",
        simple_url=lambda port: f"http://127.0.0.1:{port}/simple/",
        command=lambda _port, state: [
            "uvx",
            "--python",
            "3.10",
            "--from",
            "pypicloud",
            "--with",
            "sqlalchemy<2",
            "--with",
            "waitress",
            "pserve",
            str(state / "pypicloud.ini"),
        ],
        setup=_pypicloud_config,
    ),
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=1, help="measurements per cell; the best is kept")
    parser.add_argument("--skip-pip", action="store_true", help="skip the pip client (uv only)")
    parser.add_argument("--skip-load", action="store_true", help="skip the locust request workload")
    parser.add_argument("--skip-install", action="store_true", help="skip the install workload")
    parser.add_argument(
        "--only",
        default="",
        help="comma-separated server names to run (default: all)",
    )
    arguments = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s", datefmt="%H:%M:%S")
    ensure_velodex_built()
    servers = [server for server in SERVERS if not arguments.only or server.name in arguments.only.split(",")]
    if not arguments.skip_install:
        bench_installs(servers, ["uv", *([] if arguments.skip_pip else ["pip"])], arguments.runs)
    if not arguments.skip_load:
        bench_load(servers, users=(1, 32))


def ensure_velodex_built() -> None:
    """Build the release binary when it is absent, so one command reproduces everything."""
    binary = REPO / "target" / "release" / "velodex"
    if binary.exists():
        return
    _LOG.info("building velodex (release)")
    subprocess.run(["cargo", "build", "--release"], check=True, cwd=REPO)


def bench_installs(servers: list[Server], clients: list[str], runs: int) -> None:
    """The install workload: every server, cold then warm, per client; best of `runs`."""
    prewarm_cdn()
    for client in clients:
        results: dict[str, dict[str, float]] = {}
        for server in servers:
            colds: list[float] = []
            warms: list[float] = []
            for attempt in range(runs):
                with tempfile.TemporaryDirectory(prefix=f"bench-{server.name}-") as scratch:
                    state = Path(scratch) / "state"
                    state.mkdir()
                    with running(server, state) as index_url:
                        _LOG.info("[%s] %s #%d: cold", client, server.name, attempt + 1)
                        colds.append(install_once(client, index_url, Path(scratch)))
                        _LOG.info("[%s] %s #%d: warm", client, server.name, attempt + 1)
                        warms.append(install_once(client, index_url, Path(scratch)))
            results[server.name] = {"cold": min(colds), "warm": min(warms)}
            _LOG.info("[%s] %s: cold %.1fs warm %.1fs", client, server.name, min(colds), min(warms))
        names = [server.name for server in servers]
        baseline = names.index("direct") if "direct" in names else 0
        rows = [
            {
                "name": f"{phase} cache",
                "cells": tinted_cells([results[server.name][phase] for server in servers], baseline=baseline),
            }
            for phase in ("cold", "warm")
        ]
        write_feed(
            f"install-{client}",
            {
                "label": f"{client}: install the top {len(TOP_PACKAGES)} PyPI packages",
                "baseline": names[baseline],
                "parties": [{"name": server.name, "url": server.homepage} for server in servers],
                "rows": rows,
            },
        )


@contextmanager
def running(server: Server, state: Path) -> Iterator[str]:
    """Start `server` against `state`.

    Yields:
        The server's simple-index URL, ready to answer requests.

    Raises:
        RuntimeError: The server exited or never became ready; includes its log tail.
    """
    port = _free_port()
    if server.command is None:
        yield server.simple_url(port)
        return
    if server.name == "devpi":
        subprocess.run(
            ["uvx", "--from", "devpi-server", "devpi-init", "--serverdir", str(state)],
            check=True,
            capture_output=True,
        )
    if server.setup is not None:
        server.setup(port, state)
    log_path = state / "server.log"
    with log_path.open("wb") as log:
        process = subprocess.Popen(server.command(port, state), stdout=log, stderr=subprocess.STDOUT)
        try:
            try:
                _wait_ready(server.simple_url(port) + "six/", process)
            except (TimeoutError, RuntimeError) as error:
                tail = log_path.read_text(errors="replace")[-2000:]
                message = f"{error}; server log tail:\n{tail}"
                raise RuntimeError(message) from error
            yield server.simple_url(port)
        finally:
            process.terminate()
            process.wait(timeout=10)


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _wait_ready(url: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        if process.poll() is not None:
            msg = f"server exited early with {process.returncode}"
            raise RuntimeError(msg)
        try:
            with urllib.request.urlopen(url, timeout=30):
                return
        except urllib.error.HTTPError:
            return  # any HTTP status means the server is up and routing
        except (urllib.error.URLError, TimeoutError):
            time.sleep(0.3)
    msg = f"server never answered at {url}"
    raise TimeoutError(msg)


def prewarm_cdn() -> None:
    """One unmeasured direct install so PyPI's CDN edge is equally warm for every party.

    Without it the first party measured pays the CDN's cold-cache penalty and everyone after
    rides the edge cache that run just warmed, biasing the comparison by run order.
    """
    _LOG.info("prewarming the CDN edge (unmeasured)")
    with tempfile.TemporaryDirectory(prefix="bench-prewarm-") as scratch:
        install_once("uv", "https://pypi.org/simple/", Path(scratch))


def install_once(client: str, index_url: str, scratch: Path) -> float:
    """Time one from-scratch install of the workload through `index_url`.

    Returns:
        Wall-clock seconds for the install command alone.

    Raises:
        RuntimeError: The install command failed; its stderr tail is included.
    """
    with tempfile.TemporaryDirectory(prefix="client-", dir=scratch) as workdir:
        venv = Path(workdir) / "venv"
        cache = Path(workdir) / "client-cache"
        subprocess.run(["uv", "venv", str(venv)], check=True, capture_output=True)
        if client == "uv":
            command = ["uv", "pip", "install", "--index-url", index_url, *TOP_PACKAGES]
            env = {**os.environ, "VIRTUAL_ENV": str(venv), "UV_CACHE_DIR": str(cache)}
        else:
            subprocess.run(
                ["uv", "pip", "install", "--python", str(venv / "bin" / "python"), "pip"],
                check=True,
                capture_output=True,
            )
            command = [
                str(venv / "bin" / "pip"),
                "install",
                "--no-cache-dir",
                "--disable-pip-version-check",
                "--index-url",
                index_url,
                *TOP_PACKAGES,
            ]
            env = dict(os.environ)
        start = time.monotonic()
        result = subprocess.run(command, env=env, capture_output=True, check=False)
        elapsed = time.monotonic() - start
    if result.returncode != 0:
        msg = f"install via {index_url} failed:\n{result.stderr.decode()[-2000:]}"
        raise RuntimeError(msg)
    return elapsed


def bench_load(servers: list[Server], users: tuple[int, ...]) -> None:
    """The request workload: locust against each warm server, per swarm size."""
    metrics: dict[str, dict[int, dict[str, float]]] = {}
    for server in servers:
        with tempfile.TemporaryDirectory(prefix=f"load-{server.name}-") as scratch:
            state = Path(scratch) / "state"
            state.mkdir()
            with running(server, state) as index_url:
                warm_pages(index_url)
                for count in users:
                    _LOG.info("[load] %s: %d user(s)", server.name, count)
                    metrics.setdefault(server.name, {})[count] = locust_run(index_url, count, Path(scratch))
    names = [server.name for server in servers]
    baseline = names.index("direct") if "direct" in names else 0
    rows = []
    for count in users:
        label = f"{count} user" + ("s" if count > 1 else "")
        rps = [metrics[server.name][count]["rps"] for server in servers]
        p95 = [metrics[server.name][count]["p95"] / 1000 for server in servers]
        rows.extend((
            {
                "name": f"{label}: requests/s",
                "cells": tinted_cells(rps, baseline=baseline, higher_is_better=True, unit="req/s"),
            },
            {"name": f"{label}: p95 latency", "cells": tinted_cells(p95, baseline=baseline)},
        ))
    write_feed(
        "load",
        {
            "label": "simple-page requests against a warm cache",
            "baseline": names[baseline],
            "parties": [{"name": server.name, "url": server.homepage} for server in servers],
            "rows": rows,
        },
    )


def warm_pages(index_url: str) -> None:
    for package in TOP_PACKAGES[:10]:
        request = urllib.request.Request(f"{index_url}{package}/", headers={"Accept": "*/*"})
        with urllib.request.urlopen(request, timeout=120):
            pass


def locust_run(index_url: str, users: int, scratch: Path) -> dict[str, float]:
    csv_prefix = scratch / f"locust-{users}"
    command = [
        "uvx",
        "locust",
        "-f",
        str(Path(__file__).parent / "locustfile.py"),
        "--headless",
        "--users",
        str(users),
        "--spawn-rate",
        str(users),
        "--run-time",
        "20s",
        "--csv",
        str(csv_prefix),
    ]
    env = {**os.environ, "BENCH_SIMPLE_URL": index_url, "BENCH_PACKAGES": ",".join(TOP_PACKAGES[:10])}
    subprocess.run(command, check=True, env=env, capture_output=True)
    header, *entries = (csv_prefix.parent / f"{csv_prefix.name}_stats.csv").read_text().splitlines()
    names = [name.strip('"') for name in header.split(",")]
    # The aggregate over every request name is the last row locust writes.
    columns = [cell.strip('"') for cell in entries[-1].split(",")]
    return {
        "rps": float(columns[names.index("Requests/s")]),
        "p95": float(columns[names.index("95%")]),
    }


def tinted_cells(
    values: list[float], *, baseline: int = 0, higher_is_better: bool = False, unit: str = "s"
) -> list[dict[str, str]]:
    """Format one row: readable value, ratio against the baseline party, and a best-to-worst tint.

    The baseline is the no-proxy `direct` measurement where present, so every other cell reads as
    the overhead (or win) a server adds on top of talking to the upstream itself.

    Returns:
        One cell dict (`text`, `ratio`, `tint`) per value, in input order.
    """
    reference = values[baseline]
    costs = [1.0 / value if higher_is_better else value for value in values]
    best = min(costs)
    span = max(math.log(max(costs) / best), MIN_SPAN)
    cells = []
    for value, cost in zip(values, costs, strict=True):
        position = math.log(cost / best) / span
        cells.append({
            "text": _format_seconds(value) if unit == "s" else f"{value:,.0f} {unit}",
            "ratio": f"{value / reference:.2f}x",
            "tint": LADDER[min(int(position * len(LADDER)), len(LADDER) - 1)],
        })
    return cells


def _format_seconds(seconds: float) -> str:
    if seconds >= 60:
        return f"{int(seconds // 60)}m {seconds % 60:04.1f}s"
    if seconds >= 1:
        return f"{seconds:.1f} s"
    return f"{seconds * 1000:.0f} ms"


def write_feed(name: str, feed: dict[str, object]) -> None:
    FEEDS.mkdir(parents=True, exist_ok=True)
    path = FEEDS / f"{name}.json"
    path.write_text(json.dumps(feed, indent=2) + "\n")
    _LOG.info("wrote %s", path)


if __name__ == "__main__":
    main()
