+++
title = "Observe availability health"
description = "Inspect topology, placement, operations, readiness, and replica frontiers."
weight = 9
aliases = [ "/core/availability-observability/"]
+++

Peryx exposes the group roster, node roles, readable frontiers, and replica lag through bounded, role-filtered surfaces.
See [`[availability]`](@/core/operations/configuration.md#availability) for configuration and
[availability contracts](@/core/availability/contracts.md) for acknowledgement guarantees.

Every surface below filters its fields to the caller's class, sends `Cache-Control: no-store`, and stamps its own
observation time, so a stale render shows as age rather than passing for health. None of them traverse live membership
or storage state per request.

## The topology view

The topology surface is one immutable picture of the group taken at a single instant: the mode, the group identity, the
configured roster with each node's datacenter and role, and this node's own live frontier and liveness.
[Availability modes](@/core/availability/high-availability.md#availability-topology-snapshot) documents the snapshot and
its per-class filtering in full; read it as the
[availability topology page](@/core/operations/web-ui.md#availability-topology) at `/admin/topology`, or as JSON from
`GET /+availability/topology`. The snapshot does not poll peers, so non-local liveness stays `unknown`. Read heartbeat
health from a `dc` or `ha` writer's replication documents (see [Node liveness](@/core/availability/liveness.md)) rather
than from the snapshot.

An open operator page keeps that picture current without polling each node. `GET /+availability/topology/stream` is a
bounded Server-Sent Events feed of the same role-filtered snapshot. It sends the current snapshot on connect, then one
event only when this node's frontier or liveness moves, so its traffic tracks the change rate rather than the roster
size or the number of open pages; the observation time advancing on its own emits nothing, and an idle group carries
only a keep-alive comment every fifteen seconds. Each event's id increases, so a browser resumes from `Last-Event-ID` on
reconnect. A reader too slow to keep up coalesces to the latest snapshot rather than a backlog, because each sample
re-reads live state and the connection buffers nothing. The topology page shows a feed badge that reads `Reconnecting`
or `Offline` while the browser retries, so a paused feed freezes the snapshot time rather than passing stale data for
fresh. The stream inherits the caller's credentials and `no-store` policy from the one-shot endpoint, so it never widens
what a page already reveals.

## Placement health

The placement surface reports how the store's bytes are placed: how many artifacts serve from local storage, how many
have no local bytes but a reachable upstream, and how many have neither. Read it as the
[artifact placement-health page](@/core/operations/web-ui.md#artifact-placement-health) at `/admin/placements`, or as
JSON from `GET /+availability/placements`.

The whole-store counts need `operator:read` and are aggregated before serialization, so the summary never scales with
the object count. A per-digest table needs `administration:read`, because a digest identifies an artifact; it pages in
digest order, bounded at the supported limit with a cursor to resume. Each row carries a digest with its source and byte
availability alone, never a file path, repository, or owner, so inspecting convergence exposes no tenant data. An
operator who cannot read the rows still reads the counts.

Use the counts to watch a replica converge: a rising `remote-only` count on a node that should hold bytes locally names
a store that has applied metadata ahead of the blobs it references, which the
[derived-view frontier](@/core/availability/derived-views.md) holds back from readers until the bytes land.

## Pending operations

The operations surface reports the admitted writes the node retains, bucketed by the client-facing status each reads:
`pending` while a write is in flight within its retention deadline, `published` once it finalizes, `failed` when it
gives up, and `expired` when it outlives its deadline without finalizing. Read it as the
[pending-operations page](@/core/operations/web-ui.md#pending-operations) at `/admin/operations`, or as JSON from
`GET /+availability/operations`.

The whole-ledger counts need `operator:read` and are aggregated before serialization, so the summary never scales with
the number of retained writes. A per-operation table needs `administration:read`, because an operation id identifies a
write; it pages in operation-id order, bounded at the supported limit with a cursor to resume. Each row carries an
operation id with its status, when its record last changed, and when it may be pruned, never the response bytes, the
repository, or the owner, so inspecting a write's convergence exposes no tenant data. An operator who cannot read the
rows still reads the counts.

An `expired` write is not a definite failure: the retention deadline is the client's wait, not the write's, so a durable
completion may have happened after the client stopped polling. A terminal record is pruned once its deadline passes,
while a still-pending one is kept, so a rising `expired` count names writes whose durability a client could not confirm
within its deadline rather than writes known to have been lost.

The operation surface consumes the shared `OperationObserver` contract. An observation contains an owner-neutral source,
authority epoch, serial, and operation kind. Content owners emit observations; availability code aggregates them without
importing owner schemas or route types.

## Liveness and readiness

The topology and placement surfaces describe the group; the
[load-balancer probes](@/core/availability/high-availability.md#load-balancer-probes) describe one node's fitness to
receive traffic. `GET /+health` stays live while the process can answer at all; `GET /+ready` fails a node whose local
metadata or blob store cannot serve; `GET /+ready?writes=true` fails a replica, which serves reads but rejects mutation.
`GET /+status` is the detailed operator surface, filtered to the caller's class the same way the topology view is. Point
an ingress rule at the probes and reserve the topology and placement pages for authenticated operators.

## Reading at a consistent point

A replica serves reads only up to its [readable frontier](@/core/availability/derived-views.md): the lowest metadata
serial every required view has rebuilt to. A record the replica has stored but not yet reflected in its search index or
rendered pages stays below that frontier, so a client never pairs new metadata with an old view. The frontier is durable
and monotonic, so a restart never exposes a record a view had not applied. A replica exports how far its views trail the
metadata it has committed as `peryx_ha_distributed_readable_serial`, which the placement and topology surfaces
complement with per-node liveness.

Send every mutation to the writer. A replica rejects uploads, deletes, and other mutations with
`503 Service Unavailable`, preventing a misrouted write from diverging the copy.

## Mode scope

Availability surfaces exist only under `dc` or `ha`. With `mode = "none"`, the process registers no availability routes,
metrics, listeners, workers, or lifecycle handles. It also creates no distributed-domain tables. General request, job,
and storage metrics remain available because they are not part of distributed coordination.

Each surface reports only proven coordinator state. An absent or unknown peer observation never reads as healthy.
