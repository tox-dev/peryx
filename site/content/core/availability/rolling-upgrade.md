+++
title = "Rolling upgrade and rollback"
description = "Preflight, order, and recover a rolling availability upgrade."
weight = 9
aliases = [ "/core/availability-rolling-upgrade/"]
+++

Replacing nodes one at a time requires protocol compatibility and enough healthy capacity for each drain. The
[version compatibility](@/core/availability/version-compatibility.md) contract defines negotiation, command enablement,
and downgrade limits. The sections below define drain preconditions, replacement order, and recovery after a failed
step. For the operator walkthrough against live surfaces, see
[upgrade and roll back an availability cluster](@/core/availability/upgrade-runbook.md).

The roll operates the writer model that [failover and recovery](@/core/availability/failover-recovery.md) guide recovers
and reuses the [planned transfer](@/core/availability/planned-transfer.md) to move the writer's home when its turn
comes. It changes no voting roster; membership changes go through the
[voting membership](@/core/availability/membership.md) procedure separately, and a new command stays disabled until the
whole committed set can apply it.

## The preflight

Before the roll drains a node it settles one go decision over the group's measured state. The version rules come first,
exactly as the compatibility page fixes them: every committed voter must already run the target on both dimensions, and
the target state machine must sit at or above the irreversible-migration floor. A target that fails either is reported
as a version blocker before any operational rule is read.

Four operational rules follow, each measured against an operator budget:

- **Quorum.** The group must be able to acknowledge a new write at its durability policy: a writer must report with
  enough members. The drain removes a node only from a group that already holds quorum rather than one hoping to regain
  it.
- **Capacity.** Draining one node must leave at least the budgeted number of members still serving, so the step keeps
  the headroom to serve reads and reach quorum while the node is gone rather than assuming the drain is instant.
- **Replication lag.** The group's durable frontier must trail the writer by no more than the budget, so a replica the
  group promotes mid-roll starts from an almost-current state instead of a long catch-up.
- **Backup currency.** The backup must trail the writer by no more than the budget, so a failed step recovers from a
  backup no further behind than the operator accepts.

The writer is the sole source of serials and bounds every member, so its reported frontier anchors both lag checks. A
node listed twice in the roster counts once, so a doubled node inflates neither the serving count nor the frontier. The
verdict reports every unmet rule in a fixed order: version rules, quorum, capacity, replication lag, then backup
currency. The stable order gives an operator every reason to wait rather than fixing them one round-trip at a time.

## The order

The roll replaces read replicas before the writer, each set in stable id order, and the writer last. Every replica step
removes a node the group can lose without an authority handoff, so the required handoff, moving the writer's home
through a [planned transfer](@/core/availability/planned-transfer.md), happens once at the end rather than repeatedly
through the roll. The order is taken over the configured roster, independent of what each member currently reports, so a
member that is briefly not reporting keeps its place rather than being skipped or reordered.

## Draining, transferring, and rolling back

Each step drains the node under replacement: it stops taking new work and the group confirms its acknowledged writes are
held elsewhere before the node leaves. A replica step ends there. The writer step additionally transfers the home to a
caught-up datacenter, gated on catch-up exactly as the planned transfer describes, so the new home takes ownership
holding every write the old home acknowledged.

Stopping a node cancels its active availability handle before joining managed resources. A join that misses the shutdown
deadline continues under the process reaper. Confirm that the old process has exited before starting its replacement;
the bounded application shutdown does not turn a still-joining resource into a second owner.

Rollback is a version decision until a migration makes it irreversible. Below that floor the preflight refuses the
target, because a snapshot written past an irreversible migration cannot be restored by the older build. Above it, a
downgrade is an ordinary target and rolls by the same order and preflight as an upgrade.

## When a step fails

A step that fails leaves the drained node out of service and the rest of the group serving. Recovery is bounded by the
two lag budgets the preflight held: the replication-lag budget kept a promotable replica close to the writer, and the
backup-currency budget kept a [restorable image](@/core/backup-restore.md) close behind, so the group either promotes
the replica or restores the backup without having acknowledged writes that neither holds. Once the failed node is
repaired or replaced and rejoins, the preflight is re-run before the roll continues, so a step never resumes into a
group that has not recovered its quorum, capacity, and currency.
