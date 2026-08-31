+++
title = "Visibility replication"
description = "How a replica learns that an artifact is hidden."
weight = 12
aliases = [ "/core/availability-visibility-replication/"]
+++

Trash, restore, revoke, and lift decide whether a node may serve an artifact. Hidden state is metadata: a small
authoritative record and the serial that orders it, never artifact bytes. A tombstone hides bytes; it never moves them.
So visibility replicates the way every other metadata mutation does, over the one change feed, and a replica commits it
in the same transaction that advances its cursor.

There is no separate visibility feed, envelope, projection, or snapshot. A replica's hidden set is the rows it has
committed, so it is durable exactly when its cursor is, and a restart resumes from that cursor with the hidden set the
node last committed.

This contract refines the [availability contracts](@/core/availability/contracts.md) and uses the read frontier defined
by [derived-view frontiers](@/core/availability/derived-views.md).

## Two records, one feed

Where the record lives follows who owns the decision.

**An ecosystem owns its soft deletes.** Trash and restore edit the ecosystem's own artifact record, and the ecosystem's
serving path reads that record back when it answers. The edit reaches a replica as the change's metadata mutations,
which the replica writes as the opaque driver rows they are. Nothing in the shared crates interprets them, so trash
semantics stay with the owner that defines them.

**The server owns digest revocation.** Revocation is server-wide and ecosystem-independent, so it lives in core tables
and its change carries the whole row as a tagged core payload rather than as driver rows. A replica writes the row, its
status index, and its active count together, from the same function the writer uses. See
[digest revocations](@/core/repositories/digest-revocations.md).

Both shapes travel as ordinary changes on the same feed and inherit its ordering, back-pressure, and recovery.

## Ordering and repeated delivery

The journal serial is the whole order. A replica applies changes in serial order, refuses a page that skips a serial or
that comes from a source other than the one its cursor is pinned to, and commits each page against the cursor it read.
Two nodes that have applied the same serial therefore agree on the hidden set at that serial.

Each change carries the state it intends rather than a delta toward it, so redelivering one is a no-op: a repeated
revocation writes the same row and leaves the active count where it was, and a repeated trash rewrites the artifact
record it already wrote. Recovery can replay a page it is unsure of without resurrecting an artifact.

## What a replica advertises

A replica publishes an applied frontier only after the page's transaction has committed, and it retires the cached
serving decisions the page invalidated before publishing. Serving reads expose changes only up to the readable frontier,
which trails the applied frontier until the derived views catch up. A reader that trusts a replica's reported serial can
therefore trust that the node has stopped serving what the writer hid at or below it, and never sees a transition half
applied.

If the commit fails, nothing moves: the rows, the cursor, and the advertised frontier all stay where they were, and the
page is retried. A change whose core payload does not decode fails the whole page rather than being skipped, so a
replica cannot advance past a revocation it did not understand.

## Consistency, lag, and recovery by mode

A visibility transition carries the **metadata** promise of its mutation mode and never waits on byte convergence.

- **`none`** applies the transition on the local node. It starts no replication feed or managed replica. Storage loss
  can lose every transition committed after the latest verified backup; recovery restores that backup.
- **`dc`** makes the transition durable in a second failure domain within the datacenter before it acknowledges. It
  survives the loss of one node's storage with no lost transition; it does not survive the loss of the datacenter.
- **`ha`** makes the transition durable in the write-ack policy's share of the remote datacenters before it
  acknowledges. A trash, restore, revoke, or lift survives the loss of the writing datacenter, and a reader in the
  surviving datacenter serves the same hidden set the transition intended.

**Consistency across modes is the same.** The modes differ only in how much is at risk when a failure domain is lost,
not in what a served answer means.

**Lag signals.** Each replica's applied frontier, read against the writer's current serial, is the lag of its hidden
set; a replica that has caught up hides what the writer has hidden. The
[readiness probe](@/core/availability/high-availability.md#availability-health-and-readiness) reports whether a
replica's derived views have reached the frontier it advertises.

**Recovery objectives.** Because a visibility transition never gates on bytes, its recovery objectives are the metadata
objectives of its mode:

| Mode   | Visibility RPO on failure-domain loss                    | Visibility RTO                                            |
| ------ | -------------------------------------------------------- | --------------------------------------------------------- |
| `none` | Every transition since the last verifiable backup        | Writer restore or manual promotion, then replica catch-up |
| `dc`   | Zero within the datacenter; DC loss falls back to `none` | In-DC promotion of the synchronous copy, then catch-up    |
| `ha`   | Zero across the loss of the writing datacenter           | Cross-DC failover to the remote copy, then catch-up       |
