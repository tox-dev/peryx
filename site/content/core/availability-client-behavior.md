+++
title = "Client behavior across availability modes"
description = "Shared retry, fencing, and read-consistency behavior in none, dc, and ha modes."
weight = 11
+++

Availability mode changes when a mutation can be acknowledged. Ecosystem protocols keep their request and response
formats.

## Writes

Send mutations to a writer. A replica refuses them before the ecosystem driver creates authoritative state. A writer
also refuses a mutation when it cannot meet the configured durability contract.

Each admitted mutation has an idempotency identity. A retry after a lost response returns the committed result without
applying another change. The authority epoch fences a request admitted under an earlier owner.

Ingress retains bounded records and bytes for work awaiting finalization. It refuses new work when either bound is full.

## Reads

The [readable frontier](@/core/availability-derived-views.md) bounds mutable reads. A replica withholds metadata until
each required view has applied its serial. Content-addressed reads verify returned bytes against the requested digest.

[Remote read-through](@/core/remote-read-through.md) can fetch missing local bytes from an eligible peer. Failed
verification leaves no partial content in the local store.

## Failover

An authority transfer advances the epoch and fences its former owner. Retry the same mutation against the current
writer. Do not change its identity between attempts.

Protocol mappings define the status, body, and retry signal:

- [Python package availability behavior](@/ecosystems/pypi/reference/availability.md)
- [OCI availability behavior](@/ecosystems/oci/reference/availability.md)

## Local mode

In `none` mode, writes commit on the local node and reads use local state. The process installs no distributed routing,
replication queue, or availability timer.
