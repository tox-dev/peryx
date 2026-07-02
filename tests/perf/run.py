# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Benchmark velodex against direct PyPI and competing index servers.

Four workloads:

- **install**: time ``uv pip install`` and ``pip install`` of the top PyPI packages through each
  server, cold (fresh server state) and warm (the server keeps its cache, the client starts
  over). This is the number a user feels.
- **throughput**: move one large wheel; four clients racing for it cold, then single and
  eight-way parallel downloads of it hot.
- **parallel installs**: ten venvs install polars at once with separate client caches, like ten
  CI jobs hitting the same server, cold and warm.
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
import concurrent.futures
import json
import logging
import math
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
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
# The stress wheel is torch, the largest of the top packages; the fleet package is polars, a
# heavy single-wheel install a CI fleet grabs over and over.
STRESS_PROJECT: Final = "torch"
FLEET_PACKAGE: Final = "polars"


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
    parser.add_argument("--skip-throughput", action="store_true", help="skip the file throughput workload")
    parser.add_argument("--skip-parallel", action="store_true", help="skip the parallel-CI install workload")
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
    if not arguments.skip_throughput:
        bench_throughput(servers)
    if not arguments.skip_parallel:
        bench_parallel_installs(servers)
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


def bench_throughput(servers: list[Server]) -> None:
    """The file-transfer workload: one large wheel, cold under contention and hot at full speed.

    The cold row sends four clients after the same uncached wheel at once, which is what a CI fleet
    does to a cache the moment a new release lands: it measures whether the server fans one
    upstream transfer out to every waiter or serializes them. The hot rows measure how fast a
    cached wheel leaves the server, alone and under eight parallel readers.
    """
    filename = stress_wheel_filename()
    _LOG.info("[throughput] measuring with %s", filename)
    results: dict[str, dict[str, float] | None] = {}
    for server in servers:
        with tempfile.TemporaryDirectory(prefix=f"tput-{server.name}-") as scratch:
            state = Path(scratch) / "state"
            state.mkdir()
            with running(server, state) as index_url:
                # A server erroring under contention is itself a result worth a table cell.
                try:
                    url = wheel_url(index_url, STRESS_PROJECT, filename)
                    cold_wall = parallel_downloads(url, clients=4)
                    # Hot transfers are sub-second syscall benchmarks; the best of three smooths
                    # scheduler noise. The cold transfer cannot repeat without resetting state.
                    single_seconds, size = min(timed_download(url) for _ in range(3))
                    hot_wall = min(parallel_downloads(url, clients=8) for _ in range(3))
                except urllib.error.URLError as error:
                    _LOG.info("[throughput] %s: failed under contention (%s)", server.name, error)
                    results[server.name] = None
                    continue
                results[server.name] = {
                    "cold4": cold_wall,
                    "hot1": size / single_seconds / 1e6,
                    "hot8": 8 * size / hot_wall / 1e6,
                }
                _LOG.info(
                    "[throughput] %s: cold-4 %.1fs, hot %.0f MB/s, hot-8 %.0f MB/s",
                    server.name,
                    cold_wall,
                    results[server.name]["hot1"],
                    results[server.name]["hot8"],
                )
    names = [server.name for server in servers]
    baseline = names.index("direct") if "direct" in names else 0

    def column(key: str) -> list[float | None]:
        return [(cells := results[name]) and cells[key] for name in names]

    rows = [
        {
            "name": "cold cache: 4 clients, one wheel",
            "cells": tinted_cells(column("cold4"), baseline=baseline),
        },
        {
            "name": "hot cache: single download",
            "cells": tinted_cells(column("hot1"), baseline=baseline, higher_is_better=True, unit="MB/s"),
        },
        {
            "name": "hot cache: 8 parallel downloads",
            "cells": tinted_cells(column("hot8"), baseline=baseline, higher_is_better=True, unit="MB/s"),
        },
    ]
    write_feed(
        "throughput",
        {
            "label": f"moving one large wheel ({STRESS_PROJECT}): cold under contention, hot at speed",
            "baseline": names[baseline],
            "parties": [{"name": server.name, "url": server.homepage} for server in servers],
            "rows": rows,
        },
    )


def stress_wheel_filename() -> str:
    """The concrete wheel every server moves, resolved once from PyPI so all parties match.

    Returns:
        The newest stress-project wheel filename for this host's platform.
    """
    request = urllib.request.Request(
        f"https://pypi.org/simple/{STRESS_PROJECT}/",
        headers={"Accept": "application/vnd.pypi.simple.v1+json"},
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        files = json.load(response)["files"]
    tags = ("macosx", "arm64") if sys.platform == "darwin" else ("manylinux", "x86_64")
    matches = [file["filename"] for file in files if all(tag in file["filename"] for tag in tags)]
    return matches[-1]


def wheel_url(index_url: str, project: str, filename: str) -> str:
    """Resolve `filename`'s download URL through a server's simple page, JSON or HTML alike.

    Returns:
        The absolute download URL the server serves that wheel under.
    """
    page_url = f"{index_url}{project}/"
    request = urllib.request.Request(
        page_url, headers={"Accept": "application/vnd.pypi.simple.v1+json, text/html;q=0.5"}
    )
    with urllib.request.urlopen(request, timeout=300) as response:
        content_type = response.headers.get_content_type()
        body = response.read().decode(errors="replace")
        page_url = response.geturl()
    if content_type == "application/vnd.pypi.simple.v1+json":
        href = next(file["url"] for file in json.loads(body)["files"] if file["filename"] == filename)
    else:
        href = next(match for match in re.findall(r'href="([^"]+)"', body) if filename in match)
    return urllib.parse.urljoin(page_url, href.partition("#")[0])


def timed_download(url: str) -> tuple[float, int]:
    """One full download.

    Returns:
        Wall seconds and byte count.
    """
    start = time.monotonic()
    total = 0
    with urllib.request.urlopen(urllib.request.Request(url, headers={"Accept": "*/*"}), timeout=600) as response:
        while chunk := response.read(1 << 20):
            total += len(chunk)
    return time.monotonic() - start, total


def parallel_downloads(url: str, *, clients: int) -> float:
    """`clients` simultaneous downloads of the same URL.

    Returns:
        Wall seconds until every download finishes.
    """
    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as pool:
        start = time.monotonic()
        futures = [pool.submit(timed_download, url) for _ in range(clients)]
        for future in futures:
            future.result()
    return time.monotonic() - start


def bench_parallel_installs(servers: list[Server]) -> None:
    """The CI-fleet workload: ten venvs install polars at once, cold then warm.

    Each worker gets its own empty uv cache, exactly like ten CI jobs landing on the same runner
    pool: the server sees ten simultaneous copies of every page and wheel request.
    """
    results: dict[str, dict[str, float] | None] = {}
    for server in servers:
        with tempfile.TemporaryDirectory(prefix=f"fleet-{server.name}-") as scratch:
            state = Path(scratch) / "state"
            state.mkdir()
            with running(server, state) as index_url:
                # A server erroring under the fleet is itself a result worth a table cell.
                try:
                    cold = fleet_install(index_url, Path(scratch), workers=10)
                    warm = fleet_install(index_url, Path(scratch), workers=10)
                except RuntimeError as error:
                    _LOG.info("[fleet] %s: failed under the fleet (%s)", server.name, error)
                    results[server.name] = None
                    continue
                results[server.name] = {"cold": cold, "warm": warm}
                _LOG.info("[fleet] %s: cold %.1fs warm %.1fs", server.name, cold, warm)
    names = [server.name for server in servers]
    baseline = names.index("direct") if "direct" in names else 0
    rows = [
        {
            "name": f"{phase} cache: 10 parallel installs",
            "cells": tinted_cells([(cells := results[name]) and cells[phase] for name in names], baseline=baseline),
        }
        for phase in ("cold", "warm")
    ]
    write_feed(
        "parallel-install",
        {
            "label": f"uv: ten venvs install {FLEET_PACKAGE} at once",
            "baseline": names[baseline],
            "parties": [{"name": server.name, "url": server.homepage} for server in servers],
            "rows": rows,
        },
    )


def fleet_install(index_url: str, scratch: Path, *, workers: int) -> float:
    """Install the fleet package into `workers` fresh venvs at once.

    Returns:
        Wall seconds until every install finishes.
    """
    with tempfile.TemporaryDirectory(prefix="fleet-run-", dir=scratch) as rundir:
        venvs = [Path(rundir) / f"venv-{index}" for index in range(workers)]
        for venv in venvs:
            subprocess.run(["uv", "venv", str(venv)], check=True, capture_output=True)

        def one(venv: Path) -> None:
            env = {**os.environ, "VIRTUAL_ENV": str(venv), "UV_CACHE_DIR": f"{venv}-cache"}
            result = subprocess.run(
                ["uv", "pip", "install", "--index-url", index_url, FLEET_PACKAGE],
                env=env,
                capture_output=True,
                check=False,
            )
            if result.returncode != 0:
                msg = f"fleet install via {index_url} failed:\n{result.stderr.decode()[-2000:]}"
                raise RuntimeError(msg)

        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            start = time.monotonic()
            futures = [pool.submit(one, venv) for venv in venvs]
            for future in futures:
                future.result()
        return time.monotonic() - start


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
    values: list[float | None], *, baseline: int = 0, higher_is_better: bool = False, unit: str = "s"
) -> list[dict[str, str]]:
    """Format one row: readable value, ratio against the baseline party, and a best-to-worst tint.

    The baseline is the no-proxy `direct` measurement where present, so every other cell reads as
    the overhead (or win) a server adds on top of talking to the upstream itself. A `None` marks a
    party that errored on the workload; it renders as a red `error` cell.

    Returns:
        One cell dict (`text`, `ratio`, `tint`) per value, in input order.
    """
    reference = values[baseline]
    assert reference is not None, "the baseline party never errors"
    costs = [None if value is None else (1.0 / value if higher_is_better else value) for value in values]
    finite = [cost for cost in costs if cost is not None]
    best = min(finite)
    span = max(math.log(max(finite) / best), MIN_SPAN)
    cells = []
    for value, cost in zip(values, costs, strict=True):
        if value is None or cost is None:
            cells.append({"text": "error", "ratio": "n/a", "tint": "worst"})
            continue
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
