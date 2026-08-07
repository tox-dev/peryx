+++
title = "Client behavior across availability modes"
description = "The protocol-neutral retry, fencing, and read-consistency rules clients observe in none, dc, and ha modes."
weight = 11
+++

Availability mode changes when a mutation is acknowledged, not which ecosystem protocol a client speaks. Protocol status
codes and bodies belong to each ecosystem reference; this page defines the shared behavior they map onto.

## Writes

Send mutations to a writer. A replica refuses them before protocol handling, so it cannot create local authoritative
state. A writer also refuses a mutation when it cannot satisfy the configured durability contract.

Every admitted mutation receives an idempotency identity. If a response is lost, retrying the same mutation resolves to
the committed result rather than applying a second change. A request under an authority epoch that has been superseded
is fenced and returns a retryable protocol error.

Ingress is bounded. When retained, unfinalized work reaches its record or byte limit, the node refuses more work with a
retryable response instead of growing memory or disk use without limit.

## Reads

Mutable reads are bounded by the readable frontier. A replica does not expose metadata until every required derived view
has applied through that serial. Digest-addressed reads remain safe because returned bytes must verify against the
requested digest.

Missing local bytes may be read through from an eligible peer when distributed read-through is configured. A failed peer
never causes unverified or partial bytes to be committed locally.

## Failover

After authority moves, the former owner is fenced. A retry against the new owner converges on the committed result.
Clients should treat the ecosystem's authority-moved and durability-unavailable responses as retryable and should not
rewrite the mutation between attempts.

Protocol mappings:

- [Python package availability behavior](@/ecosystems/pypi/reference/availability.md)
- [OCI availability behavior](@/ecosystems/oci/reference/availability.md)

## Local mode

Under `none`, writes commit locally and reads use local state. No distributed route, client, task, timer, queue, or
metric exists, so the shared request path pays no availability-mode branch per request.
