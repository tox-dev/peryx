+++
title = "Availability modes"
description = "Configure none and dc modes and inspect the HA component boundary."
weight = 7
aliases = [ "/core/high-availability/"]
+++

An omitted `[availability]` table or `mode = "none"` starts no distributed work. Mode `dc` starts metadata and blob
replication without ownership consensus. Mode `ha` assembles ownership-consensus components, which peers reach at the
one address the roster names for a member. See the [release-status table](@/core/availability/_index.md#release-status)
before choosing a mode.

In distributed modes, send mutation traffic to the writer. Managed workers copy committed metadata and artifact bytes to
replicas, which reject mutation requests with `503 Service Unavailable`. The
[availability contract](@/core/availability/contracts.md) defines what each acknowledgement proves.

## Availability lifecycle

Startup validates settings and reserves listeners before it starts availability work. Mode `dc` starts replication and
reconciliation. Mode `ha` also starts ownership consensus. If one service fails, startup stops the services it started
and returns an error. The process does not accept artifact requests in this state.

During shutdown, Peryx stops accepting work, cancels background operations, and waits for availability services within a
bounded deadline. It reports any service that misses the deadline. Mode `none` skips these steps and adds no
availability work to artifact requests.

Contributors can read [runtime architecture](@/contributing/runtime-architecture.md) for startup ownership and cleanup.

## Read-only processes

`read_only` rejects mutations independently of the availability mode:

```toml
read_only = true
```

```shell
PERYX_READ_ONLY=true peryx serve --config peryx.toml
peryx serve --config peryx.toml --read-only
```

The environment variable and command-line flag provide the same setting. Under `none`, populate the data directory from
a verified backup or external replication before routing reads, and omit `writer_identity`. `none` does not copy data,
manage membership, or coordinate failover.

Managed `dc` and `ha` replicas receive data through `[availability.replication]`. Those modes accept `writer_identity`
as a writer-claim guard; `none` rejects it.

## Load-balancer probes

`GET /+health` is the liveness probe. It returns `200 OK` with `{"status":"live"}` while the HTTP process can answer.
Metadata, blob-store, and upstream failures do not fail liveness because a restart cannot repair those dependencies.

`GET /+ready` checks the local metadata store and blob-store root used by artifact requests. It returns `200 OK` with
`{"status":"ready"}` or `503 Service Unavailable` with `{"status":"not_ready"}`. It does not scan metadata, enumerate
repositories, or contact an upstream. `GET /+ready?writes=true` also requires a writer; replicas return `503` for that
query while remaining ready for reads.

Both public probes are anonymous, bypass the hosted request limiter, and send `Cache-Control: no-store`. Their fixed
documents contain no repository, upstream, user, topology, or failure details. `GET /+status` is the detailed operator
surface: it stays reachable anonymously for coarse health, adds the process counters for `operator:read`, and reveals
the index topology and upstream reachability only for `administration:read`. That per-class filtering already keeps the
topology off an unauthenticated response, so an ingress rule is defense in depth rather than the primary control.

For [Kubernetes probes](https://kubernetes.io/docs/concepts/workloads/pods/probes/), let readiness remove a pod from
service before liveness restarts it:

```yaml
livenessProbe:
  httpGet:
    path: /+health
    port: 4433
  periodSeconds: 10
  failureThreshold: 3
readinessProbe:
  httpGet:
    path: /+ready
    port: 4433
  periodSeconds: 5
  failureThreshold: 2
```

A generic load balancer should use readiness to select backends. For example, an
[HAProxy HTTP health check](https://www.haproxy.com/documentation/haproxy-configuration-tutorials/reliability/health-checks/)
can use the same route for a read pool:

```haproxy
backend peryx-readers
    option httpchk GET /+ready
    http-check expect status 200
    server peryx-1 10.0.0.11:4433 check
    server peryx-2 10.0.0.12:4433 check
```

Use `/+ready?writes=true` for the writer pool. Do not use `/+health` for load balancing because it detects a process
that cannot answer at all, so it remains successful during recoverable dependency failures.

## Availability health and readiness

A `dc` or `ha` node serves two more probes scoped to replication itself. A `none` node runs no availability subsystem,
so it mounts neither.

`GET /+replication/v1/health` returns `200 OK` in both distributed modes.

`GET /+replication/v1/ready` is the availability readiness probe. It answers `200 OK` when the node can serve at its
frontier and `503 Service Unavailable` otherwise, naming every cause in `reasons`:

- `blob_store`: the mounted blob store failed its reachability check, so the mount cannot answer artifact requests.
- `frontier_lag`: a replica has not yet reached the writer's latest observed serial.
- `sync_error`: a replica's last poll of its writer failed.
- `incompatible_schema`: a replica's writer speaks an unsupported replication protocol version, which a later poll
  cannot resolve without upgrading the writer.
- `worker_unhealthy`: a background availability task panicked. The node keeps answering the reads it can still satisfy,
  but readiness reports the fault until the process restarts.

Both documents are filtered to the caller's class, like `/+status`. Any caller reads `mode`, `role`, `ready`, and
`reasons`. `operator:read` adds a replica's `serial`, `primary_serial`, `lag`, and synced counters, or a writer's own
`serial`. `administration:read` adds the redacted `upstream` origin a replica follows, with credentials, query, and
fragment removed. An anonymous or repository-only caller never reads a serial, lag, or peer origin, so the topology
stays off an unauthenticated response. Both probes send `Cache-Control: no-store`.

Point a replica read pool at readiness so a lagging or disconnected replica leaves rotation without a restart:

```haproxy
backend peryx-replicas
    option httpchk GET /+replication/v1/ready
    http-check expect status 200
    server replica-1 10.0.0.21:4433 check
    server replica-2 10.0.0.22:4433 check
```

When readiness reports `frontier_lag`, compare the replica's `lag` against the writer's write rate: a lag that never
reaches zero points at a stalled poll, which readiness reports as `sync_error` once a poll fails. An
`incompatible_schema` reason means the writer and replica were built against different replication protocol versions;
upgrade the writer before routing reads to that replica.

### Distributed group readiness

A `dc` or `ha` writer that names a member roster folds the group's frontiers into one verdict on its own
`GET /+replication/v1/ready`, under a `group_readiness` field an `operator:read` caller reads. The writer knows its own
applied `serial`. Each replica sends its highest applied serial to the writer through authenticated
`POST /+replication/v1/heartbeat` requests, so the writer aggregates the group without dialing a replica. A replica
without `node_identity` sends no heartbeat and counts as not reporting.

The field carries four values. `ready` is whether the group can acknowledge a new write under its durability policy.
`durable_frontier` is the highest serial the policy's required number of members have all applied, the serial the group
guarantees is durable. `policy` is the configured write-ack policy: `local` requires the writer, `majority` requires a
strict majority of configured members, and `everywhere` requires every configured member. `blocked` is `null` when the
group is ready, otherwise the reason it is not: `writer_lost` when no writer is reporting, or `insufficient_members`
with the `reporting` and `required` counts when a writer is present but too few members are.

Membership is the fixed configured roster, so a vanished replica reads as one that is not reporting rather than
shrinking the required count into a smaller one. A single lagging or lost replica never blocks readiness while the rest
still meet the policy, and a serial a majority already holds stays durable even when a lost writer stops new writes from
being acknowledged. Losing a DC writer stops new writes until it returns or an operator performs an offline
[promotion](#dc-writer-promotion); no replica promotes itself on a timeout.

A replica refuses every client mutation with `503 Service Unavailable` and `{"error":"read_only_replica"}` ahead of any
handler, so a misrouted write fails closed rather than diverging a copy. A restarted replica resumes from the frontier
it durably stored, re-applying only the pages past it, and beats its recovered frontier on its next beacon, so a restart
rejoins the group's readiness without re-copying the journal.

## Peer change feed endpoint

A `dc` or `ha` node serves change pages to authenticated peers at `GET /+replication/v1/changes`. Building a page reads
the metadata journal and encodes every record in it, so the node runs that work on a blocking worker rather than on the
request loop, and bounds how many pages it builds at once. A replica relaying the feed reads its own applied source
identity on the same worker, under the same bound.

A request that arrives while every slot is held receives `503 Service Unavailable` with `Retry-After` before the node
opens the store, so a fan-in of catching-up peers costs a refusal rather than a queue of waiting reads. A build that has
already started keeps its slot until it returns, because a started blocking task cannot be cancelled; a peer that
disconnects instead stops that build at the next record boundary rather than paying for the rest of the page.

## Peer artifact byte endpoint

A `dc` or `ha` writer serves artifact bytes to authenticated peers at `GET /+replication/v1/blobs/sha256/{digest}`. The
endpoint carries the same bearer token as the change feed; a request without it, or with a wrong one, receives
`401 Unauthorized` and a `WWW-Authenticate: Bearer` challenge. This is a private plane between nodes, not a public
download path.

The endpoint serves a committed blob by its digest; a digest the store does not hold reads as `404 Not Found`. A served
response carries `ETag: "sha256:<digest>"` as checksum evidence and `Cache-Control: private, no-store`. A peer selects
which verified placements to request from its own routing metadata; the endpoint hands over bytes rather than deciding
placement.

A peer fetches the whole object or a byte range. The endpoint advertises `Accept-Ranges: bytes` and honors a single
`Range: bytes=first-last` per [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-range-requests): a satisfiable
range returns `206 Partial Content` with `Content-Range` and `Content-Length`, a well-formed but unmeetable range
returns `416 Range Not Satisfiable` naming the size, and a malformed range falls back to the whole object.

The writer bounds how many byte streams it serves at once. A request that arrives while every slot is held receives
`503 Service Unavailable` with `Retry-After`, so a burst of slow or abandoned readers cannot exhaust the file handles,
sockets, and buffers a stream pins. A stream releases its slot the moment it finishes, its reader cancels, or the
connection drops, so a stalled peer frees capacity for the next without waiting on a timeout.

Responses and logs stay clear of peer credentials and internal addresses. The endpoint returns bytes, a status, and the
size and digest headers a range needs, and nothing about the token, the requesting peer, or the store's filesystem
paths.

## Peer durability receipt endpoint

A `dc` or `ha` node serves `GET /+replication/v1/receipts/sha256/{digest}` on the public server. The receipt client
queries the configured addresses of other members in the same datacenter with the replication bearer token. A present
blob returns `200 OK` naming the node that answered, the digest it read, and that blob's size; an absent blob returns
`404 Not Found`, and a malformed digest returns `400 Bad Request`. A node that has no configured identity serves no
receipt route at all, because it cannot name itself in an answer.

The client holds every `200` to the member its source was configured for: the node, digest, and size must all be the
ones it asked about. Anything else is a protocol failure that contributes no receipt and retires that source for the
rest of the write. Two configured addresses that reach one process therefore yield one receipt rather than two, and a
replaced node stops contributing under its predecessor's name until the roster names it. The shared bearer token proves
group membership, not which member answered, so only this binding keeps one process out of two receipt slots.

The current resolver uses this node-receipt path for object stores as well as filesystems; it does not construct
object-store-specific evidence. The filesystem persistence path also ignores a parent-directory sync failure before this
receipt can be issued, which can overstate crash durability. This route is an internal peer operation and is not part of
the public OpenAPI document.

## Availability topology snapshot

`GET /+availability/topology` returns one caller-filtered snapshot of the configured topology. Handlers read the
snapshot instead of traversing live membership and storage state. Distributed nodes mount the route; `none` does not.

The snapshot names the `mode`, the `group`, and a `nodes` roster drawn from the
[`[[availability.member]]`](@/core/operations/configuration.md#availability) configuration. Each roster entry carries
its `node` identity, `dc`, `role`, and a `local` flag marking the node that produced the snapshot. A `local` block
reports this node's own live self-observation, which the process always knows: its `role`, its `liveness`, and the
metadata `frontier` it has committed. `captured_at` dates the snapshot in Unix seconds, and `node_count` reports the
full roster size when the `nodes` list is capped, so a stale or truncated render is visible rather than passing for a
healthy, complete one.

The topology snapshot reports live state only for the local node. Peer entries use `unknown` without a frontier. Read
heartbeat-derived peer liveness from the writer's `/+replication/v1/ready` or `/+replication/v1/health` response. The
local node reports `live` when its metadata and blob stores can serve and `unready` otherwise.

Fields are filtered to the caller's class, like `/+status`. Any caller reads `mode`, `group`, `captured_at`,
`node_count`, and each node's `node`, `dc`, `role`, and `local` flag. `operator:read` adds the `liveness` of every node
and the local `frontier`. `administration:read` adds each node's advertised `address`. An anonymous or repository-only
caller never reads a liveness, frontier, or peer address. The response sends `Cache-Control: no-store`, and the node
list is capped so one request cannot return an unbounded roster.

## The `none` resource contract

Configuration `none` builds no availability record, route, metric, background client, task, timer, queue, thread,
prepared handle, or active handle. It mounts none of the `/+replication` routes and registers no
`peryx_ha_distributed_*` or `peryx_availability_*` metric family. An omitted `[availability]` table and `mode = "none"`
resolve to the same process.

Ordinary work keeps running. General request metrics such as `peryx_requests_total` and the node-local `peryx_jobs_*`
counters stay present, background maintenance still runs, and writes keep their local-durability acknowledgement.
Selecting `none` removes distributed availability cost without disabling general metrics or local jobs.

The metadata store creates shared tables when it opens. Placement, reclamation, and reconciliation tables do not exist
until their first write. Consensus opens its own state during distributed activation. A `none` process therefore creates
no distributed persistence as a side effect of opening storage.

## Blob reclamation

Reclamation first records a tombstone at the current metadata serial. It marks that tombstone ready only after every
configured durability plane covers the serial and a final reference check still finds the blob unused.

The live replica frontier is the lowest serial reported by any configured replica. A missing replica report blocks
finalization. A plane that is not configured contributes no frontier; it is absent rather than represented by zero.
Future backup implementations provide their observed serial through the `ReclamationFrontiers` contract.

## Background worker runtime

A `dc` or `ha` replica runs changelog apply and blob copies on a dedicated runtime, apart from artifact traffic. A
`none` node builds no worker threads, queue, or metrics.

The runtime starts one worker thread per CPU the process is scheduled on, capped at four, so a container pinned to two
cores starts two workers and a large host does not start one background worker per core. A separate blocking pool,
capped at eight threads, absorbs filesystem writes and checksum work without touching the foreground executor. Both caps
are deliberate: replication is a throughput-bounded trickle whose job is to stay out of the way of the request path, not
to consume the machine.

Background tasks draw from a fixed set of concurrency slots. The resident apply loop holds one for the process lifetime;
the rest bound the copy and apply work a replica issues. A full set returns backpressure to the caller rather than
queueing without limit, and every refused submission increments `peryx_availability_worker_rejected_total` so a
sustained rejection rate is visible rather than silent. `peryx_availability_worker_slots_active` against
`peryx_availability_worker_slots` shows how close the runtime runs to saturation.

A panicked task releases its slot, increments `peryx_availability_worker_panics_total`, and marks the worker domain
unhealthy. Readiness then reports `worker_unhealthy`. Shutdown cancels workers and waits up to the lifecycle deadline.
An unfinished join moves to the process reaper. A replica reapplies any uncommitted page after restart.

## Fenced cluster jobs

Background jobs carry an authority fence so an old owner cannot mutate authoritative metadata or destructive storage
state after ownership has moved. A job declares one of two ownership scopes, and the runner applies the matching fence
before it runs the work and again before it counts the result.

A **node-local** job, such as cache maintenance, a search rebuild, or a repository sweep, runs on every node
independently and takes no control-plane lease. A per-repository job is already fenced by its repository's authority
epoch: the run leases the committed epoch when it starts and its success is rejected if the authority transferred while
it ran, so a former home's late write loses to the new home's. A node-wide node-local job that names no repository holds
the closed `0` sentinel and is never fenced, and it makes no control-plane call at all.

A **cluster-singleton** job runs on one node cluster-wide, and every decision about who owns it commits through the
ownership group's replicated log before a worker enters the job body. Nodes cannot contend through their own metadata
stores: each process opens its own `peryx.redb`, so a node-local claim would grant the same job on every node at once.

Each process incarnation mints one holder token at startup, from its process ID and a random half. Two containers can
both be PID 1, and a restart inherits its predecessor's PID, so the process ID by itself names the wrong thing. The
random half gives each incarnation its own identity, and nothing writes it down, so a restarted process cannot replay
the token its predecessor held.

A grant carries three identifiers: the **holder** token, the consensus **term** the claim committed under, and a
**generation** that rises with every grant of that job. One leader can grant the same job to two holders in sequence
within a single term, which holder and term together cannot separate. The generation does, so a delayed request from the
first grant is refused even though it carries the current term.

The authority decides when a grant lapses, from committed lease state and the leader's clock; a worker's own clock
reading cannot extend its ownership. A claim against an unlapsed grant loses, and the run is recorded as failed with
`lease_not_held` rather than started. The holder renews for as long as the body runs, so a run longer than one lease
period stays exclusive. A renewal the authority refuses means ownership has moved, and the run is cancelled as cleanup.
A renewal that cannot reach consensus is retried, since only committed state decides whether the grant has lapsed.

Completion presents holder, term, and generation back to the authority. If any of the three no longer matches, the run's
outcome is rejected with `lease_fenced` rather than counted. The body's outcome and the lease cleanup stay separate: a
release that cannot reach the group is logged and the grant is left to lapse, and a finished body keeps the result it
produced.

Modes `none` and `dc` run no ownership group. One such process is the whole cluster and has nothing to contend with, so
it runs singleton kinds under the closed `0` sentinel with no lease, renewal, or fence lookup.

Fenced runs are visible through the ordinary `peryx_jobs_*` lifecycle counters: a fenced-before-start or superseded run
increments the `failed` outcome for its kind, and its durable run record carries the `lease_not_held` or `lease_fenced`
reason. To convert a node-local kind to a cluster singleton, return `LeaseScope::ClusterSingleton` with the singleton
key from the job's `lease_scope`. The runner then leases and fences it with no further wiring.

## DC writer promotion

1. Stop or fence the old writer so it cannot accept another mutation.

1. Finish copying its metadata and blobs to the selected replica and verify the copy.

1. With the replica stopped and still configured with the old identity, replace the store's writer claim:

   ```shell
   peryx writer promote writer-b --config peryx.toml
   ```

   The command compares the configured identity with the store's current claim and refuses a stale or missing value.

1. Set `writer_identity = "writer-b"`, remove replica mode, and start the selected replica.

1. Wait for `GET /+ready?writes=true` to return `200`, then move write traffic to it.

1. Rebuild former writer nodes as replicas before returning them to service.

Promotion changes the store's claim; it does not copy data or stop the old process. Fence the old writer before
promotion, and do not start two writers against copies that can diverge.

## Related

- Size and stand up each shape: [availability deployment and sizing](@/core/availability/deployment.md)
- What each mode's acknowledgement promises: [availability contracts](@/core/availability/contracts.md)
- The mode and replication keys: [`[availability]`](@/core/operations/configuration.md#availability)
