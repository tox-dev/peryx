+++
title = "Upgrade and roll back an availability cluster"
description = "The operator walkthrough for moving a live dc or ha cluster to an adjacent build and back: the two go decisions to read off the running surfaces before each hop, the replicas-first-writer-last order, and how a downgrade stops at the irreversible-migration floor."
weight = 9
+++

This page walks an operator through upgrading a running `dc` or `ha` cluster to an adjacent build, and rolling it back,
without dropping the service. It is the how-to counterpart to two reference pages and restates neither. Whether a target
build may run at all — the versions two nodes negotiate, when a new command becomes safe to issue, and the boundary a
downgrade may not cross — is fixed on the [version compatibility](@/core/availability-version-compatibility.md) page.
Why the roll drains replicas before the writer and how a failed step recovers is fixed on the
[rolling upgrade and rollback](@/core/availability-rolling-upgrade.md) page. This page is the operator's task: which
surface to read each go decision off, the order to replace nodes in, and how to confirm the client saw no gap. Stand the
cluster up first with [availability deployment and sizing](@/core/availability-deployment.md); this guide assumes a
healthy group already serving.

peryx runs no single upgrade command. A roll is you replacing one node at a time against the running readiness and
transfer surfaces, holding the two go decisions the reference pages fix, so every step below is an ordinary deploy gated
on a surface peryx already serves rather than a bespoke upgrade tool.

## Read the two go decisions before each hop

A hop moves the cluster one negotiated version, because nodes interoperate one step at a time and a jump past a version
no peer still advertises has no shared step to land on (see
[the compatibility matrix](@/core/availability-version-compatibility.md#the-compatibility-matrix)). Before each hop,
settle two go decisions against the live group; both must pass for the whole cluster, not one node.

**The version decision.** The target must be adjacent, its advertised range must overlap the range of every node still
in the cluster, and its state machine must sit at or above the recorded irreversible-migration floor. The contract fixes
these rules; the runtime signal that a pair is version-incompatible is the `incompatible_schema` reason on a replica's
[`GET /+replication/v1/ready`](@/core/high-availability.md#availability-health-and-readiness), which a later poll cannot
clear without changing a build. A replica already reporting `incompatible_schema` against its writer is telling you the
hop you are about to take is not adjacent.

**The operational decision.** Even a version-clean target is unsafe to roll if draining the next node would drop the
group below its durability policy or strand it too far ahead of a promotable replica and the backup. Read each of the
[preflight's operational rules](@/core/availability-rolling-upgrade.md#the-preflight) off a running surface:

- **Quorum and capacity.** The writer folds the group's frontiers into one verdict under the `group_readiness` field of
  its own [`GET /+replication/v1/ready`](@/core/high-availability.md#dc-group-readiness) for an `operator:read` caller.
  Proceed only when `ready` is `true` and `blocked` is `null`; a `writer_lost` or `insufficient_members` block names a
  group that cannot acknowledge a write right now, so draining a further node would take quorum, not remove a spare.
- **Replication lag.** `group_readiness.durable_frontier` is the serial the policy's members have all applied; compare
  it against the writer's own committed `serial` and against [`peryx_ha_distributed_lag`](@/core/metrics.md) per
  replica. A replica the roll may promote mid-hop should trail by no more than your lag budget, so it starts almost
  current rather than facing a long catch-up.
- **Backup currency.** A step that fails recovers from a backup no further behind than you accept, so confirm the backup
  is current and reproven — `peryx backup verify` — before you drain (see
  [verify a backup](@/core/backup-restore.md#verify-a-backup)).

## Upgrade the cluster, one version at a time

Replace read replicas first and the writer last, so the one unavoidable authority handoff happens once, at the end,
rather than repeatedly (see [the order](@/core/availability-rolling-upgrade.md#the-order)). Take the order over the
configured roster in stable id order, independent of what each member currently reports.

1. **Settle both go decisions** for the target, as above. A version blocker or an unmet operational rule is a reason to
   wait, not to override; fix it and re-read before you drain.

1. **Replace each read replica, one at a time.** For a replica in stable id order:

   1. Drain it from the read pool by letting readiness pull it — point the pool at
      [`GET /+replication/v1/ready`](@/core/availability-deployment.md#monitor-each-shape) so a node that stops
      answering `200` leaves rotation without a client change.
   1. Stop the process, deploy the target build against the same data directory, and start it.
   1. Wait for `GET /+replication/v1/ready` to answer `200` with an empty `reasons`, the signal it has caught its
      frontier back up and is safe to serve, then return it to the pool.
   1. Re-read the operational go decision before the next replica: a step never resumes into a group that has not
      recovered its quorum, capacity, and currency.

1. **Replace the writer last.** Move its home to a caught-up datacenter with a
   [planned transfer](@/core/availability-planned-transfer.md#starting-a-transfer) — `POST /availability/v1/transfers`
   with a stated `reason` and an `Idempotency-Key` — which commits only after the target has applied through the
   barrier, so the new home takes ownership holding every write the old home acknowledged. Then upgrade the drained
   former writer as a replica and let it rejoin. On a `none` deployment there is no group to transfer through; promote a
   caught-up replica with [`peryx writer promote`](@/core/high-availability.md#manual-promotion) instead, fencing the
   old writer first.

1. **Let the new behavior activate at the end.** A version-gated command stays disabled until every committed voter
   advertises support for it, so new behavior turns on once the last node clears the target, not as each node restarts
   (see [the feature barrier](@/core/availability-version-compatibility.md#the-feature-barrier)).

## Roll back the cluster

A downgrade rolls by the same order and the same two go decisions as an upgrade — replicas first, the writer last
through a planned transfer — with one added boundary. A rollback is an ordinary target until a migration makes it
irreversible: once the state machine has crossed a version that rewrites persisted state into a form an older build
cannot read, the cluster records that version as an irreversible-migration floor, and the version decision refuses any
target below it because a snapshot taken past the floor cannot be restored by the older build (see
[the rollback boundary](@/core/availability-version-compatibility.md#the-rollback-boundary)). Above the floor, downgrade
one adjacent version at a time exactly as you upgraded.

## When a step fails

A drained node that will not come back leaves the rest of the group serving on its survivors, and recovery is bounded by
the two lag budgets the go decision held: the replication-lag budget kept a promotable replica close to the writer, and
the backup-currency budget kept a restorable image close behind, so the group promotes the replica or restores the
backup without having acknowledged writes neither holds. The full recovery paths are on
[when a step fails](@/core/availability-rolling-upgrade.md#when-a-step-fails) and, by failure class, on
[failover and recovery](@/core/availability-failover-recovery.md). Repair or replace the failed node, let it rejoin, and
re-read both go decisions before the roll continues.

## Confirm the upgrade

An upgrade is done when the cluster proves it, not when the last process restarts:

- Every replica answers `GET /+replication/v1/ready` with `200` and an empty `reasons`, and the writer answers
  `GET /+ready?writes=true` with `200`.
- [`GET /+availability/topology`](@/core/high-availability.md#availability-topology-snapshot) shows every node at its
  intended `role`, with the home where the transfer left it.
- The client saw no gap: a mutation stayed retry-safe across the writer handoff and a read never served the wrong bytes,
  the [client behavior across availability modes](@/core/availability-client-behavior.md) the whole roll preserves.

## Related

- Whether a target may run, and the downgrade floor:
  [version compatibility](@/core/availability-version-compatibility.md)
- Why the order and preflight are what they are: [rolling upgrade and rollback](@/core/availability-rolling-upgrade.md)
- Move the writer's home deliberately: [planned authority transfer](@/core/availability-planned-transfer.md)
- Stand up and size the cluster this guide upgrades:
  [availability deployment and sizing](@/core/availability-deployment.md)
- What a client observes while the roll runs:
  [client behavior across availability modes](@/core/availability-client-behavior.md)
