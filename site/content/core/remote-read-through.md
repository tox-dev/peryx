+++
title = "Remote read-through"
description = "Fetch missing immutable content from a peer that holds a verified placement."
weight = 11
+++

A node can serve a local content miss from a peer datacenter that holds a [verified placement](@/core/blob-placement.md)
for the requested digest. It stages verified bytes in the local content store, so later requests read from disk.
Background replication fills stores ahead of demand; remote read-through runs after a request misses local storage.

Ecosystem drivers call the same selection, transfer, verification, and staging service. Their client routes and fallback
responses remain in the ecosystem layer.

## Eligibility

A node installs remote read-through in `dc` or `ha` mode when it has a member roster, a replication token, and a
datacenter identity. An unnamed replica uses its driver fallback path. `none` mode has no remote read-through service.

Each request checks local storage first. The content digest also keys the single-flight gate, so concurrent misses share
one peer fetch.

## Source selection

The service reads verified remote placements for the digest. It orders candidates by generation and a stable tie-break.
A [circuit breaker](@/core/availability-liveness.md) excludes a datacenter after repeated failures until its cooldown
ends. Fan-out and per-datacenter attempts have fixed bounds.

The placement record supplies the expected length. A peer cannot change the allocation size through its response.

## Transfer and verification

The transport requests bounded byte ranges. A failed range can use the next source. Transient failures follow a bounded
retry schedule; terminal responses end the attempt.

The service reassembles the ranges and verifies the complete content against the requested digest. It then stages the
bytes under their content address and verifies them again during publication. Failed verification leaves no local
content and produces no response body. The driver may then use its upstream fallback.

Remote read-through does not advertise a new placement. Placement publication has its own fenced lifecycle. A later
request can still use the local bytes.

## Memory bound

The current transfer path buffers one content item until complete verification. Per-peer concurrency limits bound the
number of buffers. Staged content serves from disk.

## Configuration

`dc` and `ha` modes accept `[availability.read-through]`. `none` mode rejects it.

```toml
[availability.read-through]
concurrency = 8
per-fetch-bytes = 67108864
chunk-bytes = 8388608
max-fanout = 4
trip-after = 3
cooldown-secs = 30

[availability.read-through.retry]
base-ms = 100
multiplier = 2
max-delay-secs = 30
max-attempts = 10
```

Fields are optional. The `retry` table sets the reconnect schedule when present. Bounds that require a positive value
reject zero during configuration loading.
