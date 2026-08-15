+++
title = "Blob placement"
description = "Record verified content locations and route reads from them."
weight = 10
+++

A blob placement is durable evidence that a content digest exists at one backend location in one datacenter. Routers use
placements to serve local bytes, stream from a peer, wait for transfer, or refuse the read. The per-digest
[artifact source and availability](@/core/artifact-source.md) projection reports whether one instance can serve bytes;
the placement ledger records every known location and state.

This contract covers ledger state and routing queries. Transfer, peer streaming, and operator UI contracts are separate.

`peryx-ha` owns placement values and the `BlobPlacementStore` and `ArtifactPlacementStore` traits. Storage implements
their atomic operations. The distributed coordinator owns transition policy, copying, routing, and reconciliation.
Content owners use the traits without adding owner fields to the ledger.

## The placement key

Each placement is keyed by four parts:

- the content digest, canonical `sha256:<64 lowercase hex>`;
- the backend it lives on, such as `filesystem` or `s3`;
- the datacenter that holds it, an opaque operator-assigned label;
- the location inside that backend: an object key or a store-relative path.

The same digest can hold up to 64 placements, so one artifact can be local on one backend, verified on a peer, and
mid-transfer on a third without the rows colliding. A stage that would exceed the bound is refused rather than growing
an unbounded history.

## States and evidence

A placement moves through an evidence-based lifecycle. A staged temporary file is never treated as serveable; only a
backend-confirmed digest is.

- **Pending**: a transfer is in flight or a file is staged. No serving evidence exists.
- **Verified**: the backend confirmed the object with a matching digest and byte size. This is the only state routing
  serves from.
- **Failed**: a transfer attempt failed for a classified reason: the source was unavailable, the transferred bytes
  hashed to a different digest, or the backend refused the write. A digest mismatch can never serve.
- **Revoked**: the placement was withdrawn from serving for reclamation or an administrative decision.

Verifying a digest that does not match the key records a digest-mismatch failure rather than a verified placement. A
failure on one placement never erases a verified placement in another location, because each
`(digest, backend, datacenter, location)` is its own row.

## Fencing stale writers

Every placement carries a fencing epoch. A transfer worker applies transitions under the epoch it holds; a write from a
lower epoch is a stale worker that lost ownership, and it is rejected without changing the record. A higher epoch takes
over. This keeps a preempted worker from overwriting the decision of the worker that replaced it. Each accepted
transition also advances a generation counter, so a reader can detect a concurrent change.

## Routing categories

A routing query for one digest, given the querying node's own datacenter, splits the digest's placements into the
choices a router picks between:

- **local**: verified placements in this datacenter, served without a network hop;
- **verified remote**: verified placements a peer datacenter can stream;
- **pending**: transfers still in flight;
- **failed**: candidates that cannot serve until retried;
- **revoked**: placements withdrawn from serving.

A digest is serveable when it has any verified placement, local or remote. A digest with only pending or failed
placements is present in the ledger but not yet serveable, which a router distinguishes from a digest with no placement
at all.

Placement tables are optional persistence domains. Opening a metadata store does not create them. The first placement
write creates the required table; reads before that write return no placements.
