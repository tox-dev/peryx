+++
title = "Home assignment on first publish"
description = "Assign one authority home through the control quorum and fence later writes with its epoch."
weight = 8
+++

An authority owns a set of related writes and has one home datacenter. The first successful publication assigns that
home and mints its first epoch. Later writes must pass the current epoch fence.

[Authority transfer](@/core/availability-authority-transfer.md) moves an assigned home. The
[availability contracts](@/core/availability-contracts.md) define the durability required during assignment and
transfer.

## Authority identity

The ecosystem driver derives a canonical authority key from the client-visible identity. Name variants that identify the
same subject must produce one key. Distinct ecosystems use disjoint keyspaces.

Key derivation belongs to the ecosystem references:

- [Python package authority keys](@/ecosystems/pypi/reference/availability.md#authority-keys)
- [OCI authority keys](@/ecosystems/oci/reference/availability.md#authority-keys)

## Home selection

The ingress datacenter for the first successful publication becomes the home. It is already a live member of the
committed cluster because it accepted the request. Assignment needs no separate placement pass.

## Concurrent assignment

The control quorum applies a compare-and-set command to an unassigned authority. The first committed command records the
home and epoch. A later command for the same authority returns the committed assignment without replacing it.

{% mermaid() %}
flowchart TB
p1["DC east: first publish"] --> cas{"authority homed?"}
p2["DC west: first publish"] --> cas
p3["DC north: first publish"] --> cas
cas -->|"no: first to commit"| win["assign home, mint epoch one"]
cas -->|"yes: already homed"| lose["reject: keep the committed home"]
class win good
class lose warn
{% end %}

The winning command records its cause, leader term, log index, and minted epoch. Ownership snapshots retain that audit
record. Rejected commands do not replace it.

## Retries and partitions

A datacenter in a control-plane minority cannot commit an assignment. It forwards the claim to the leader and does not
assign a local home. A restart or partition repair reads the quorum result and uses the committed winner.

A failed best-effort claim does not publish conflicting authority state. A later publication can repeat the claim after
quorum access returns.

## Related

- [Authority transfer and drain](@/core/availability-authority-transfer.md)
- [Availability contracts](@/core/availability-contracts.md)
- [Node liveness](@/core/availability-liveness.md)
