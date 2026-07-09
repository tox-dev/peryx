+++
title = "Performance and methodology"
description = "Why a peryx cache keeps up with the upstream on a miss and pulls ahead when warm, how it is measured, and the per-operation cost of each ecosystem's driver."
weight = 2
+++

peryx is fast for the same reasons whichever ecosystem it serves: it never buffers a whole response, concurrent misses
for one thing share a single upstream fetch, and everything it has served once is content-addressed on local disk. This
page explains those mechanisms and how they are measured. The head-to-head numbers against the tools you would run
instead live per ecosystem: [PyPI](@/ecosystems/pypi/performance.md) against devpi, proxpi, pypiserver, and pypicloud;
[OCI](@/ecosystems/oci/performance.md) against the distribution reference registry and zot.

The headline both pages share: a **cold** request through peryx costs about what going straight to the upstream costs,
and a **warm** one is bounded by the client's own work, not the network.

## Why the cold path keeps up with the upstream

A proxy that downloads a thing, stores it, and only then serves it roughly doubles time-to-first-byte on every miss.
peryx [streams instead](@/core/architecture.md): bytes are transformed and forwarded chunk by chunk as they arrive,
artifact bytes are teed to the client and the store at once, and hash verification and durable writes happen after the
client already has its last byte. A wheel and an image layer take the same path: the client waits for upstream wire time
plus one hop, not for a second copy to land on disk first. What remains on top of raw wire time is connection setup,
softened by warming upstream connections at startup, and single-digit milliseconds of transformation.

## What a warm cache is worth

Warm numbers on loopback measure overhead, not value; the value shows up when the alternative is a real network. Three
effects compound:

- **Bytes stop repeating.** The store is content-addressed, so the 47 MB wheel or the base image layer that four CI
  jobs, two Docker builds, and a laptop all need crosses your uplink once and is stored once, however many projects or
  images reference it.
- **A concurrent burst costs one fetch.** When several clients miss the same uncached thing at once (a page, a wheel, a
  layer), peryx's single-flight collapses them into one upstream transfer that every waiter tails, so a ten-job CI fleet
  reaching for a fresh release does not become ten upstream downloads.
- **Latency stops stacking.** A resolve-install or pull cycle is a chain of dependent requests; moving them from
  cross-continent round-trips to your LAN shortens every link in the chain. For PyPI,
  [PEP 658](https://peps.python.org/pep-0658/) metadata makes the chain cheaper still: a resolver reads kilobytes of
  dependency metadata instead of whole wheels.

A laptop next to its cache is the *least* favorable setup for these numbers: the farther your machines sit from the
upstream, the more the warm path wins, because it replaces your worst network hop instead of a loopback.

## How the numbers are measured

Every workload measures each server over several independent rounds. A round restarts the server on an empty state
directory, so a cold pass is genuinely cold each time and the round-to-round spread captures the between-launch variance
(page cache, allocator layout, CPU frequency) that repeating inside one process would hide. The rounds reduce to a
**median**, which unlike a best-of-N minimum does not drift lower as you add rounds, so two runs of different lengths
stay comparable.

Each cell prints the median with **± its coefficient of variation**, and a cell whose spread is wide enough to rival the
differences being compared is flagged: a number you cannot trust to a few percent is marked rather than read as fact. A
cold row that fetches from the real upstream is labelled `net`. Its time is dominated by CDN and network variance peryx
does not control, so it is shown for context but never used to decide whether a change is a regression.

The request-load latency is measured **open-loop**: each client fires on a fixed schedule instead of waiting for the
previous response, so a stall is charged the full delay since its intended send time. A closed-loop client would stop
issuing exactly the requests a stall delays and never see the tail, understating p99 by orders of magnitude (the
[coordinated-omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/) problem). Latencies land in an
[HdrHistogram](https://github.com/HdrHistogram/HdrHistogram) so the reported percentile is exact.

The suite runs two ways. The tables here are **peryx against the other tools** (`cargo run --release -p peryx-bench`).
To check a change against an earlier build, **peryx against itself at a base commit** builds both revisions, measures
each through this same harness so the method matches on both sides, and prints a per-metric verdict aggregated with the
geometric mean, gating only the local metrics:

```shell
cargo run --release -p peryx-bench -- ab <base-commit>
```

## Per-operation cost, by ecosystem

The competitor tables time a whole workload against a real network. A second set of benchmarks prices peryx against
itself: each row below is one request served in process through the full router, with no socket and no upstream, from a
warm store. They come from the criterion suites the repository carries per ecosystem, and they answer the narrower
question the workload tables cannot isolate: once the bytes are local, what does peryx spend to turn a request into a
response?

PyPI, serving a cached project from the store:

| Operation                           | Cost    |
| ----------------------------------- | ------- |
| Simple index page, JSON (PEP 691)   | 2.3 µs  |
| Simple index page, HTML (PEP 503)   | 0.37 ms |
| project detail, legacy JSON         | 1.2 ms  |
| parse an upstream JSON page (numpy) | 0.16 ms |

```shell
cargo bench -p peryx-ecosystem-pypi --bench operations
```

OCI (Docker), serving a hosted registry:

| Operation                      | Cost    |
| ------------------------------ | ------- |
| `GET /v2/` version check       | 0.73 µs |
| manifest by digest (store hit) | 1.7 µs  |
| tag list                       | 1.8 µs  |
| blob fetch (4 KB, streamed)    | 35 µs   |

```shell
cargo bench -p peryx-ecosystem-oci --bench operations
```

The two read paths differ in kind, which the numbers show. A container manifest and blob are content-addressed bytes
peryx returns as stored, so a pull is a store lookup and a stream: single-digit microseconds for the manifest and tag
metadata, tens for a small blob whose cost is the file read. peryx caches a PyPI Simple page in the modern JSON encoding
after its first transform and serves it as a memory copy at the same few microseconds; the HTML and legacy-JSON rows are
the one-time cost of rendering an alternate representation of that page on request, not a cache miss. Neither read path
touches the network once the store is warm.

## In practice

- The head-to-head numbers: [PyPI performance](@/ecosystems/pypi/performance.md),
  [OCI performance](@/ecosystems/oci/performance.md)
- Put the cache in front of CI: [the CI guide](@/ecosystems/pypi/guides/ci-cache.md)
- Watch hit rates and bytes served: [monitoring](@/core/monitor.md)
