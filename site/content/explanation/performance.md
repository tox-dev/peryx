+++
title = "Performance and methodology"
description = "Measured numbers for cold and warm installs, the commands that produced them, and where the remaining time goes."
weight = 2
+++

Claims about speed are worthless without the commands behind them, so here are both. The headline: a **cold** install
through velodex costs about what going straight to pypi.org costs, and a **warm** one is bounded by the installer's own
CPU, not the network.

## The measurement

The workload installs pandas and polars (six wheels, about 64 MB, including one 47 MB wheel) into a fresh virtualenv
with a fresh installer cache, so every byte must come through the index:

```shell
uv venv fresh-venv
env VIRTUAL_ENV=$PWD/fresh-venv UV_CACHE_DIR=$PWD/fresh-cache \
    UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/ \
    uv pip install pandas polars
```

Setup: velodex release build and the client on the same Apple Silicon laptop, on a 1 Gbit symmetric fiber connection in
Los Angeles. Five runs per scenario; "cold" deletes velodex's data directory first, "warm" keeps it and only resets the
client.

| Scenario                    | Wall time   | What dominates                                     |
| --------------------------- | ----------- | -------------------------------------------------- |
| uv direct to pypi.org       | 0.94–1.03 s | the network, end to end                            |
| through velodex, cold cache | 1.13–1.38 s | the network; velodex adds ~0.1–0.3 s               |
| through velodex, warm cache | 0.66–0.71 s | uv itself (0.76 s of CPU unzipping and installing) |

Per-request server timings from the warm runs: simple pages and cached wheels serve in 0 ms; the largest page in the set
(numpy's, 2.6 MB of JSON) transforms in under 30 ms on its first warm hit and is a memory copy afterwards.

The run-to-run spread on the cold numbers is the CDN, not velodex: the same 47 MB wheel arrived in anything from 0.7 to
1.3 s across runs. And a laptop next to its cache is the *least* favorable setup for the warm numbers: the farther your
machines sit from PyPI (CI in a private subnet, an office behind one uplink), the more the warm path wins, because it
replaces your worst network hop instead of a loopback.

## Why the cold path keeps up with the CDN

A proxy that downloads, stores, and then serves would roughly double time-to-first-byte on every miss. velodex
[streams instead](@/explanation/architecture.md): page bytes are transformed and forwarded chunk by chunk as they
arrive, artifact bytes are teed to the client and the store simultaneously, hash verification and durable writes happen
after the client's last byte, and concurrent misses for the same thing share one upstream fetch. What remains on top of
raw wire time is connection setup, softened by warming upstream connections at startup, and single-digit milliseconds of
transformation.

## What "warm" is worth

Warm numbers on loopback measure overhead, not value; the value shows up when the alternative is a real network. Three
effects compound:

- **Bytes stop repeating.** The store is content-addressed, so the 47 MB wheel that four CI jobs, two Docker builds, and
  a laptop all need crosses your uplink once.
- **Resolution stops downloading wheels.** With [PEP 658](https://peps.python.org/pep-0658/) metadata cached, a resolver
  examining ten candidate versions fetches kilobytes, not gigabytes.
- **Latency stops stacking.** A resolve-install cycle is a chain of dependent requests; moving them from cross-continent
  RTTs to your LAN shortens every link in the chain.

## The benchmark suite

The tables below come from a [benchmark harness](https://github.com/tox-dev/velodex/tree/main/crates/velodex-bench) the
repository carries as a Rust crate: it builds velodex, starts every competitor from its published package, times the
same workload through each with a native HTTP client, samples each server's process tree while its workload runs, and
writes one TOML report these tables render from. Every cell is five samples with the high and low dropped and the rest
averaged, so one slow run cannot move it; the small ± beside a cell is the run-to-run coefficient of variation across
those samples, so you can see how settled each figure is. Servers are measured round by round in rotating order rather
than one after another, so a drift in the network or the laptop's temperature spreads across every party instead of
landing on whoever came last. Cells tint from best-in-row green to worst-in-row red; the ratio in parentheses compares
against **direct**, the no-proxy baseline, so each server's cell reads as the overhead (or win) it adds over talking to
pypi.org yourself.

{{ benchmeta() }}

The numbers come from a laptop on a home connection, not a controlled lab, so the absolute figures move with the network
and thermal state. The ratios against **direct** are what to read: every server meets the same conditions in the same
run, so the cells that lean on the upstream CDN (cold installs, transferring an uncached wheel) shift together and
cancel in the ratio, while the cells served from velodex's own memory or disk (warm pages, hot wheels, peak request
throughput) stay put.

The table covers every alternative that can be started hermetically from a published package: velodex, devpi, proxpi,
pypiserver (whose upstream fallback is a redirect rather than a cache), and pypicloud (archived upstream; it still runs,
but only under Python 3.10 with SQLAlchemy pinned below 2). Pulp needs PostgreSQL plus four services, nginx_pypi_cache
is a Docker configuration rather than a package, and Artifactory, Nexus, and the cloud registries need licenses or
accounts, so none of them can be measured this way.

The install workload is the top 51 most-downloaded PyPI packages
([the snapshot](https://github.com/tox-dev/velodex/blob/main/crates/velodex-bench/src/packages.rs), torch included for
one large wheel), installed with uv into a fresh virtualenv with a fresh client cache. **Cold** is the first install
against a server with empty state; **warm** reruns it with the server's cache full and only the client reset.

{{ bench(file="install-uv") }}

The same workload through pip tells a different story: pip installs serially and does its own work between requests, so
the client dominates and every server lands within a few seconds of the rest. A faster index cannot rescue a slow
client; through uv, the index is what you feel.

{{ bench(file="install-pip") }}

The throughput workload moves one large wheel (torch, ~88 MB). The cold row is the moment a CI fleet fears: four clients
ask for the same wheel the instant a release lands, and the server either fans one upstream transfer out to every waiter
or serializes them. velodex runs the transfer as a detached task every client tails, so all four see their first byte in
milliseconds and finish together in the time one download takes; pypicloud answers the same burst with HTTP 500. The hot
rows measure how fast a cached wheel leaves the server, alone and under eight parallel readers. Every number past ~3
GB/s outruns a 25 GbE link, so those cells compare server efficiency, not anything a client on a network would feel.

{{ bench(file="throughput") }}

The parallel-install workload is that fleet end to end: ten virtualenvs install polars at once, each with its own empty
client cache, exactly like ten CI jobs landing on the same runner pool. The server sees ten simultaneous copies of every
page and wheel request. This is where correctness under concurrency shows up next to speed: devpi fails eight of the ten
cold installs, because concurrent requests for a project it is fetching for the first time see an empty page and uv
concludes the package does not exist.

{{ bench(file="parallel-install") }}

The request workload measures the ceiling: how much traffic each warm server sustains under a real resolver. The client
keeps connections alive and follows redirects, the way uv and pip do, so its numbers are the ones a resolver would see.
It ramps concurrency, 1, 2, 4, 8, and on up to 64, and at each step pushes project-page requests as fast as the last
returns, recording both the request rate and the megabytes of page delivered. A server's highest rate is its peak; the
table shows that rate, the data served there, the p95/p99 latency, and the connection count it took. The ramp rides
through run-to-run noise and stops only when a server starts erroring or its throughput collapses, so a fragile server
settles at the load it holds rather than the crash a heavier pool would force. The connections-at-peak column reads how
far each server scales before more connections stop paying off: velodex saturates the client at 16, while the slower
servers need more open connections to reach a lower ceiling.

velodex leads both rates, and by a wide margin: 2,459 requests a second and 1,076 MB of page delivered, against 901 and
428 for the next server. It serves every page from its own store over the loopback, so neither number touches the
network. pypiserver comes second only because it does no serving at all: it answers each request with a redirect to
pypi.org, so the client fetches the page from PyPI's CDN, and its throughput is whatever the uplink allows. On this
machine that is a 1 Gbit link, and its 428 MB/s of delivered pages fills that link once you unpack the gzip, the same
wire limit velodex never meets. Its CPU per thousand requests, a rounding error next to velodex's, is the tell: it
caches nothing, saves no bandwidth, and works only while PyPI is reachable and close. Point the same benchmark at a
slower or more distant link and pypiserver's lead over devpi and pypicloud shrinks while velodex holds, because velodex
runs at local speed and the redirect can only go as fast as the wire. The price velodex pays for serving real pages is real
CPU and memory, more than a server that only forwards; the resource rows show it. devpi is the cautionary case: its peak
lands at 48 connections, but that only means its best rate, a slow 68 a second at nearly a second of tail latency,
happened there rather than lower. direct sits this one out: it is pypi.org itself, so its ceiling would just measure the
uplink again. Ratios read against velodex, since direct is absent from this table.

{{ bench(file="load") }}

Every table ends with resource rows: the CPU and peak resident memory of the server's whole process tree while its
workload ran, anchored to velodex (direct runs no server, so it cannot anchor them). The request table divides CPU by
requests answered, because a server that redirects everything spends almost nothing while doing almost nothing — raw
seconds would reward that. Peak memory is sampled resident size every 200 ms, which counts pages shared across a process
tree once per process and can miss a spike between samples; read it as an order-of-magnitude comparison, not a
byte-exact one. Speed alone hides a trade either way: proxpi's hot-transfer lead comes from holding wheels in memory at
several times velodex's footprint, and pypiserver's tiny CPU reflects that it redirects downloads to PyPI instead of
serving them.

Every server is measured the same way, on the same machine, in the same run, and one command reproduces all five tables:

```shell
cargo run --release -p velodex-bench
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
