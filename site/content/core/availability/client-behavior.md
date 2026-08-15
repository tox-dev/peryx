+++
title = "Client behavior across availability modes"
description = "Retry, fencing, and read consistency in none, dc, and ha modes."
weight = 11
aliases = [ "/core/availability-client-behavior/"]
+++

Availability mode changes when a mutation can be acknowledged. Content owners map the shared outcome to their client
boundary.

## Writes

Send mutations to a writer. A replica refuses them before the content owner creates authoritative state. A writer also
refuses a mutation when it cannot meet the configured durability contract.

Each admitted mutation has an idempotency identity. A retry after a lost response returns the committed result without
applying another change. The authority epoch fences a request admitted under an earlier owner.

Ingress retains bounded records and bytes for work awaiting finalization. It refuses new work when either bound is full.

## Reads

The [readable frontier](@/core/availability/derived-views.md) bounds mutable reads. A replica withholds metadata until
each required view has applied its serial. Content-addressed reads verify returned bytes against the requested digest.

[Remote read-through](@/core/remote-read-through.md) can fetch missing local bytes from an eligible peer. Failed
verification leaves no partial content in the local store.

## Failover

An authority transfer advances the epoch and fences its former owner. Retry the same mutation against the current
writer. Do not change its identity between attempts.

The owning implementation defines the status, body, and retry signal. Availability code returns an owner-neutral
operation result.

## Configured `none` mode

With `mode = "none"`, writes commit on the local node and reads use local state. Startup constructs no distributed
coordinator, route, queue, timer, lifecycle handle, or persistence domain.
