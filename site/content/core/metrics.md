+++
title = "Prometheus metric contract"
description = "Scrape behavior, label limits, counting rules, and ecosystem metric registration."
weight = 8
+++

`GET /metrics` returns Prometheus text exposition with content type `text/plain; version=0.0.4`. Counters and gauges
live in memory and reset on restart. A scraper turns them into durable time series.

## Access

The endpoint is unauthenticated and independent of repository access policy. Restrict it at the network or reverse
proxy. A scrape reveals volumes, error rates, and runtime health even though labels exclude tenant identities.

## Label policy

Metric labels use closed or configuration-bounded sets:

| Label       | Source                                   |
| ----------- | ---------------------------------------- |
| `ecosystem` | Registered implementation identifier     |
| `role`      | `cached`, `hosted`, or `virtual`         |
| `class`     | Fixed request, limiter, or failure class |
| `kind`      | Registered background job kind           |
| `outcome`   | Fixed completion outcome                 |
| `reason`    | Fixed rejection or readiness reason      |
| `le`        | Fixed histogram bucket boundary          |

Repository, entity, artifact, file, user, path, error text, credential, token, digest, node, and URL values cannot
become metric names or labels. Use stats and analytics APIs for repository drill-down.

An implementation registers a metric family with its supported roles and label set. Registration rejects undeclared
labels and duplicate family names. A configured repository contributes to the series for its implementation and role; it
does not create a repository label.

Neutral examples should use `<registered-ecosystem-id>` in label selectors. Concrete selectors belong to the selected
implementation's metric reference.

## Counting rules

Serving counters follow the monitoring API's completion rule. A full response increments artifact count and bytes after
the expected body leaves the server. A completed range increments one artifact count and its transmitted bytes. Failed,
cancelled, unauthorized, policy-denied, and digest-rejected transfers do not count as downloads.

Cache counters separate revalidation, changed upstream metadata, stale serving, hard upstream errors, and rejected
content. Hosted counters separate accepted writes from policy or quota rejection. Runtime counters cover request limits,
background jobs, replication, availability work, and durability decisions.

## Family ownership

Core owns the registration rules and shared runtime collectors. Ecosystem implementations own serving, metadata,
publication, quota, catalog, and protocol-specific families. Their references list exact series names, roles, and alert
queries:

- [PyPI metric families](@/ecosystems/pypi/reference/endpoints.md#prometheus-metrics)
- [OCI metric families](@/ecosystems/oci/reference/endpoints.md#prometheus-metrics)

## Alert design

Build rates from counters and use sustained windows for upstream, scheduler, and replication failures. Alert on gauge
state for readiness, replica lag, pending work, and worker saturation. Set thresholds from the deployment's request and
write rates. Repository-level alerts require stats or analytics queries because Prometheus labels omit repository names.

## Related

- [Monitoring APIs](@/core/monitor.md)
- [Availability deployment and sizing](@/core/availability-deployment.md)
- [High availability](@/core/high-availability.md)
