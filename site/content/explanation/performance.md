+++
title = "Performance and methodology"
description = "Measured numbers for cold and warm installs, the commands that produced them, and where the remaining time goes."
weight = 2
+++

Claims about speed are worthless without the commands behind them, so here are both. The headline: a **cold**
install through velodex costs about what going straight to pypi.org costs, and a **warm** one is bounded by the
installer's own CPU, not the network.

## The measurement

The workload installs pandas and polars (six wheels, about 64 MB, including one 47 MB wheel) into a fresh
virtualenv with a fresh installer cache, so every byte must come through the index:

```shell
uv venv fresh-venv
env VIRTUAL_ENV=$PWD/fresh-venv UV_CACHE_DIR=$PWD/fresh-cache \
    UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/ \
    uv pip install pandas polars
```

Setup: velodex release build and the client on the same Apple Silicon laptop, roughly 700 Mbit/s to PyPI's CDN.
Five runs per scenario; "cold" deletes velodex's data directory first, "warm" keeps it and only resets the client.

| Scenario | Wall time | What dominates |
| ---------------------------- | ------------ | ------------------------------------------------- |
| uv direct to pypi.org | 0.94–1.03 s | the network, end to end |
| through velodex, cold cache | 1.13–1.38 s | the network; velodex adds ~0.1–0.3 s |
| through velodex, warm cache | 0.66–0.71 s | uv itself (0.76 s of CPU unzipping and installing) |

Per-request server timings from the warm runs: simple pages and cached wheels serve in 0 ms; the largest page in
the set (numpy's, 2.6 MB of JSON) transforms in under 30 ms on its first warm hit and is a memory copy afterwards.

The run-to-run spread on the cold numbers is the CDN, not velodex: the same 47 MB wheel arrived in anything from
0.7 to 1.3 s across runs. And a laptop next to its cache is the *least* favorable setup for the warm numbers:
the farther your machines sit from PyPI (CI in a private subnet, an office behind one uplink), the more the warm
path wins, because it replaces your worst network hop instead of a loopback.

## Why the cold path keeps up with the CDN

A proxy that downloads, stores, and then serves would roughly double time-to-first-byte on every miss. velodex
[streams instead](@/explanation/architecture.md): page bytes are transformed and forwarded chunk by chunk as they
arrive, artifact bytes are teed to the client and the store simultaneously, hash verification and durable writes
happen after the client's last byte, and concurrent misses for the same thing share one upstream fetch. What
remains on top of raw wire time is connection setup, softened by warming upstream connections at startup, and
single-digit milliseconds of transformation.

## What "warm" is worth

Warm numbers on loopback measure overhead, not value; the value shows up when the alternative is a real network.
Three effects compound:

- **Bytes stop repeating.** The store is content-addressed, so the 47 MB wheel that four CI jobs, two Docker
  builds, and a laptop all need crosses your uplink once.
- **Resolution stops downloading wheels.** With [PEP 658](https://peps.python.org/pep-0658/) metadata cached,
  a resolver examining ten candidate versions fetches kilobytes, not gigabytes.
- **Latency stops stacking.** A resolve-install cycle is a chain of dependent requests; moving them from
  cross-continent RTTs to your LAN shortens every link in the chain.

## The benchmark suite

The tables below come from a [benchmark harness](https://github.com/tox-dev/velodex/tree/main/tests/perf) the
repository carries: it builds velodex, starts every competitor from its published package, times the same
workload through each, and writes the feeds these tables render from. Cells tint from best-in-row green to
worst-in-row red; the ratio in parentheses compares against **direct**, the no-proxy baseline, so each server's
cell reads as the overhead (or win) it adds over talking to pypi.org yourself.

The table covers every alternative that can be started hermetically from a published package: velodex, devpi,
proxpi, pypiserver (whose upstream fallback is a redirect rather than a cache), and pypicloud (archived upstream;
it still runs, but only under Python 3.10 with SQLAlchemy pinned below 2). Pulp needs PostgreSQL plus four
services, nginx_pypi_cache is a Docker configuration rather than a package, and Artifactory, Nexus, and the cloud
registries need licenses or accounts, so none of them can be measured this way.

The install workload is the top 51 most-downloaded PyPI packages
([the snapshot](https://github.com/tox-dev/velodex/blob/main/tests/perf/packages.py), torch included for one
large wheel), installed with uv into a fresh virtualenv
with a fresh client cache. **Cold** is the first install against a server with empty state; **warm** reruns it
with the server's cache full and only the client reset.

{{ bench(file="install-uv") }}

The same workload through pip tells a different story: pip installs serially and does its own work between
requests, so the client dominates and every server lands within a few seconds of the rest. A faster index cannot
rescue a slow client; through uv, the index is what you feel.

{{ bench(file="install-pip") }}

The throughput workload moves one large wheel (torch, ~88 MB). The cold row is the moment a CI fleet fears: four
clients ask for the same wheel the instant a release lands, and the server either fans one upstream transfer out
to every waiter or serializes them. velodex runs the transfer as a detached task every client tails, so all four
see their first byte in milliseconds and finish together in the time one download takes; pypicloud answers the
same burst with HTTP 500. The hot rows measure how fast a cached wheel leaves the server, alone and under eight
parallel readers. Every number past ~3 GB/s outruns a 25 GbE link, so those cells compare server efficiency, not
anything a client on a network would feel.

{{ bench(file="throughput") }}

The parallel-install workload is that fleet end to end: ten virtualenvs install polars at once, each with its own
empty client cache, exactly like ten CI jobs landing on the same runner pool. The server sees ten simultaneous
copies of every page and wheel request. This is where correctness under concurrency shows up next to speed:
devpi fails eight of the ten cold installs, because concurrent requests for a project it is fetching for the
first time see an empty page and uv concludes the package does not exist.

{{ bench(file="parallel-install") }}

The request workload drives locust against each warm server: one user, then a swarm of 32, fetching project pages
the way a resolver does.

{{ bench(file="load") }}

Every server is measured the same way, on the same machine, in the same run, and one command reproduces all
five tables:

```shell
uv run tests/perf/run.py
```

## Reproducing

Everything above reproduces with the repository checked out:

```shell
cargo build --release
./target/release/velodex serve &
# cold: rm -rf velodex-data between runs; warm: leave it
time env VIRTUAL_ENV=… UV_CACHE_DIR=… UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/ \
    uv pip install pandas polars
```

If your numbers disagree with ours, we want to know: [open an issue](https://github.com/tox-dev/velodex/issues).

## In practice

- Put the cache in front of CI: [the CI guide](@/guides/ci-cache.md)
- Watch hit rates and bytes served: [monitoring](@/guides/monitor.md)
