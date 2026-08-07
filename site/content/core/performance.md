+++
title = "Performance methodology"
description = "Shared cache mechanisms, benchmark controls, and interpretation rules for ecosystem measurements."
weight = 2
+++

peryx uses the same cache path across ecosystem implementations. A miss streams bytes from the upstream to the client
and content store. Concurrent requests for one missing object share that fetch. A warm read serves content-addressed
bytes from local storage.

## Cold path

The server forwards chunks as they arrive and writes the same chunks to storage. It verifies the digest before it makes
new metadata visible. The client pays upstream transfer time, one proxy hop, and stream processing; it does not wait for
a second full copy into storage.

## Warm path

Content addressing stores one copy of equal bytes referenced by multiple entities. Single-flight fetches prevent a
client burst from multiplying an upstream transfer. Local placement also removes wide-area round trips from dependent
metadata and artifact requests.

## Benchmark controls

Each workload runs in independent rounds. A round starts the server with empty state, measures the cold operation, then
measures the warm operation against the populated cache. Process groups and client caches reset between rounds.

Tables report the median and coefficient of variation. Network-bound rows provide context but do not gate regressions.
Local workloads compare revisions on the same host and power state.

Request-load tests use an open-loop schedule. Each request keeps its intended send time, so a stall contributes its full
delay to tail latency. A coordinated-omission-safe histogram records percentiles.

The harness measures wall time, throughput, latency, process-tree CPU, and peak resident memory. It records storage,
memory, and loopback baselines from the same machine. Those baselines catch client subprocess or filesystem limits that
could otherwise look like server behavior.

## The machine these numbers come from

The benchmark run records the host beside its result tables. Each baseline discards a warm-up sample and reports the
median and spread of five samples. The harness places server state, client caches, and scratch files on the system
temporary volume, separate from the repository checkout.

{{ machine() }}

Memory, disk, and minimal HTTP rows provide scales for the serving results. The minimal server measures socket and
system-call cost. Disk results identify storage limits, but warm artifact reads can come from the operating system page
cache.

## Reading results

Cold results include upstream and network variance. Warm results isolate local serving and client work. A complete
client operation can hide server differences behind resolution, decompression, verification, or installation, so each
implementation also measures protocol endpoints and large-artifact transfer.

Throughput above the deployment network rate compares host efficiency rather than user-visible speed. CPU must be
normalized by completed work. Failed operations do not count as throughput.

## Implementation results

- [PyPI benchmarks and client commands](@/ecosystems/pypi/performance.md)
- [OCI benchmarks and client commands](@/ecosystems/oci/performance.md)
- [Benchmark harness commands](@/contributing/benchmarking.md)
- [Architecture](@/core/architecture.md)
