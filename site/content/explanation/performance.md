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

| Scenario                     | Wall time    | What dominates                                    |
| ---------------------------- | ------------ | ------------------------------------------------- |
| uv direct to pypi.org        | 0.94–1.03 s  | the network, end to end                           |
| through velodex, cold cache  | 1.13–1.38 s  | the network; velodex adds ~0.1–0.3 s              |
| through velodex, warm cache  | 0.66–0.71 s  | uv itself (0.76 s of CPU unzipping and installing) |

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

The repository carries the harness that produced the tables below: `tests/perf/run.py` starts velodex and its
competitors from their published packages, times the same workload through each, and writes the JSON feeds these
tables render from. Cells tint from best-in-row green to worst-in-row red; the ratio in parentheses compares
against velodex.

The install workload is the top 51 most-downloaded PyPI packages (snapshot in
`tests/perf/packages.py`, torch included for one genuinely large wheel), installed with uv into a fresh virtualenv
with a fresh client cache. **Cold** is the first install against a server with empty state; **warm** reruns it
with the server's cache full and only the client reset.

{{ bench(file="install-uv") }}

The same workload through pip tells a different story: pip installs serially and does its own work between
requests, so the client dominates and every server lands within a few seconds of the rest. A faster index cannot
rescue a slow client; through uv, the index is what you feel.

{{ bench(file="install-pip") }}

The request workload drives locust against each warm server: one user, then a swarm of 32, fetching project pages
the way a resolver does.

{{ bench(file="load") }}

Single-process Python servers and velodex are all measured the same way, on the same machine, in the same run;
`uv run tests/perf/run.py` reproduces the install table and `--load` the request table.

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
