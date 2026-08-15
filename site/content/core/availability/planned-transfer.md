+++
title = "Planned authority transfer"
description = "Move a healthy authority after its target catches up."
weight = 8
aliases = [ "/core/availability-planned-transfer/"]
+++

An authority is a repository's write owner in one home datacenter. A planned transfer moves a healthy authority for a
drain, rebalance, or migration. It commits only after the target has every acknowledged write. A
[failover transfer](@/core/availability/authority-transfer.md) instead moves an authority whose tracker state is `Dead`.

Planned transfers use the [availability control listener](@/core/availability/listener.md) and the fencing epoch defined
by the [availability contracts](@/core/availability/contracts.md). This contract covers start, cancellation, catch-up,
state transitions, audit records, and recovery.

The transfer engine treats authority and operation identities as opaque. The content owner supplies those identities;
`peryx-ha-distributed` owns the catch-up barrier, epoch transition, and audit record.

## Starting a transfer

An administrator starts a transfer with a `POST` to the control listener, behind the administration write scope:

```
POST /availability/v1/transfers
Idempotency-Key: 5f0c-drain-proj
{ "authority": "proj", "source": "east", "target": "west", "reason": "drain east" }
```

The node reads the barrier from its own current metadata serial rather than the request body, so a caller cannot ask a
move to commit before the target has replicated the writes this node has already applied. It takes the actor from the
authenticated principal, not the body, so the audit records who truly ordered the move. The `reason` is the operator's
stated justification, carried into the audit.

This is a distinct surface from the raw `transfer_authority` command on
[`/availability/v1/commands`](@/core/availability/listener.md#membership-and-transfer-commands). That command is the
unconditional consensus move a failover commits; the planned-transfer endpoint wraps it in the catch-up gate, the
barrier, and the audit, so an operator moving a live home does not have to hold the catch-up invariant by hand.

## The catch-up barrier

A planned transfer commits only after the target has applied through the barrier. The node probes the target's
change-feed serial through its roster-resolved address and folds each reading into the plan; the plan stands ready to
commit once the target's applied serial reaches the barrier. The check is monotonic, so a reading that arrives out of
order never un-readies a move.

A target that cannot be reached, or one absent from the roster, reads as no frontier, which never advances the plan, so
an unreachable target leaves the move waiting rather than committing to a home that has not caught up. The node
re-probes on a bounded schedule. A target that has not reached the barrier within the budget, about five minutes by
default, times out with `504 Gateway Timeout`. No audit is written, so the transfer can be retried once the target
catches up.

## States and cancellation

A transfer moves through one lifecycle to a single outcome. It waits at `AwaitingCatchUp` until the target reaches the
barrier, stands `Ready`, and commits once. An administrator can cancel it while it waits, and a cancel is refused once
it has committed, so a cancel that races the commit resolves to exactly one of the two.

{% mermaid() %} stateDiagram-v2 \[*\] --> AwaitingCatchUp AwaitingCatchUp --> Ready: target reaches the barrier Ready
--> Committed: commit mints the epoch and records the audit AwaitingCatchUp --> Cancelled: operator cancels Ready -->
Cancelled: operator cancels Committed --> \[*\] Cancelled --> [\*] {% end %}

Cancel a waiting transfer with a `DELETE` keyed by the authority:

```
DELETE /availability/v1/transfers/proj
```

- A cancel of a **waiting** transfer abandons its plan and answers `204 No Content`. Its run then observes the abandoned
  plan and resolves as a `409 Conflict` rather than committing a move the operator called off.
- A cancel of a transfer that **already committed** answers `409 Conflict`: the move stands, and the node keeps the
  committed plan registered so the cancel resolves against the sealed record rather than a lost lookup.
- A cancel for an authority with **no registered transfer** answers `404 Not Found`.

One transfer runs per authority at a time. A second `POST` for an authority whose transfer is still running answers
`409 Conflict`, so two moves never race toward the same home.

The commit rides an idempotency key, so a retry across a leader loss and a duplicated request collapse to one committed
move rather than two. Reissue a transfer that failed mid-flight with the same `Idempotency-Key` to reach one outcome.

## The audit record

A committed transfer seals a durable audit and answers with it:

```json
{
  "authority": "proj",
  "source": "east",
  "target": "west",
  "actor": "olivia",
  "reason": "drain east",
  "barrier": 4821,
  "epoch": 7,
  "commit_index": 9043
}
```

The record retains the authority, the source and target datacenters, the actor who ordered the move and their stated
reason, the barrier the target had to reach, the epoch the move minted, and the index of the consensus log entry that
committed it. The epoch comes from a read of committed ownership state rather than the commit receipt, matching the rest
of the ownership plane. The audit is durable and is the record an operator and reconciliation answer from: who moved an
authority, from where, to where, why, on what catch-up barrier, and under which committed index.

The minted epoch is the fence. A write the old home had in flight under the previous epoch is now stale and is rejected,
so a former home cannot finalize a write against an authority it no longer owns. Reconciling the operations the old home
recorded before the transfer follows the same terminal-disposition rules the
[failover transfer](@/core/availability/authority-transfer.md#reconciling-old-epoch-operations) uses.

## Operator recovery

To move a live home in an `ha` deployment:

1. Confirm the target is a reachable member of the datacenter roster; an unknown or unreachable target never catches up,
   so the transfer waits and then times out.
1. `POST` the transfer with a stated `reason` and an `Idempotency-Key`. The call returns the sealed audit once the
   target catches up and the move commits.
1. A `504 Gateway Timeout` means the target had not reached the barrier within the budget; let it catch up and reissue
   with the same key. A `409 Conflict` means a transfer for the authority is already running or has committed; read the
   returned message before retrying.
1. To abandon a transfer that is still waiting, `DELETE` it by authority. A `204` confirms it was abandoned before it
   committed; a `409` means it had already committed and the move stands.

Data at risk: nothing acknowledged. The barrier holds the commit until the target has applied every write this node
acknowledged, and a transfer that times out or is cancelled writes no audit and moves no home, so the authority stays
where it was.

## Related

- The involuntary move on a confirmed home failure:
  [authority transfer and drain](@/core/availability/authority-transfer.md)
- The socket these endpoints run on: [availability control listener](@/core/availability/listener.md)
- The durability and fencing each step preserves: [availability contracts](@/core/availability/contracts.md)
