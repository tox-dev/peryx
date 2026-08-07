+++
title = "Availability contracts"
description = "The durability and failure guarantees of none, dc, and ha modes."
weight = 7
+++

The same binary implements three availability contracts. Startup configuration selects one and initializes its
resources.

| Mode   | Acknowledgement                                                            | Failure domain                 | Runtime                                                    |
| ------ | -------------------------------------------------------------------------- | ------------------------------ | ---------------------------------------------------------- |
| `none` | Authoritative metadata and required bytes are durable on the local backend | One process and its storage    | Local coordinator; no distributed resources                |
| `dc`   | The configured same-datacenter durability requirement is satisfied         | One node within the datacenter | Distributed coordinator with same-DC replication           |
| `ha`   | The configured cross-datacenter durability requirement is satisfied        | Loss of a covered datacenter   | Distributed coordinator with cross-DC placement and quorum |

## Mutation contract

An authoritative mutation moves through admission, validation, durable commit, and acknowledgement. A success confirms
the selected mode's metadata and byte-placement requirements. If peryx cannot prove the requirement, it refuses the
mutation or leaves it retry-safe without weakening the configured contract.

Cache fills are reconstructible and do not wait for authoritative durability. They still verify content before local
commit.

## Why a partition refuses instead of accepting {#why-a-partition-refuses-instead-of-accepting}

A node acknowledges only durability it can prove. If required peers or failure domains are unreachable, accepting a
write would weaken the configured contract and could create two authoritative histories. The node refuses the mutation
and leaves the client a retry-safe result instead.

## Fencing

Every distributed authority has a monotonic epoch. The current owner may commit under that epoch. The epoch fences a
former owner or stale background job before its result becomes authoritative. Retrying the same idempotent mutation
against the current owner returns one result.

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

`none` has the recovery point of its local backend and latest verified backup. `dc` protects acknowledged work against a
covered node loss in one datacenter. `ha` protects acknowledged work against a covered datacenter loss. Recovery time
still includes detection, fencing, routing, and catch-up.

## Benchmark method for mode budgets {#benchmark-method-for-mode-budgets}

Measure metadata commit latency, byte-placement latency, replication backlog, and catch-up throughput separately. Size
worker and network budgets from the slowest required stage at expected peak mutation volume, then repeat with one
covered failure domain unavailable.

## Disabled contract

`none` allocates no distributed state and starts no distributed work. This is a runtime configuration guarantee, not a
separate build. See [high availability](@/core/high-availability.md#what-none-costs).

Each ecosystem maps these outcomes to its protocol under [Ecosystems](@/ecosystems/_index.md).
