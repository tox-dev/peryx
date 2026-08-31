+++
title = "Remote read-through"
description = "Fetch missing immutable content from a peer that holds a verified placement."
weight = 11
+++

A node can serve a local content miss from a peer datacenter that holds a
[verified placement](@/core/repositories/blob-placement.md) for the requested digest. It stages verified bytes in the
local content store, so later requests read from disk. Background replication fills stores ahead of demand; remote
read-through runs after a request misses local storage.

Each ecosystem uses the same selection, transfer, verification, and staging behavior while retaining its client routes
and fallback responses.

## Eligibility

A node installs remote read-through in `dc` or `ha` mode when it has a member roster, a replication token, and a
datacenter identity. An unnamed replica uses its owner's normal miss path. `none` mode has no remote read-through
service.

Each request checks local storage first. The content digest also keys the single-flight gate, so concurrent misses share
one peer fetch.

## Source selection

The service reads verified remote placements for the digest. It orders candidates by generation and a stable tie-break.
A [circuit breaker](@/core/availability/liveness.md) excludes a datacenter after repeated failures until its cooldown
ends. Fan-out and per-datacenter attempts have fixed bounds.

The placement record supplies the expected length. A peer cannot change the allocation size through its response.

## Transfer and verification

The transport requests bounded byte ranges. A failed range can use the next source. Transient failures follow a bounded
retry schedule; terminal responses end the attempt.

Each completed range is written to an unpublished stage in offset order, and publication happens only once the stage
hashes to the requested digest. Failed verification leaves no local content and produces no response body. The owner may
then use its configured fallback.

The first fetch of a digest records per-chunk digests taken from the same pass that published it. A later fetch splits
the transfer on those recorded boundaries and verifies each range as it arrives, which names the peer that served
corrupt bytes instead of failing the whole transfer.

Remote read-through does not advertise a new placement. Placement publication has its own fenced lifecycle. A later
request can still use the local bytes.

## Memory bound

A transfer holds its range budget rather than the content item, whatever the item's size. Ranges are requested in offset
order, at most four at a time and within a 32 MiB resident cap, and each one is written to the stage and released as
soon as the ranges before it have been written. A range wider than the whole cap still transfers on its own, so one
fetch holds at most the larger of its range size and 32 MiB. Concurrent misses multiply that ceiling by the number of
fetches in flight, not by the size of the content. Staged content serves from disk.

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
probe-timeout-secs = 30

[availability.read-through.retry]
base-ms = 100
multiplier = 2
max-delay-secs = 30
max-attempts = 10
```

Fields are optional. `chunk-bytes` sets the range size for a digest with no recorded per-chunk digests; once they are
recorded they own the boundaries, because a chunk digest verifies only its own span. The `retry` table sets the
reconnect schedule when present. Bounds that require a positive value reject zero during configuration loading.
`cooldown-secs` controls the open-state delay. `probe-timeout-secs` bounds the single half-open request; expiry or
cancellation begins another cooldown.
