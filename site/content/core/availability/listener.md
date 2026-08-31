+++
title = "Availability control listener"
description = "Configure the private availability control socket."
weight = 8
aliases = [ "/core/availability-listener/"]
+++

Availability controls use a private socket, separate from public content routes. In `dc` or `ha` mode, the
[availability contract](@/core/availability/contracts.md) requires authentication and server-administrator access for
every control request. Mode `none` creates no listener, socket, timer, or task.

The listener serves read-only status in `dc` and `ha`. DC has no ownership consensus, so command and transfer requests
return `503 Service Unavailable`. HA assembles the command and transfer handlers here; peer traffic never reaches this
socket, the ownership Raft RPCs included, because a member `address` names the public server that serves every peer
route. The listener is private and is not part of the public OpenAPI document.

## Enabling the listener

The listener is configured under the `[availability.listener]` table and requires `dc` or `ha` mode. Configuring it
under `none` is rejected, so a process without managed availability cannot open the control plane.

```toml
[availability]
mode = "dc"

[availability.replication]
role = "primary"
source = "https://writer.internal/"
token_file = "/run/secrets/replication-token"

[availability.listener]
bind = "127.0.0.1:4460"
```

`bind` defaults to `127.0.0.1:4460`, a loopback address, so the control plane stays private until an operator widens it.
A node reads its own listener from configuration; a restored backup does not carry it, because the bind is a per-node
network fact rather than cluster state.

Preparation binds the socket without accepting requests. Activation starts the listener after startup checks pass.
Failed activation closes it with the other prepared resources. Shutdown cancels the listener before joining it.

## Transport and network segmentation

Keep the listener on a management network an administrator reaches and package clients do not. A non-loopback `bind`
must terminate TLS or explicitly opt in to plaintext, so the control plane is never exposed to the network unencrypted
by omission:

```toml
[availability.listener]
bind = "10.0.0.5:4460"

[availability.listener.tls]
cert = "/etc/peryx/control-cert.pem"
key = "/etc/peryx/control-key.pem"
```

A non-loopback bind without a `[availability.listener.tls]` block is refused unless `allow-remote-plaintext = true`
states the intent, which suits only a trusted, isolated segment that terminates TLS in front of the node.

## Authentication and scopes

The listener reuses the same identity store as the package API; it holds no second user database. A request presents
HTTP Basic credentials for a local user, which the node authenticates once and then authorizes against the scope the
route needs: the status endpoint requires the server-wide administration read scope over the operator resource, and the
command endpoint requires the administration write scope. A request without a credential, or with an invalid one,
receives `401 Unauthorized` with a `WWW-Authenticate` challenge. An authenticated user lacking the route's scope
receives `403 Forbidden`. Rotating a user's password immediately rejects the old one, and revoking the administration
grant immediately forbids a prior administrator.

## The status endpoint

```
GET /availability/v1/status
```

The response reports the advertised protocol version, the node's mode and authority role, and whether it currently
serves read-only. On a node that runs an ownership consensus group it also reports the group's leader, term, and voter
membership under `consensus`, and the recent command latency under `commands`:

```json
{
  "protocol_version": 2,
  "mode": "ha",
  "role": "writer",
  "read_only": false,
  "consensus": {
    "leader": "east",
    "term": 3,
    "voters": [
      "east",
      "west"
    ]
  },
  "commands": {
    "completed": 12,
    "p50_ms": 4,
    "p99_ms": 90
  }
}
```

A DC response contains the four posture fields and omits `consensus` and `commands` because DC constructs neither
component. The HA example shows the response shape once those components are present.

The `commands.p99_ms` figure is the 99th-percentile command latency over a bounded recent window, so a latency spike
through a leader change is visible without an external metrics pipeline. The path carries a version segment so a client
pins the protocol versions it understands and refuses an incompatible peer rather than guessing a wire shape; protocol
version 2 adds the command endpoint below. An unknown path answers `404 Not Found` without consulting the identity
store, so an unauthenticated caller cannot probe the surface.

## HA membership and transfer commands

```
POST /availability/v1/commands
```

An administrator drives the HA ownership consensus group through this endpoint. Every command commits through the Raft
log; the handler submits a typed command and never writes the membership or ownership store directly, so a rejected or
replayed command cannot corrupt the group. The endpoint requires the administration write scope, the write counterpart
of the read scope the status endpoint gates.

The request body is a tagged command. The four membership commands rewrite the consensus roster, and the three authority
commands move, fence, or drop an artifact home:

| `type`               | Fields                            | Effect                                                            |
| -------------------- | --------------------------------- | ----------------------------------------------------------------- |
| `add_learner`        | `datacenter`, `address`           | Add a non-voting learner that replicates the log.                 |
| `promote_voter`      | `datacenter`                      | Promote a caught-up learner to a voter that counts toward quorum. |
| `remove_voter`       | `datacenter`                      | Remove a voter from the roster.                                   |
| `replace_voter`      | `remove`, `datacenter`, `address` | Add the incoming datacenter as a learner and swap it in.          |
| `transfer_authority` | `authority`, `new_home`           | Move an authority's home, minting the next epoch.                 |
| `advance_epoch`      | `authority`                       | Mint the next epoch without moving the home, fencing stale work.  |
| `forget_authority`   | `authority`                       | Drop a retired authority's home and epoch from replicated state.  |

`forget_authority` is how a deleted repository stops paying for replication. Replicated ownership state holds one record
per authority and every snapshot carries all of them, so a repository that will never be published to again otherwise
travels to each rejoining follower forever. The command answers `no_change` when nothing is homed under the authority,
and is refused while a write lease is live, because the lease holder still stamps work with the epoch it drops. An
authority published to after being forgotten is assigned again from epoch one.

An `address` follows the same contract as an `[[availability.member]]` address: an `http` or `https` URL with an
explicit port and no path, query, fragment, or credentials. A command carrying any other form is rejected as invalid
before it reaches the log, so a learner cannot join on an address static membership would refuse.

```
POST /availability/v1/commands
Idempotency-Key: 5f0c-transfer-proj-west
{ "type": "transfer_authority", "authority": "proj", "new_home": "west" }
```

A committed command answers `200 OK` with the committed identity: the log term and index, and whether the command
changed the state:

```json
{
  "term": 3,
  "index": 42,
  "outcome": "committed"
}
```

An `outcome` of `no_change` means the command committed but left the roster or ownership state as it was, for example
promoting a datacenter that is already a voter, so a repeat is safe.

### Idempotency

A client stamps an `Idempotency-Key` header to make a command retry-safe. A repeat carrying a key that already committed
reads back the first receipt without submitting a second command, so a client retry across a leader change, common for a
membership command, mints one command rather than two.

The group replicates the key and the command it stands for, and an authority transfer or epoch advance records its
receipt in the consensus decision that applies it. A retry served by a replacement leader, or by a node that has since
restarted, therefore reads back the committed result rather than mutating a second time. A key stays replayable for 15
minutes from its first use; a key that ages out of that window is submitted again, so a key is a retry token, not a
durable dedup ledger.

Reusing a key for a different command is refused with `409 Conflict` for as long as the key stays in the window,
including after a restart.

Only a committed receipt holds a key. A command that fails releases its key in the same replicated decision that would
have recorded the receipt, whether the roster refused the address, the authority was unassigned, or the node lost
leadership mid-command. Correct such a command and reissue it under the same key; it runs afresh rather than reading
back a failure or waiting out the 15-minute window.

### Failure statuses

| Status                    | Cause                                                                                        |
| ------------------------- | -------------------------------------------------------------------------------------------- |
| `403 Forbidden`           | The actor lacks the administration write scope.                                              |
| `409 Conflict`            | The command is invalid against the current state, for example transferring to the same home. |
| `429 Too Many Requests`   | The bounded set of concurrent commands is saturated; retry after one drains.                 |
| `503 Service Unavailable` | This node is not the leader, cannot reach a quorum, or runs no consensus group.              |

A `503` from a non-leader node names the current leader in its body when the group knows one, so a client retries
against it.

## HA planned transfers

The `transfer_authority` command above is the unconditional consensus move a failover commits. To move a *healthy* home
on purpose for a drain, rebalance, or migration, the listener also serves a planned-transfer surface at
`POST /availability/v1/transfers` and `DELETE /availability/v1/transfers/{authority}`, behind the same administration
write scope. A planned transfer waits for the target to catch up before it commits and records who moved the authority
and why. See [planned authority transfer](@/core/availability/planned-transfer.md) for the request shape, the catch-up
gate, cancellation, and the audit record. These components are not a deployable HA runbook until the public and private
peer routes have one reachable member address.

## Request limits and audit

The listener bounds each request body so the command endpoint cannot be handed an unbounded body on the control plane,
and bounds the set of commands in flight so an operator script cannot fan out an unbounded burst of roster rewrites.
Every authenticated request records an audit line naming the actor and the path. Every command records a second audit
line naming the actor, the command kind, its datacenter or authority target, the result, and the committed term and
index, never the request body, so an address or credential never reaches the log. A replayed command is audited as
`replayed` so a retry is distinguishable from a fresh command. Keep the listener behind a management-network boundary
that bounds connection volume; the node applies its request-body, concurrency, and authorization gates on every call.
