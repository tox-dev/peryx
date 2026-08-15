+++
title = "Node liveness"
description = "Track datacenter replica health from bounded heartbeats while leaving the configured roster unchanged."
weight = 7
aliases = [ "/core/availability-liveness/"]
+++

A datacenter replication group is a fixed roster: one writer and its read replicas, set by the
[`[[availability.member]]`](@/core/configuration.md#availability) configuration and changed only by an operator editing
it. Liveness tracking observes how recently each replica reported in, so routing and operators can tell a lagging
replica from a healthy one. It never edits that roster. A missed heartbeat cannot evict a replica, promote a replica to
writer, or transfer authority; only a reviewed configuration edit changes membership.

Heartbeat tasks start only after distributed activation. Shutdown cancels them before joining workers. Configuration
`none` creates no heartbeat state, timer, metric, or task.

## Heartbeats

Each replica sends its health to the group writer at the writer's bearer-authenticated replication endpoint:

```http
POST /+replication/v1/heartbeat
Authorization: Bearer <replication-token>
Content-Type: application/json

{"node": "replica-a", "incarnation": 3, "sequence": 128}
```

The writer accepts a heartbeat only from a configured member. `incarnation` rises when a node restarts and `sequence`
rises with each heartbeat, so the pair orders one node's reports. The writer keeps the latest accepted report per member
and drops any report that does not advance that position. It rejects a report with no bearer credential, a wrong
credential, an unconfigured node, or a body over 4 KiB. The roster size and body cap bound the tracked state.

## Suspicion

The writer ages the most recent accepted heartbeat into one verdict per member:

| Verdict   | Meaning                                                 |
| --------- | ------------------------------------------------------- |
| `alive`   | A heartbeat arrived within the last 15 seconds.         |
| `suspect` | The last heartbeat is between 15 and 45 seconds old.    |
| `dead`    | The last heartbeat is older than 45 seconds.            |
| `unknown` | The member is configured but has no accepted heartbeat. |

Suspicion is derived independently on each observer from the observations it holds, so an asymmetric partition can leave
two writers holding different verdicts for the same replica while neither changes committed membership.

## Reading liveness

An operator or administrator reading the writer's availability health document sees a `peers` array, one entry per
configured replica, with its verdict and last-seen age:

```json
{
  "mode": "dc",
  "role": "primary",
  "ready": true,
  "reasons": [],
  "serial": 42,
  "peers": [
    {
      "node": "replica-a",
      "suspicion": "alive",
      "incarnation": 3,
      "sequence": 128,
      "last_seen_seconds": 2
    }
  ]
}
```

The `peers` field is operator-classified: an unauthenticated caller reading the same document sees only the public mode,
role, and readiness verdict. Peer suspicion is a routing hint. It never gates the writer's own readiness, so a suspect
or dead replica does not remove the writer from a pool or stop it accepting writes.
