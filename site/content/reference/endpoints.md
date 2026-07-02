+++
title = "HTTP endpoints"
description = "The routes every index serves, plus health and metrics."
weight = 2
+++

Every configured index route serves the same surface; `{route}` below is the index's `route`, for example
`root/pypi`. velodex resolves a request to the index with the longest matching route prefix. The
[API explorer](@/reference/api.md) breaks each endpoint down with copyable example requests and responses.

| Method and path                                    | Purpose                                              |
| -------------------------------------------------- | ---------------------------------------------------- |
| `GET /{route}/simple/`                              | Project list, JSON or HTML by `Accept`               |
| `GET /{route}/simple/{project}/`                    | Project detail, merged across overlay layers         |
| `GET /{route}/files/{sha256}/{filename}`            | Artifact download, cached content-addressed          |
| `GET /{route}/files/{sha256}/{filename}.metadata`   | [PEP 658](https://peps.python.org/pep-0658/) core-metadata sibling                        |
| `POST /{route}/`                                    | Upload ([legacy API](https://docs.pypi.org/api/upload/), used by twine and `uv publish`)  |
| `GET /{route}/inspect/{sha256}/{filename}`          | Archive member listing (JSON)                        |
| `GET /{route}/inspect/{sha256}/{filename}/{member}` | One archive member's content                         |
| `PUT /{route}/{project}/[{version}/]yank`           | Yank files ([PEP 592](https://peps.python.org/pep-0592/)); mirror files get an override |
| `DELETE /{route}/{project}/[{version}/]yank`        | Un-yank                                              |
| `DELETE /{route}/{project}/[{version}/]`            | Delete uploads (volatile only); hide mirror files    |
| `PUT /{route}/{project}/[{version}/]restore`        | Restore hidden mirror files                          |
| `GET /+status`                                      | JSON health: version, counters, index descriptions   |
| `GET /+stats`                                       | Usage counters, drillable to project and file level   |
| `GET /metrics`                                      | [Prometheus](https://prometheus.io/docs/instrumenting/exposition_formats/) text exposition                           |

The web UI lives outside the index namespace: `GET /` (dashboard), `GET /browse` (package browser), `GET /stats`
(usage drill-down), and `GET /pkg/*` (the wasm bundle that hydrates the pages).

## Content negotiation

Simple-API responses honor the `Accept` header: `application/vnd.pypi.simple.v1+json` ([PEP 691](https://peps.python.org/pep-0691/)) when the client asks
for JSON, `text/html` ([PEP 503](https://peps.python.org/pep-0503/)) otherwise. Responses carry `Vary: Accept` and advertise `meta.api-version` 1.1,
which includes the [PEP 700](https://peps.python.org/pep-0700/) `versions`, `size`, and `upload-time` fields.

## Authentication

`POST`, `PUT`, and `DELETE` require `Authorization: Basic` where the password is the target local index's
`upload_token`; the username is ignored. Responses:

| Status | Meaning                                                       |
| ------ | ------------------------------------------------------------- |
| `200`  | Accepted; removal responses state how many files were affected |
| `400`  | Malformed upload: wrong `:action`, missing field, digest mismatch |
| `401`  | Missing or wrong token                                         |
| `403`  | Uploads disabled (no token configured) or index not volatile   |
| `404`  | Unknown route, project, or nothing matched                     |
| `405`  | The route's index does not accept writes                       |

## Usage statistics

`GET /+stats` returns JSON counters aggregated off the request path, at three depths:

- No parameters: totals per index route.
- `?index={route}`: one index's totals plus a counter set per project.
- `?index={route}&project={name}`: one project's totals plus downloads, metadata hits, and bytes per file.

The counters are `pages`, `downloads`, `metadata`, `uploads`, `bytes`, `refreshes` (upstream revalidations),
`changed` (revalidations that found new upstream content), `stale_served` (pages served from cache with upstream
down), `upstream_errors` (failures with nothing cached), and `rejected` (downloads whose bytes failed digest
verification and were not cached). Counters reset on restart; scrape `/metrics` for durable time series.

## Metrics

`GET /metrics` exposes Prometheus counters:

- `velodex_requests_total`: HTTP requests served.
- `velodex_metadata_requests_total`: PEP 658 `.metadata` siblings served; a rising value proves clients resolve via
  the metadata fast path rather than by downloading wheels.
- `velodex_index_*_total{index="<route>"}`: the `/+stats` counter set per index route (`pages`, `downloads`,
  `download_bytes`, `metadata`, `uploads`, `refreshes`, `pages_changed`, `stale_served`, `upstream_errors`,
  `rejected`).
