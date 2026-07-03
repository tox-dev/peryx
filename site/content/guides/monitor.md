+++
title = "Monitor usage and cache health"
description = "Read the usage counters, drill down to files, watch for upstream changes, and scrape Prometheus."
weight = 8
+++

velodex counts everything it serves, off the request path. Three surfaces show the same numbers: the dashboard, the
`/+stats` JSON endpoint, and Prometheus `/metrics`.

## Drill down from the dashboard

Each index card on the dashboard shows its pages, downloads, and bytes served, with a `usage` link. The link opens the
drill-down: totals for the index and a table of its projects, each project linking to its per-file counters. The same
pages live at `/stats`, `/stats?index={route}`, and `/stats?index={route}&project={name}`.

{{ screen(name="stats-index", alt="The usage drill-down for one index: totals up top, busiest projects first") }}

{{ screen(name="stats-project", alt="One project drilled to file level: downloads, bytes, and metadata hits per artifact") }}

## Query the counters

```shell
curl -s http://127.0.0.1:4433/+stats | jq
curl -s 'http://127.0.0.1:4433/+stats?index=root/pypi' | jq '.projects | to_entries | sort_by(-.value.downloads)[:5]'
curl -s 'http://127.0.0.1:4433/+stats?index=root/pypi&project=numpy' | jq .files
```

Counters live in memory and reset on restart; for durable time series, scrape `/metrics`, which exposes the same set per
index (`velodex_index_downloads_total{index="root/pypi"}` and friends) alongside the global request counters.

## What the cache-health counters mean

| Counter           | Signal                                                                      |
| ----------------- | --------------------------------------------------------------------------- |
| `refreshes`       | Revalidations against upstream, on demand or from the background sweep       |
| `changed`         | Revalidations that found new upstream content; also logged at `info`         |
| `stale_served`    | Pages served from cache because upstream was down; rising means an outage    |
| `upstream_errors` | Upstream failures with nothing cached to fall back to; clients saw an error  |
| `rejected`        | Downloads whose bytes did not match the advertised digest; never cached      |

A steady `refreshes` with zero `changed` is the normal idle state. `rejected` above zero deserves attention: either the
upstream served corrupt bytes or something rewrote them in transit.


## Related

- Where the counters come from: [architecture](@/explanation/architecture.md)
- The endpoint and counter reference: [HTTP endpoints](@/reference/endpoints.md)
- The counters in a browser: [the web UI](@/guides/web-ui.md)
