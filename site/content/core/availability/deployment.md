+++
title = "Availability deployment and sizing"
description = "Deploy none and dc modes and inspect the HA peer-plane gap."
weight = 7
aliases = [ "/core/availability-deployment/"]
+++

peryx accepts three [availability modes](@/core/operations/configuration.md#availability): local (`none`), a
single-datacenter group (`dc`), and a geo-distributed group (`ha`). Modes `none` and `dc` have deployable runtime paths.
HA components ship, but the configured member address cannot reach both the public replication routes and the private
Raft listener, so `ha` has no supported end-to-end network layout. The
[availability contracts](@/core/availability/contracts.md) define each mode's acknowledgement and recovery objectives.
The [`[availability]`](@/core/operations/configuration.md#availability) reference defines the configuration keys. The
sections below cover hardware sizing, TOML validation, and monitoring.

Mode `none` skips availability setup. Mode `dc` starts replication without ownership consensus. Mode `ha` prepares
replication and ownership-consensus components, subject to the peer-plane gap above.

## Choose a shape

Choose a shape by the failure it must survive. The selected shape determines durability, recovery point, and recovery
time. Pick the smallest shape that covers the required failure domain; larger shapes add synchronous write-path cost.

| Shape     | Mode   | Shipped recovery boundary                                  | Recovery point                                  |
| --------- | ------ | ---------------------------------------------------------- | ----------------------------------------------- |
| Unmanaged | `none` | Process restart or operator restore                        | Last external backup on storage loss            |
| Single DC | `dc`   | Replica recovery; offline writer promotion                 | Replica's applied metadata and blob frontiers   |
| Geo HA    | `ha`   | No supported end-to-end deployment while peer planes split | Not a deployable recovery contract this release |

The recovery-point and recovery-time columns follow the [contract's RPO and RTO table][rpo]. A recovery point is a
serial, "no acknowledged mutation at or before frontier *n*". Size a shape by the serials it can recover. A `none`
deployment has one failure domain; do not use it where the service must survive losing that domain.

## Storage capability per shape

A mode that acknowledges a write across nodes needs more evidence than a bare storage success. The `dc` and `ha` modes
require the blob backend to prove two
[durability capabilities](@/core/operations/configuration.md#durability-capabilities): a conditional create-if-absent
write and a checksum-validated write. Startup resolves both once and refuses a `dc` or `ha` mode backed by a store that
declares either missing, naming the guarantee and never the endpoint, bucket, or credentials.

The local filesystem backend proves both capabilities through its atomic no-clobber rename, so it satisfies each mode
and needs no extra declaration. An S3-compatible backend proves what its endpoint honors, not what its vendor brands:
AWS S3 honors `If-None-Match` create-if-absent and validates the SHA-256 checksum on every write, while some gateways
reject the `*` precondition or skip checksum validation. Declare each guarantee your endpoint lacks with
`conditional_writes = false` or `checksum_writes = false` under `[blob]`; a `dc` or `ha` node then refuses to start
rather than acknowledge a cross-node write its store cannot make durable. A `none` node acknowledges from local
durability alone and accepts any backend.

## Size the resources

Each larger shape adds cost over the `none` baseline. Measure the delta with the
[benchmark method for mode budgets][method]: open-loop load, an exact-percentile histogram, separate latency and
throughput gates, and CPU, RSS, allocations, and disk I/O beside each gate. Run the method against your artifact mix and
hardware. The budgets below identify the source of each cost but do not replace measurements.

**CPU.** A `none` node spends CPU on request handling, digest hashing, and background cache maintenance. It adds no
availability work. A replica spends a bounded amount more on its poll-and-apply loop, whose per-cycle apply latency the
`peryx_availability_apply_seconds` histogram measures; a primary spends a bounded amount more serving the replication
journal. Size replication as a fraction of write-path CPU and confirm it against the histogram under representative
write load.

**Memory.** The durable stores are on disk, so steady-state RSS tracks in-flight requests and the freshness cache
instead of catalogue size. A replica holds its sync cursor and a bounded page of pending changes (`page_size`, default
`100`). Its replication memory depends on that page rather than its lag. Provision headroom for the freshness cache and
concurrent uploads first; replication is the smaller term.

**Disk.** Size disk to the artifact working set plus the metadata store, then account for the backend. A
local-filesystem node holds each served blob on its disk, so it needs the full working set. Its mount's crash and
replication behavior defines node durability; put the data directory on a mount that honors `fsync`. A DC-durable object
store holds the blobs off the node, so the node's local disk sizes only to blob staging (`data_dir/blob-staging`), the
metadata store, and the freshness cache, while the bucket carries the working set and provides its own cross-node
durability through versioning or replication. A replica needs disk for the metadata and blobs it has copied to its
frontier, which converges toward the primary's working set.

**Network.** A `none` node uses network for client traffic and upstream cache fills. A `dc` or `ha` group adds the
replication stream between primary and replica, sized by the write rate times the artifact size, plus the metadata
journal, which is small. An `ha` group crosses datacenters, so wide-area latency lands on the write path for each
metadata mutation that needs synchronous acknowledgement. Place the primary where write latency matters and let bytes
converge behind the acknowledgement.

## Stand up each shape

The `none` and `dc` examples below are deployable and contain no secrets. The `ha` example describes the intended
topology and calls out the network contract that prevents deployment. peryx reads credentials from a mounted file
through `token_file`; a configuration snapshot preserves the path without the secret.

### Local availability (`none`)

A `none` process needs no availability table; an omitted `[availability]` and an explicit `mode = "none"` resolve to the
same zero-availability configuration.

```toml
data_dir = "/var/lib/peryx"

[availability]
mode = "none"
```

Set `read_only = true` on an externally populated copy before routing read traffic. Omit `writer_identity` under `none`;
the validator reserves it for managed `dc` and `ha` replication.

Opening the metadata store creates shared tables only. Placement, reclamation, and reconciliation tables are created by
their first write. Consensus state opens during distributed activation, so this shape leaves no distributed schema
behind.

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
dc = "dc-east"
address = "https://a.internal:8443"
role = "writer"

[[availability.member]]
node = "replica-b"
dc = "dc-east"
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

peryx validates the roster at startup and refuses to serve on a blank or duplicated `group`, `node`, or `address`, a
writer count other than one, or a group with no replica. It does not probe an `address`; startup accepts an unreachable
configured peer. Losing the writer stops new writes until it returns or an operator performs the offline
[writer promotion](@/core/availability/high-availability.md#dc-writer-promotion). Mode `dc` runs no ownership consensus
or automatic election.

### Geo HA design (`ha`)

The intended `ha` shape extends the `dc` roster across datacenters. Set `mode = "ha"` and give each member a distinct
`dc`. This configuration passes topology validation, but it cannot form a working cluster in this release: public
replication and receipt routes run on the content server, Raft runs on the private availability listener, and the one
member `address` is used for both transports.

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

The assembled HA acknowledgement resolver waits for any one remote metadata frontier. The write-ack policy does not
raise that threshold, and the split peer planes prevent this from being a deployable durability guarantee.

## Secure the replication path

DC replicas reach a primary over its public HTTPS server, so each member `address` is an `https://` URL and the
replication stream inherits the node's [TLS configuration](@/core/operations/serve-https.md). Terminate TLS at peryx or
at a trusted proxy in front of it; do not expose a plaintext replication endpoint. The shared replication credential
authenticates a follower to the journal and is administrator-managed: mount it as a Docker or Kubernetes secret or a
systemd credential and point `token_file` at the path. peryx reads it at startup and omits it from logs. A
`peryx backup` snapshot records the path rather than the secret. Rotate the credential by replacing the mounted file and
restarting the members that read it. In HA, Raft uses the private listener while the other peer routes remain public; no
supported proxy or bind setting joins those route sets behind the single member address.

## Bootstrap order

Each distributed node validates and prepares resources before activation. Activation starts the control plane and
workers after process startup succeeds. See
[availability lifecycle](@/core/availability/high-availability.md#availability-lifecycle).

Bring a group up writer first. Start the writer, wait for
[`GET /+ready?writes=true`](@/core/availability/high-availability.md#load-balancer-probes) to return `200`, then start
each replica; a replica whose primary is not yet reachable reports `sync_error` on its availability readiness probe and
joins the read pool once its first poll succeeds. Populate a replica's data directory from a verified backup taken at or
before the writer's current frontier before it first polls, so its catch-up copies only the tail of history rather than
the whole catalogue. The [bootstrap administrator](@/core/access/bootstrap-administrator.md) runs on the writer;
replicas serve the identity state they copy.

On Unix, send `SIGTERM` or `SIGINT`; on other platforms, use Ctrl-C. The first signal stops listener acceptance, lets
in-flight requests finish, drains webhook delivery, cancels the local scheduler, and shuts down availability resources.
Peryx logs each resource's shutdown result before the process exits. On Unix, a second `SIGTERM` or `SIGINT` kills the
process if a drain is stuck.

## Monitor each shape

A `dc` or `ha` node mounts two replication-scoped probes beside the public
[`/+health` and `/+ready`](@/core/availability/high-availability.md#load-balancer-probes) probes; a `none` node runs no
availability subsystem and mounts neither. Point a replica read pool at
[`GET /+replication/v1/ready`](@/core/availability/high-availability.md#availability-health-and-readiness) so a lagging
or disconnected replica leaves rotation without a restart, naming its cause in `reasons` (`frontier_lag`, `sync_error`,
`incompatible_schema`, or `blob_store`). Use the public `/+ready?writes=true` for the writer pool.

Scrape [`/metrics`](@/core/operations/metrics.md) for durable signals. Alert on `peryx_ha_distributed_lag`, the
committed-serial distance a replica runs behind its primary, and on a sustained
`rate(peryx_availability_sync_errors_total[5m])` split by its bounded failure class to catch a primary a replica can no
longer reach. `peryx_availability_pending_serials` is the queue depth behind the frontier and moves with the lag. The
[monitor](@/core/operations/monitor.md) page covers the request counters and cache health every shape shares. The
`/+status` operator surface reveals index topology and upstream reachability only to an `administration:read` caller, so
a pending dedicated availability topology page, which later observability work adds, is an operator convenience rather
than the control that keeps the topology off an unauthenticated response.

## What each claim rests on

The [availability contracts](@/core/availability/contracts.md) define the CAP position, per-mode durability, RPO, and
RTO. The [benchmark method][method] measures performance against the `none` baseline. Inline references point to the
storage capability gate, readiness probes, metrics, and promotion command. Use those surfaces for `none` and `dc`
deployments. Treat the HA sizing material as design guidance until its peer network has one deployable contract.

## Related

- What each mode promises and refuses: [availability contracts](@/core/availability/contracts.md)
- Every configuration key these examples use: [`[availability]`](@/core/operations/configuration.md#availability)
- Operate each availability mode: [high availability](@/core/availability/high-availability.md)
- The replication and availability series to alert on: [metrics reference](@/core/operations/metrics.md)
- Serve the HTTPS listener replicas follow: [serve over HTTPS](@/core/operations/serve-https.md)

[method]: @/core/availability/contracts.md#benchmark-method-for-mode-budgets
[rpo]: @/core/availability/contracts.md#recovery-objectives
