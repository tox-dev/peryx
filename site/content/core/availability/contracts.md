+++
title = "Availability contracts"
description = "Shipped durability evidence and the remaining HA contract gaps."
weight = 7
aliases = [ "/core/availability-contracts/"]
+++

The binary accepts three availability modes. `none` skips distributed setup. `dc` activates primary-to-replica
replication without an ownership consensus group. `ha` also assembles ownership consensus, and every HA peer protocol
runs on the one address a roster member carries. See the
[release-status inventory](@/core/availability/_index.md#release-status) before using a mode in production.

| Mode   | Current PyPI upload request acknowledgement                                                      | Coordination                                                        |
| ------ | ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------- |
| `none` | Metadata and bytes committed on the local backend                                                | None                                                                |
| `dc`   | Metadata committed on the writer and bytes satisfy the configured same-DC node-receipt threshold | Asynchronous metadata and blob replication; no ownership consensus  |
| `ha`   | Writer bytes plus metadata applied in any one remote datacenter                                  | HA ownership components; one remote datacenter regardless of policy |

The `ha` write-ack policy does not yet change the remote metadata threshold. `local`, `majority`, and `everywhere` all
accept one remote datacenter. Blob acknowledgements also treat each backend as a filesystem and count node-labelled
receipts, including for a shared object store. Do not infer stronger evidence from either setting.

A filesystem receipt reports that the blob is present after the store call. The filesystem persistence path ignores a
parent-directory sync failure, so that receipt can overstate crash durability on an affected filesystem.

## Mutation contract

The PyPI upload path moves through admission, validation, durable local commit, and acknowledgement. A `200` confirms
the evidence in the table above. A `202` leaves the upload retry-safe because the deadline expired before the resolver
proved that evidence. OCI write paths do not yet call this acknowledgement resolver, so the table does not describe an
OCI success response.

The PyPI crash-recovery finalizer is a separate path. It validates that a placement exists and records the operation as
`published` without calling the distributed acknowledgement resolver. A retry can therefore replay `200 upload accepted`
after that recovery path without the same-DC receipt evidence the synchronous request path requires.

Cache fills are reconstructible and do not wait for authoritative durability. They still verify content before local
commit.

## Partition behavior {#why-a-partition-refuses-instead-of-accepting}

A node reports only durability it can prove. The PyPI path may commit the local blob and metadata before a peer becomes
unreachable; if evidence is still short at the deadline, it returns `202 Accepted` with the stable operation identity. A
retry rechecks that operation instead of publishing another copy. Replica mutation requests and writes at a non-home HA
node return `503 Service Unavailable` before publication.

## Fencing

An HA authority has a monotonic epoch. The current owner may commit under that epoch. The epoch fences a former owner or
stale background job before its result becomes authoritative. `dc` has no ownership epoch; its writer-replacement
procedure relies on stopping the old writer and replacing the store's writer claim offline.

## Read contract

Metadata and bytes advance on separate paths. A replica exposes mutable metadata only through its readable frontier and
serves bytes only when their digest verifies. A lagging replica may return unavailable or not found; it never pairs new
metadata with an old derived view or returns the wrong bytes.

## The frontier bounds staleness {#the-frontier-bounds-staleness}

A frontier is the highest serial a replica or derived view has applied. The readable frontier is the minimum of the
required view frontiers. Serial distance measures and bounds lag without relying on wall-clock time.

## Crash versus storage loss {#crash-versus-storage-loss}

A process crash preserves durable local state and resumes from its recorded frontier. Storage loss removes that failure
domain's copy. The selected mode determines whether another covered copy satisfies recovery or whether restore from
backup is required.

## Recovery objectives {#recovery-objectives}

`none` has the recovery point of its local backend and latest verified backup. A `dc` promotion recovers metadata only
through the selected replica's applied frontier; same-DC byte receipts do not make later metadata synchronous. HA waits
for one remote metadata frontier while bytes converge later, so `majority` and `everywhere` do not yet raise that remote
threshold. Recovery time includes detection, operator action, routing, and catch-up.

## Benchmark method for mode budgets {#benchmark-method-for-mode-budgets}

Measure metadata commit latency, byte-placement latency, replication backlog, and catch-up throughput separately. Size
worker and network budgets from the slowest required stage at expected peak mutation volume, then repeat with one
covered failure domain unavailable.

## Disabled contract

With `mode = "none"`, startup allocates no distributed state, creates no availability-domain tables, and starts no
distributed work. See [high availability](@/core/availability/high-availability.md#the-none-resource-contract).
