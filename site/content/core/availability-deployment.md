+++
title = "Availability deployment and sizing"
description = "Stand up and size the none, single-DC, and geo HA shapes: validated TOML, storage capability, resource budgets, TLS, bootstrap order, and monitoring per mode."
weight = 7
+++

This page stands up and sizes the three deployment shapes peryx names, one per
[availability mode](@/core/configuration.md#availability): a single node (`none`), a single-datacenter group (`dc`), and
a geo-distributed group (`ha`). It reads two neighbors rather than restating them. The
[availability contracts](@/core/availability-contracts.md) fix what each mode's acknowledgement promises and its
recovery objectives; the [`[availability]`](@/core/configuration.md#availability) reference fixes every configuration
key. This page decides how much hardware each shape needs, how to validate its TOML, and how to watch it.

`none` uses the local coordinator. `dc` and `ha` use distributed coordination and initialize the replication, topology,
and reconciliation resources their configuration requires.

## Choose a shape

A shape is chosen by the failure it must survive, and that choice fixes the durability, recovery point, and recovery
time the contract already defines. Pick the smallest shape that covers the failure domain you are required to tolerate,
because each larger shape pays synchronous cost on the write path.

| Shape | Mode | Survives | Recovery point | Recovery time | | ----------- | ------ | ----------------------------- |
-------------------------------------------------- | ----------------------------------------- | | Single node | `none`
| process crash, storage intact | last external backup on storage loss | operator restore and [promotion][promote] | |
Single DC | `dc` | loss of one node in the DC | zero acknowledged metadata and bytes in the DC | failover to the
surviving in-DC node | | Geo HA | `ha` | loss of a whole datacenter | zero acknowledged metadata; only unconverged bytes
| failover to a surviving datacenter |

The recovery-point and recovery-time columns are the [contract's RPO and RTO table][rpo]: a recovery point is a serial,
"no acknowledged mutation at or before frontier *n*", not a duration, so a shape is sized by which serials it can
recover rather than by a wall-clock estimate. A single node is a single failure domain by design; do not deploy it where
the requirement is to survive losing that domain.

## Storage capability per shape

A mode that acknowledges a write across nodes cannot treat a bare storage success as proof, so `dc` and `ha` require the
blob backend to prove two [durability capabilities](@/core/configuration.md#durability-capabilities): a conditional
create-if-absent write and a checksum-validated write. Startup resolves both once and refuses a `dc` or `ha` mode backed
by a store that declares either missing, naming the guarantee and never the endpoint, bucket, or credentials.

The local filesystem backend proves both capabilities through its atomic no-clobber rename, so it satisfies every mode
and needs no extra declaration. An S3-compatible backend proves what its endpoint honors, not what its vendor brands:
AWS S3 honors `If-None-Match` create-if-absent and validates the SHA-256 checksum on every write, while some gateways
reject the `*` precondition or skip checksum validation. Declare each guarantee your endpoint lacks with
`conditional_writes = false` or `checksum_writes = false` under `[blob]`; a `dc` or `ha` node then refuses to start
rather than acknowledge a cross-node write its store cannot make durable. A `none` node acknowledges from local
durability alone and accepts any backend.

## Size the resources

Each larger shape adds a marginal cost over `none`, which is the zero-durability baseline, so size by the delta a mode
adds rather than by an absolute number hardware alone can move. The [benchmark method for mode budgets][method] fixes
how that delta is measured: open-loop load, an exact-percentile histogram, latency and throughput as separate gates, and
CPU, RSS, allocations, and disk I/O reported beside each gate. Size a shape by running that method against your own
artifact mix and hardware; the budgets below are the shape of the cost, not a substitute measurement.

**CPU.** A `none` node spends CPU on request handling, digest hashing, and background cache maintenance, and adds no
availability work. A replica spends a bounded amount more on its poll-and-apply loop, whose per-cycle apply latency the
`peryx_availability_apply_seconds` histogram measures; a primary spends a bounded amount more serving the replication
journal. Size replication as a marginal fraction of the write-path CPU, not a fixed core count, and confirm it against
the histogram under representative write load.

**Memory.** The durable stores are on disk, so steady-state RSS tracks in-flight requests and the freshness cache, not
the catalogue size. A replica holds its sync cursor and a bounded page of pending changes (`page_size`, default `100`),
so its replication memory is bounded by that page rather than by how far behind it runs. Provision headroom for the
freshness cache and concurrent uploads first; replication is the smaller term.

**Disk.** Size disk to the artifact working set plus the metadata store, then split the guidance by backend. A
local-filesystem node holds every served blob on its own disk, so its disk is the full working set and its mount's crash
and replication behavior is the node's durability; put the data directory on a mount whose `fsync` is honored. A
DC-durable object store holds the blobs off the node, so the node's local disk sizes only to blob staging
(`data_dir/blob-staging`), the metadata store, and the freshness cache, while the bucket carries the working set and
provides its own cross-node durability through versioning or replication. A replica needs disk for the metadata and
blobs it has copied to its frontier, which converges toward the primary's working set.

**Network.** A `none` node uses network only for client traffic and upstream cache fills. A `dc` or `ha` group adds the
replication stream between primary and replica, sized by the write rate times the artifact size, plus the metadata
journal, which is small. An `ha` group crosses datacenters, so its replication link is the wide-area path whose latency
lands on the write path for every synchronously acknowledged metadata mutation; place the primary where write latency
matters and let bytes converge behind the acknowledgement.

## Stand up each shape

Every example below is a complete, secret-free configuration: credentials are read from a mounted file with
`token_file`, never inlined, and a configuration snapshot preserves that path rather than the secret behind it.

### Single node (`none`)

A single node needs no availability table at all; an omitted `[availability]` and an explicit `mode = "none"` resolve to
the same zero-availability configuration.

```toml
data_dir = "/var/lib/peryx"

[availability]
mode = "none"
```

Give the writer a distinct, stable [`writer_identity`](@/core/high-availability.md) so a restored copy cannot start as a
second writer, and populate any read replica's data directory from a verified backup before routing traffic. This is the
shape the [high availability](@/core/high-availability.md) page operates end to end today.

### Single DC (`dc`)

A `dc` group is one writer and its replicas within one datacenter, declared as a static roster. peryx never infers a
member from a broadcast and never lets a liveness timeout promote a replica, so membership is an explicit, reviewed
configuration edit. The writer serves the replication journal; each replica follows it and refuses client mutations like
a `read_only` node.

```toml
data_dir = "/var/lib/peryx"

[availability]
mode = "dc"
group = "east"

[availability.replication]
role = "primary"
source = "writer-a"
token_file = "/run/secrets/replication-token"

[[availability.member]]
node = "writer-a"
dc = "dc-east-1"
address = "https://a.internal:8443"
role = "writer"

[[availability.member]]
node = "replica-b"
dc = "dc-east-2"
address = "https://b.internal:8443"
role = "replica"
```

Each replica carries the matching replica role pointed at the writer's URL:

```toml
data_dir = "/var/lib/peryx"

[availability]
mode = "dc"
group = "east"

[availability.replication]
role = "replica"
upstream = "https://a.internal:8443"
token_file = "/run/secrets/replication-token"
poll_interval_secs = 1
page_size = 100
```

peryx validates the roster at startup and refuses to serve on any violation: a blank or duplicated `group`, `node`,
`dc`, or `address`; anything other than exactly one `writer`; or a group with no replica. It never probes an `address`,
so an unreachable configured peer is a valid topology rather than a configuration error. "Quorum" here is the configured
group, not a vote: losing the writer stops new writes until it returns or an operator runs the fenced
[transfer procedure](@/core/high-availability.md#manual-promotion), because peryx runs no automatic election.

### Geo HA (`ha`)

The `ha` shape is the `dc` roster stretched across datacenters: set `mode = "ha"` and give each member a distinct `dc`.
The roster keys already carry the geography, so the only change from `dc` is the mode and the members' locations.

```toml
data_dir = "/var/lib/peryx"

[availability]
mode = "ha"
group = "global"

[availability.replication]
role = "primary"
source = "writer-east"
token_file = "/run/secrets/replication-token"

[[availability.member]]
node = "writer-east"
dc = "us-east"
address = "https://east.internal:8443"
role = "writer"

[[availability.member]]
node = "replica-west"
dc = "us-west"
address = "https://west.internal:8443"
role = "replica"
```

Under `ha`, a metadata mutation acknowledges once it is durable in a remote datacenter and its bytes converge behind the
acknowledgement, so the wide-area link's latency lands on the metadata write while byte transfer runs asynchronously.
Place the writer in the datacenter whose write latency you most need to protect.

## Secure the replication path

Replicas reach a primary over its HTTPS listener, so every member `address` is an `https://` URL and the replication
stream inherits the node's [TLS configuration](@/core/serve-https.md). Terminate TLS at peryx or at a trusted proxy in
front of it; do not expose a plaintext replication endpoint. The shared replication credential authenticates a follower
to the journal and is administrator-managed: mount it as a Docker or Kubernetes secret or a systemd credential and point
`token_file` at the path. peryx reads it at startup, never logs it, and a `peryx backup` snapshot records the path
rather than the secret. Rotate the credential by replacing the mounted file and restarting the members that read it.

## Bootstrap order

Bring a group up writer first. Start the writer, wait for
[`GET /+ready?writes=true`](@/core/high-availability.md#load-balancer-probes) to return `200`, then start each replica;
a replica whose primary is not yet reachable reports `sync_error` on its availability readiness probe and joins the read
pool once its first poll succeeds. Populate a replica's data directory from a verified backup taken at or before the
writer's current frontier before it first polls, so its catch-up copies only the tail of history rather than the whole
catalogue. The [bootstrap administrator](@/core/bootstrap-administrator.md) runs on the writer; replicas serve the
identity state they copy.

## Monitor each shape

A `dc` or `ha` node mounts two replication-scoped probes beside the public
[`/+health` and `/+ready`](@/core/high-availability.md#load-balancer-probes) probes; a `none` node runs no availability
subsystem and mounts neither. Point a replica read pool at
[`GET /+replication/v1/ready`](@/core/high-availability.md#availability-health-and-readiness) so a lagging or
disconnected replica leaves rotation without a restart, naming its cause in `reasons` (`frontier_lag`, `sync_error`,
`incompatible_schema`, or `blob_store`). Use the public `/+ready?writes=true` for the writer pool.

Scrape [`/metrics`](@/core/metrics.md) for the durable signals. Alert on `peryx_ha_distributed_lag`, the
committed-serial distance a replica runs behind its primary, and on a sustained
`rate(peryx_availability_sync_errors_total[5m])` split by its bounded failure class to catch a primary a replica can no
longer reach. `peryx_availability_pending_serials` is the queue depth behind the frontier and moves with the lag. The
[monitor](@/core/monitor.md) page covers the request counters and cache health every shape shares. The `/+status`
operator surface reveals index topology and upstream reachability only to an `administration:read` caller, so a pending
dedicated availability topology page, which later observability work adds, is an operator convenience rather than the
control that keeps the topology off an unauthenticated response.

## What each claim rests on

Every strong claim on this page is owned by an implemented surface rather than asserted here. The CAP position, the
per-mode durability, and the RPO and RTO come from the [availability contracts](@/core/availability-contracts.md), which
fix them as the promise an implementation is judged against. The performance budgets come from the
[benchmark method][method], measured against `none` as the baseline, never a number this guide invents. The storage
capability gate, the readiness probes, the metrics, and the promotion command are shipped and cited inline. Deploy
`none` against these surfaces today, and size a `dc` or `ha` group against the same contract so the deployment is ready
when its replicating runtime lands.

## Related

- What each mode promises and refuses: [availability contracts](@/core/availability-contracts.md)
- Every configuration key these examples use: [`[availability]`](@/core/configuration.md#availability)
- Operate the single-writer shape today: [high availability](@/core/high-availability.md)
- The replication and availability series to alert on: [metrics reference](@/core/metrics.md)
- Serve the HTTPS listener replicas follow: [serve over HTTPS](@/core/serve-https.md)

[method]: @/core/availability-contracts.md#benchmark-method-for-mode-budgets
[promote]: @/core/high-availability.md#manual-promotion
[rpo]: @/core/availability-contracts.md#recovery-objectives
