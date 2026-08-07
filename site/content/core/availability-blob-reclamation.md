+++
title = "Blob reclamation"
description = "Fence deletion of unreferenced content behind placements and replication frontiers."
weight = 9
+++

Several metadata records can refer to one content digest. A single-node purge can act on one reference snapshot, but a
replicated cluster must account for replicas and backups that have not applied the snapshot. The cluster records a
durable reclamation decision before a backend executor removes bytes.

## Retained references

A selector builds the retained set from these sources:

- Driver metadata references to immutable content
- Restorable trash entries
- Verified [placements](@/core/blob-placement.md) that can serve the digest

Each ecosystem driver defines which metadata records retain content:

- [Python package reclamation references](@/ecosystems/pypi/reference/availability.md#reclamation-references)
- [OCI reclamation references](@/ecosystems/oci/reference/availability.md#reclamation-references)

The selector reads placements in the same transaction as the reference inventory. A digest outside the retained set is a
candidate. A returned reference abandons an existing candidate.

## Frontier gate

Candidate selection writes a reclamation tombstone with the current authoritative metadata serial as its required
frontier. Deletion waits until each live replica and configured backup has applied that serial. One lagging plane keeps
the candidate pending.

Replication planes publish applied frontiers through cluster liveness. Until a source reports a frontier, readiness uses
zero. A candidate with a nonzero requirement cannot advance on missing evidence.

## Fencing

One cluster worker selects and advances tombstones. Its singleton lease uses the ownership group's monotonic term. Each
tombstone transition records that term and rejects a stale term. A process without an ownership group uses term zero and
cannot reclaim content.

## Tombstone states

| State     | Meaning                                                                                     |
| --------- | ------------------------------------------------------------------------------------------- |
| `Pending` | No current reference or serveable placement exists; required frontiers have not cleared.    |
| `Ready`   | Frontiers cleared and final reference checks passed; the backend executor may delete bytes. |
| `Skipped` | A reference or serveable placement returned.                                                |

Each fenced transition increments `attempts`. Selecting the digest again raises its required frontier and returns it to
pending, including after it reached `Ready`. Selection runs in bounded batches outside request handling.

## Backups

A completed backup captures the reference set at its recovery point. It does not retain future content. Reclamation
waits for the applied frontier of each configured backup so the backup can capture every digest referenced through the
candidate serial.

## Recovery and metrics

Durable tombstones and attempt counts survive restart, snapshot, and restore. A resumed pass continues from stored
state. A bounded prune removes terminal tombstones.

Metrics expose low-cardinality counts for pending, ready, and skipped tombstones.

## Related

- [Blob placement](@/core/blob-placement.md)
- [Fenced cluster jobs](@/core/high-availability.md)
- [Backup and restore](@/core/backup-restore.md)
