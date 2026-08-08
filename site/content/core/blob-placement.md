+++
title = "Blob placement"
description = "The evidence-based ledger of where a content-addressed blob physically resides across backends and data centers, and how routing reads it."
weight = 10
+++

A blob placement records that a specific content digest is present on a specific backend, in a specific data center, at
a specific location. It is the durable evidence a router consults to decide whether it can serve an artifact's bytes
locally, stream them from a peer, wait for a transfer, or refuse. This is separate from the neutral per-digest
[artifact source and availability](@/core/artifact-source.md) projection: that page answers "can this single instance
serve the bytes right now"; a blob placement answers "which backends and data centers hold these bytes, and in what
state".

This page describes the placement ledger the transfer and reclamation work builds on. The transfer engine, the peer
streaming endpoint, and the operator UI are separate; here only the state and its routing queries exist.

## The placement key

Each placement is keyed by four parts:

- the content digest, canonical `sha256:<64 lowercase hex>`;
- the backend it lives on, such as `filesystem` or `s3`;
- the data center that holds it, an opaque operator-assigned label;
- the location inside that backend: an object key or a store-relative path.

The same digest can hold up to 64 placements, so one artifact can be local on one backend, verified on a peer, and
mid-transfer on a third without the rows colliding. A stage that would exceed the bound is refused rather than growing
an unbounded history.

## States and evidence

A placement moves through an evidence-based lifecycle. A staged temporary file is never treated as serveable; only a
backend-confirmed digest is.

- **Pending** : a transfer is in flight or a file is staged. No serving evidence exists.
- **Verified** : the backend confirmed the object with a matching digest and byte size. This is the only state routing
  serves from.
- **Failed** : a transfer attempt failed for a classified reason: the source was unavailable, the transferred bytes
  hashed to a different digest, or the backend refused the write. A digest mismatch can never serve.
- **Revoked** : the placement was withdrawn from serving, for reclamation or an administrative decision.

Verifying a digest that does not match the key records a digest-mismatch failure rather than a verified placement. A
failure on one placement never erases a verified placement in another location, because each
`(digest, backend, data center, location)` is its own row.

## Fencing stale writers

Every placement carries a fencing epoch. A transfer worker applies transitions under the epoch it holds; a write from a
lower epoch is a stale worker that lost ownership, and it is rejected without changing the record. A higher epoch takes
over. This keeps a preempted worker from overwriting the decision of the worker that replaced it. Each accepted
transition also advances a generation counter, so a reader can detect a concurrent change.

## Routing categories

A routing query for one digest, given the querying node's own data center, splits the digest's placements into the
choices a router picks between:

- **local** : verified placements in this data center, served without a network hop;
- **verified remote** : verified placements a peer data center can stream;
- **pending** : transfers still in flight;
- **failed** : candidates that cannot serve until retried;
- **revoked** : placements withdrawn from serving.

A digest is serveable when it has any verified placement, local or remote. A digest with only pending or failed
placements is present in the ledger but not yet serveable, which a router distinguishes from a digest with no placement
at all.
