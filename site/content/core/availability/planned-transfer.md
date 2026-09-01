+++
title = "Planned authority transfer"
description = "Describe the HA planned-transfer component and its deployment boundary."
weight = 8
aliases = [ "/core/availability-planned-transfer/"]
+++

The HA control listener exposes the shipped transfer handlers; the peer traffic that commits a transfer runs on the
public server every member `address` names. Mode `dc` has no ownership consensus and returns `503 Service Unavailable`
for transfer requests.

An authority is a repository's write owner in one home datacenter. A planned transfer moves a healthy authority for a
drain, rebalance, or migration. It commits only after the target has every acknowledged write. A
[failover transfer](@/core/availability/authority-transfer.md) instead moves an authority whose tracker state is `Dead`.

Planned transfers use the [availability control listener](@/core/availability/listener.md) and the fencing epoch defined
by the [availability contracts](@/core/availability/contracts.md). This contract covers start, cancellation, catch-up,
state transitions, audit records, and recovery.

The transfer engine treats authority and operation identities as opaque. The selected ecosystem supplies those
identities; Peryx enforces the catch-up barrier, epoch transition, and audit record.

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
[`/availability/v1/commands`](@/core/availability/listener.md#ha-membership-and-transfer-commands). That command is the
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
barrier, stands `Ready`, claims the move as `Committing`, and commits once. An administrator can cancel it up to that
claim, and a cancel is refused from the claim onward, so a cancel that races the commit resolves to exactly one of the
two.

The claim is the only part of a commit taken under the plan lock; the frontier probe and the consensus submission both
run without it. So a cancel is answered from the plan rather than behind whatever the node is currently blocked on: a
probe against an unresponsive target runs out its own ten-second timeout, a consensus round trip can span a leader
election, and neither delays the answer. A cancel that lands during a probe or between probes also ends that wait
outright, so the move is called off rather than carried into consensus by the barrier check that follows.

`Committing` lives in memory and is not an outcome. Two refusals are raised before the command can reach consensus: a
reused idempotency key, and a saturated command queue. Either returns the transfer to `Ready`, still cancellable under
the same identity. Any other failure, leadership loss and timeout included, may already have appended, so the transfer
stays claimed and the same `Idempotency-Key` resolves it. A later cancel for a transfer left in that state is answered
from the durable audit: `409 Conflict` if the move did commit, `404 Not Found` if it did not.

{{<diagram file="planned-transfer" />}}

Cancel a waiting transfer with a `DELETE` keyed by the authority:

```
DELETE /availability/v1/transfers/proj
```

- A cancel of a **waiting** transfer abandons its plan and answers `204 No Content`. Its run leaves the probe or poll it
  was in and resolves as a `409 Conflict` rather than committing a move the operator called off.
- A cancel of a transfer that **already committed** answers `409 Conflict`: the move stands, and the node reads the
  durable audit rather than a retained plan, so it answers the same after a restart.
- A cancel for an authority with **no registered transfer** answers `404 Not Found`.

A resolved plan leaves the registry, so a node that has moved half a million authorities holds the transfers in flight
rather than one plan per authority it ever moved. What outlives a run is the durable audit, plus the 256 most recently
abandoned authorities, which is what lets a repeated cancel of a transfer that timed out or was called off stay
idempotent. A cancel for an authority whose abandoned transfer has aged out of that window answers `404 Not Found`; a
cancel after a commit does not age out, because it is answered from the audit. A node that cannot read its metadata
store answers `503 Service Unavailable` rather than guessing which of the two it was.

One transfer runs per authority at a time. A second `POST` for an authority whose transfer is still running answers
`409 Conflict`, so two moves never race toward the same home.

The commit rides an idempotency key, so a retry across a leader loss and a duplicated request collapse to one committed
move rather than two. Reissue a transfer that failed mid-flight with the same `Idempotency-Key` to reach one outcome.
The `Idempotency-Key` links a retry to its replicated control receipt and pending audit. Peryx assigns an identity
before submitting a request without that header and includes it in the response.

## The audit record

A committed transfer seals a durable audit and answers with it:

```json
{
  "id": "5f0c-drain-proj",
  "authority": "proj",
  "source": "east",
  "target": "west",
  "actor": "olivia",
  "reason": "drain east",
  "barrier": 4821,
  "epoch": 7,
  "commit_term": 12,
  "commit_index": 9043
}
```

The record retains the transfer identity and authority; source and target datacenters; actor and reason; catch-up
barrier; epoch; and Raft term and index. Operators and reconciliation use it to identify the committed move.

The ownership state machine seals the request fields with the post-mutation epoch and the deciding log position. A later
authority move cannot change the epoch recorded for an earlier transfer. Replicated ownership keeps only the pending
audit facts and the current home and epoch for each authority. Once projected, the durable audit is the sole historical
record, rather than an unbounded trail carried in every snapshot.

## Recovering an audit

Raft commits the move before `MetaStore` stores the audit, so the store write can fail after ownership changes.
Consensus retains the sealed record under the transfer identity until the projector stores it. Snapshots carry pending
records past process and leader loss. The idempotency window does not remove them.

A failed store write answers `503 Service Unavailable`, names the transfer identity, and leaves the sealed record
pending. Peryx can finish the projection through either path:

- A retry under the same `Idempotency-Key` answers from the committed decision. It stores the sealed record and answers
  `200 OK` with the original epoch and log term/index. No second move occurs.
- Startup projects pending records. The coordinator does the same before accepting another transfer. A new leader reads
  the records from replicated state; it does not need the original process or transfer plan.

The audit store uses the authority and commit index as its identity, making repeated writes idempotent. The projector
clears the replicated fact after the store transaction commits, so a crash between those operations leaves the fact for
recovery.

The minted epoch is the fence. A write the old home had in flight under the previous epoch is now stale and is rejected,
so a former home cannot finalize a write against an authority it no longer owns. Reconciling the operations the old home
recorded before the transfer follows the same terminal-disposition rules the
[failover transfer](@/core/availability/authority-transfer.md#reconciling-old-epoch-operations) uses.

## HA component exercise

In a development environment that supplies the missing peer routing outside Peryx:

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
