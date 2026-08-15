+++
title = "Analytics replication"
description = "Apply daily usage aggregates once per producer interval and report replication completeness."
weight = 8
+++

A node records read usage in daily buckets outside request handling. In `dc` and `ha` modes, producers send sealed
buckets to replicas. A promoted replica can then report totals through its recovery point. `none` mode keeps usage on
the local node.

Analytics uses owner-neutral resource and grouping strings. Producers emit sealed aggregates through the shared
analytics contract; the distributed coordinator transfers and deduplicates them without importing an owner's schema.

## Aggregate rows

Each batch contains additive rows with these fields:

- `day`: UTC day as whole days since the Unix epoch
- `repository` and `resource`: repository and implementation resource
- `group`: implementation grouping key, or an empty value when none applies
- `source`: routed upstream for a cache miss, or an empty value for local content
- `reads` and `bytes`: request and byte totals

The rows contain bounded server labels. They omit client identities, addresses, credentials, and request history.
Addition permits batches to arrive in a different order without changing the sum.

## Producer identity

Each interval carries the producer identity, authority epoch, and sequence. The UTC day is the sequence for daily
batches. A replica records each tuple once and drops a replay without changing totals.

A producer restart reuses the interval identity it stored before restart. A failover advances the epoch, so intervals
from the new authority do not collide with accepted intervals from the former authority. Counters saturate instead of
wrapping.

## Production and transfer

A producer seals a UTC day after the next day starts. It withholds the current day because its buckets can still grow.
The authenticated `+replication/v1/analytics` route returns sealed days after a requested day.

A replica polls from its highest accepted day through a bounded availability worker. It commits totals, accepted
identities, cursors, and frontiers together. A restart resumes from the stored cursor. Transport loss or batch refusal
leaves the cursor unchanged for a later poll.

The producer generation persists across restarts. Replica apply state also carries a schema tag; a build that does not
recognize the tag refuses the snapshot instead of rebuilding totals from zero.

## Deduplication frontier

Replay protection retains accepted interval identities. A combined frontier records the highest sequence acknowledged
across the producer, replica, and backup. Compaction removes identities covered by that frontier because the producer
will not resend them. It does not alter accepted totals.

Two limits reject excess input. A batch cannot exceed its row limit. The retained identity set cannot exceed its
capacity; a new interval waits for frontier compaction to free space.

## Completeness API

`GET /+analytics/completeness` returns accepted totals for a bounded window and a verdict about expected producers. The
handler reads the replica's durable apply state and does not contact producers.

### Filters

- `repository`: optional repository name, limited to 512 bytes
- `from` and `to`: Unix timestamps floored to UTC days
- `limit`: number of day buckets, from 1 through 100 with a default of 25
- `cursor`: opaque `next_cursor` from the prior page

The end defaults to the current UTC day. The start defaults to a trailing month. The server caps the window span.

### Verdicts

The accepted frontier is the highest sealed day accepted from any expected producer. Each producer must reach that day
or the requested window end, whichever comes first.

| Verdict       | Meaning                                                                                  |
| ------------- | ---------------------------------------------------------------------------------------- |
| `complete`    | Each expected producer reached the required day.                                         |
| `delayed`     | Each producer has an accepted frontier, but one or more trail the required day.          |
| `unavailable` | An expected producer has no accepted interval, or the topology has no configured writer. |

The expected set comes from configured writer members. A historical window can be complete while a producer catches up
to newer days. `lag_days` reports the age of the accepted frontier relative to the current day.

### Authorization

A caller with repository access can read the verdict, resolved interval, totals, and day buckets for that repository.
Producer identities, per-producer frontiers, the cluster frontier, required day, and lag require operator authority. An
operator can omit `repository` for a cross-repository query.

### Delayed response

An operator response identifies lagging producers:

```json
{
  "completeness": "delayed",
  "interval": {
    "from_day": 19722,
    "to_day": 19752,
    "retained_from_day": null,
    "window_clamped_to_retention": false
  },
  "totals": {
    "reads": 128,
    "bytes": 64733247
  },
  "buckets": [
    {
      "day": 19752,
      "start_unix": 1706572800,
      "end_unix": 1706659200,
      "reads": 12,
      "bytes": 9000000
    }
  ],
  "next_cursor": null,
  "frontier_day": 19752,
  "required_day": 19752,
  "lag_days": 1,
  "producers": [
    {
      "producer": "east-writer",
      "dc": "east",
      "state": "complete",
      "accepted_epoch": 1,
      "accepted_day": 19752
    },
    {
      "producer": "west-writer",
      "dc": "west",
      "state": "delayed",
      "accepted_epoch": 1,
      "accepted_day": 19750
    }
  ]
}
```

For `unavailable`, a producer with no data has null `accepted_epoch` and `accepted_day`. Repository-scoped responses
omit the producer and frontier fields.

### Pagination and retention

Day buckets page through `next_cursor`. `retained_from_day` gives the usage-retention floor.
`window_clamped_to_retention` is true when the requested start predates that floor. These fields distinguish an empty
window from data removed by retention.

### Backup and monitoring

Backups include accepted totals, producer frontiers, and completeness state at the recovery point. The API fields
`frontier_day`, `lag_days`, and producer `state` provide low-cardinality health signals.

See [Monitoring](@/core/monitor.md) for node-local usage and [Backup and restore](@/core/backup-restore.md) for recovery
points.
