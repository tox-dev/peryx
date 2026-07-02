+++
title = "Monitor usage and cache health"
description = "Read the usage counters, drill down to files, watch for upstream changes, and scrape Prometheus."
weight = 6
+++

velodex counts everything it serves, off the request path. Three surfaces show the same numbers: the dashboard, the
`/+stats` JSON endpoint, and Prometheus `/metrics`.

## Drill down from the dashboard

Each index card on the dashboard shows its pages, downloads, and bytes served, with a `usage` link. The link opens the
drill-down: totals for the index and a table of its projects, each project linking to its per-file counters. The same
pages live at `/stats`, `/stats?index={route}`, and `/stats?index={route}&project={name}`.

## Query the counters

```shell
curl -s http://127.0.0.1:4433/+stats | jq
curl -s 'http://127.0.0.1:4433/+stats?index=root/pypi' | jq '.projects | to_entries | sort_by(-.value.downloads)[:5]'
curl -s 'http://127.0.0.1:4433/+stats?index=root/pypi&project=numpy' | jq .files
```

Counters live in memory and reset on restart; for durable time series, scrape `/metrics`, which exposes the same set per
index (`velodex_index_downloads_total{index="root/pypi"}` and friends) alongside the global request counters.

## What the cache-health counters mean

| Counter | Signal | | ----------------- |
------------------------------------------------------------------------------------------ | | `refreshes` |
Revalidations against upstream, on demand or from the minute-by-minute background sweep | | `changed` | Revalidations
that found new upstream content; the change is also logged at `info` | | `stale_served` | Pages served from cache
because upstream was down; rising means an upstream outage | | `upstream_errors` | Upstream failures with nothing cached
to fall back to; these surfaced to clients as errors | | `rejected` | Downloads whose bytes did not match the advertised
digest; the blob was not cached |

A steady `refreshes` with zero `changed` is the normal idle state. `rejected` above zero deserves attention: either the
upstream served corrupt bytes or something rewrote them in transit.
