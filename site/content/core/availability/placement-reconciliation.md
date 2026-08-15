+++
title = "Filesystem placement reconciliation"
description = "Verify blob placements, retire out-of-policy copies, and schedule repairs."
weight = 8
aliases = [ "/core/availability-placement-reconciliation/"]
+++

Placement records and local files can diverge after crashes, disk loss, or operator repair. Reconciliation runs on a
filesystem-backed `dc` or `ha` node. It reads the placement ledger, verifies local copies, retires copies outside
policy, and schedules missing copies through the cross-datacenter copier.

The reconciler consumes placement and reconciliation store traits from `peryx-ha`. Storage performs atomic reads and
compare-and-writes. `peryx-ha-distributed` owns scan order, repair policy, cancellation, and resource bounds.

## The two passes

One reconciliation pass runs two bounded scans over the placement ledger, in digest order, resuming past a cursor so no
single pass reads the whole ledger:

- **Integrity.** It re-verifies each local datacenter placement against its stored bytes by stream-hashing the file and
  comparing it to the digest it is addressed by. A copy whose bytes no longer match, or a verified record whose file has
  vanished, demotes to a digest-mismatch failure, and its bad bytes are dropped. Detecting the rot is this pass's job;
  the repair copy is not. A demoted placement leaves the served set and the local datacenter now owes the digest, so the
  copy backlog the [`dc-copy`](@/core/availability/authority-transfer.md) job schedules a fresh copy from a verified
  peer, one repair attempt, retried on the next pass if it fails.
- **Policy.** It classifies each digest's placements against the target datacenters and retires verified copies outside
  policy, such as a datacenter removed from membership, by revoking them from serving. A target datacenter that lacks a
  copy fills it through its own copy backlog, so reconciliation schedules removals to converge and leaves the copies to
  the copier.

## Repair states

A placement carries an evidence-based state, and reconciliation only ever moves it along one of two paths:

{% mermaid() %} flowchart LR; verified["Verified (served)"] -->|"bytes rotted or file gone"| failed\["Failed: digest
mismatch"\]; failed -->|"copy backlog re-copies from a peer"| pending["Pending (in flight)"]; pending -->|"peer bytes
verified"| verified; verified -->|"out of policy"| revoked["Revoked (retired)"]; class verified,pending good; class
failed,revoked warn; {% end %}

The demotion and the retirement are both fenced by the ownership group's cluster-level term, the same monotonic epoch
the copier fences on: a node running no ownership group reads term zero and reconciles nothing, and a placement write
under a stale term is rejected without effect. A demotion or retirement the fence turns away is left for the next pass
rather than forced.

## Garbage-collection coordination

Reconciliation never resurrects content the fleet is withdrawing. Before it repairs a copy, the integrity pass consults
two records and passes over any digest they cover:

- an **active [digest revocation](@/core/availability/contracts.md)**, which has retired the artifact from serving, and
- an **in-flight reclamation tombstone**, pending or ready, for bytes the reclaimer is about to delete.

Repairing either would re-copy bytes the fleet is removing and fight the reclaimer, so the pass skips them; the next
pass reconsiders once the withdrawal settles. Retirement carries no such gate: revoking an out-of-policy copy is a
removal, which never resurrects content, and a reclaimer re-checks serveability in its own transaction, so a retirement
only ever helps it along.

## Resource limits

Every scan is bounded so reconciliation stays a background cost:

- Each pass reads the ledger a bounded page of rows at a time and resumes past a cursor, so one pass reads a bounded
  slice rather than the whole ledger.
- The schedule interval bounds how often a pass runs; the node-local scheduler drops a pass whose predecessor is still
  draining, so a slow pass never stacks.
- The integrity pass re-hashes at most one page of local files per read and drops corrupt bytes rather than buffering
  them, bounding disk reads and memory.

## Related

- The copier that fills missing copies and repairs demoted ones:
  [authority transfer and drain](@/core/availability/authority-transfer.md)
- The durability each step preserves: [availability contracts](@/core/availability/contracts.md)
