+++
title = "HTTP endpoints"
description = "The routes every index serves, plus health and metrics."
weight = 2
+++

Every configured index route serves the same surface; `{route}` below is the index's `route`, for example `root/pypi`.
velodex resolves a request to the index with the longest matching route prefix. The [API explorer](@/reference/api.md)
breaks each endpoint down with copyable example requests and responses.

| Method and path                                     | Purpose                                                                                    |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `GET /{route}/simple/`                              | Project list, JSON or HTML by `Accept`                                                     |
| `GET /{route}/simple/{project}/`                    | Project detail, merged across overlay layers                                               |
| `GET /{route}/files/{sha256}/{filename}`            | Artifact download, cached content-addressed                                                |
| `GET /{route}/files/{sha256}/{filename}.metadata`   | [PEP 658](https://peps.python.org/pep-0658/) core-metadata sibling                         |
| `POST /{route}/`                                    | Upload ([legacy API](https://docs.pypi.org/api/upload/), used by twine and `uv publish`)   |
| `GET /{route}/+api`                                 | Index discovery: absolute URLs, capabilities, and redacted client config                   |
| `GET /{route}/inspect/{sha256}/{filename}`          | Archive member listing (JSON)                                                              |
| `GET /{route}/inspect/{sha256}/{filename}/{member}` | One archive member's content                                                               |
| `PUT /{route}/{project}/[{version}/]yank`           | Yank files ([PEP 592](https://peps.python.org/pep-0592/)); mirror files get an override    |
| `DELETE /{route}/{project}/[{version}/]yank`        | Un-yank                                                                                    |
| `DELETE /{route}/{project}/[{version}/]`            | Delete uploads (volatile only); hide mirror files                                          |
| `PUT /{route}/{project}/[{version}/]restore`        | Restore hidden mirror files                                                                |
| `GET /+api`                                         | Server discovery: global URLs plus every configured index                                  |
| `GET /+status`                                      | JSON health: version, counters, index descriptions                                         |
| `GET /+stats`                                       | Usage counters, drillable to project and file level                                        |
| `GET /metrics`                                      | [Prometheus](https://prometheus.io/docs/instrumenting/exposition_formats/) text exposition |

The web UI lives outside the index namespace: `GET /` (dashboard), `GET /admin/status` (read-only operational status),
`GET /browse` (package browser), `GET /stats` (usage drill-down), and `GET /pkg/*` (the wasm bundle that hydrates the
pages).

## Content negotiation

Simple-API responses honor the `Accept` header: `application/vnd.pypi.simple.v1+json`
([PEP 691](https://peps.python.org/pep-0691/)) when the client asks for JSON, `text/html`
([PEP 503](https://peps.python.org/pep-0503/)) otherwise. Responses carry `Vary: Accept` and advertise
`meta.api-version` 1.4. velodex preserves upstream Simple API fields it understands, including `versions`, `size`,
`upload-time`, `project-status`, `provenance`, `gpg-sig`, and both `core-metadata` and `dist-info-metadata`.

## Discovery

`GET /+api` returns a compact JSON document for the server and every configured index. `GET /{route}/+api` returns the
same shape for one index. Velodex builds these documents from request headers and runtime index configuration; it does
not scan package pages or storage.

When the request carries an origin (`Host`, or `X-Forwarded-Host` plus `X-Forwarded-Proto` behind a proxy), URL fields
are absolute and `client_configuration` includes copyable `pip.conf`, `uv.toml`, and `.pypirc` text. The `.pypirc`
snippet uses `__token__` as the username and `<upload-token>` as the password, and Velodex never returns the configured
upload token. Read-only indexes omit upload URLs and `.pypirc`.

Capability flags describe the current route only. `uploads`, `yanking`, and `volatile_deletes` follow the configured
local upload target; Simple HTML/JSON and PEP 658 metadata siblings are true for all indexes. `project_status`,
`provenance`, and `legacy_json` stay false until those protocol surfaces exist.

## Authentication

`POST`, `PUT`, and `DELETE` require `Authorization: Basic` where the password is the target local index's
`upload_token`; the username is ignored. Responses:

| Status | Meaning                                                                                                                                              |
| ------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `200`  | Accepted; removal responses state how many files were affected                                                                                       |
| `400`  | Malformed upload: wrong `:action`, missing field, bad distribution file, digest mismatch, metadata mismatch, duplicate filename with different bytes |
| `401`  | Missing or wrong token                                                                                                                               |
| `403`  | Uploads disabled (no token configured) or index not volatile                                                                                         |
| `404`  | Unknown route, project, or nothing matched                                                                                                           |
| `405`  | The route's index does not accept writes                                                                                                             |
| `429`  | A route-class limit or mirror upstream concurrency cap rejected the request; retry after the `Retry-After` seconds                                   |

Uploads accept wheels and `.tar.gz` sdists. The server validates the filename, form `name` and `version`, `filetype`,
archive contents, and core metadata before the artifact becomes visible. Wheel validation requires normalized
`.dist-info` paths, required `METADATA`/`WHEEL`/`RECORD` files, WHEEL tag/build consistency, RECORD hashes, and matching
RECORD sizes when present. Sdist validation requires a PEP 625 `.tar.gz` filename, one safe `{name}-{version}/`
top-level directory, `pyproject.toml`, and `PKG-INFO`; unsafe tar members and Metadata 2.4+ missing `License-File`
entries are rejected. Wheel uploads serve `METADATA` as the PEP 658/714 `.metadata` sibling. Sdist uploads serve the
verified `PKG-INFO` the same way.

Archive inspection is broader than uploads. It can list and preview cached wheels, zips, zipped eggs, `.tar`, `.tar.gz`,
and `.tgz` archives, including supported archives nested inside them. Other legacy compressed tar formats stay
download-only until velodex adds decoders for them. Mirrored eggs remain downloadable when upstream lists them with a
sha256 hash, but they do not get PEP 658 metadata.

## Rate limits

When `[rate_limit] enabled = true` and a client exceeds a configured route-class window, velodex returns
`429 Too Many Requests` before the handler reads multipart bodies, cache state, or upstreams. The response includes
`Retry-After` in seconds. The same status and header apply when a mirror `upstream_concurrency` cap has no free slot.

Velodex writes a security log for each denial with `event = "rate_limit"`, the denied class or repository, and the retry
delay. It never logs credentials. Prometheus includes allowed and denied HTTP request counters by class, plus upstream
concurrency denials by mirror index. HTTP request counters stay at zero while the request limiter is disabled.

## Status and usage

`GET /+status` returns version, serial, request counters, configured index descriptions, mirror status, and redacted
token metadata. It includes sanitized upstream URLs with user info, query strings, and fragments removed. It does not
include upload-token values, upstream usernames, passwords, bearer tokens, URL query secrets, or URL fragments.

Add `?details=admin` for the read-only admin status page. That shape also includes observed project counts, uploaded
file counts, and capped recent uploads. The summary scans metadata keys once and does not fetch upstreams or read cached
artifact bytes.

`GET /+stats` returns JSON counters aggregated off the request path, at three depths:

- No parameters: totals per index route.
- `?index={route}`: one index's totals plus a counter set per project.
- `?index={route}&project={name}`: one project's totals plus downloads, metadata hits, and bytes per file.

The counters are `pages`, `downloads`, `metadata`, `uploads`, `bytes`, `refreshes` (upstream revalidations), `changed`
(revalidations that found new upstream content), `stale_served` (pages served from cache with upstream down),
`upstream_errors` (failures with nothing cached), and `rejected` (downloads whose bytes failed digest verification and
were not cached). Counters reset on restart; scrape `/metrics` for durable time series.

## Metrics

`GET /metrics` exposes Prometheus counters:

- `velodex_requests_total`: HTTP requests served.
- `velodex_metadata_requests_total`: PEP 658/714 `.metadata` siblings served; a rising value proves clients resolve via
  the metadata fast path rather than by downloading artifacts.
- `velodex_rate_limit_allowed_total{class="<class>"}`: HTTP requests the local rate limiter allowed.
- `velodex_rate_limit_denied_total{class="<class>"}`: HTTP requests the local rate limiter denied.
- `velodex_upstream_rate_limit_denied_total{index="<name>"}`: mirror concurrency cap denials.
- `velodex_upstream_inflight_fetches{index="<name>"}`: current upstream fetches holding a concurrency slot.
- `velodex_index_*_total{index="<route>"}`: the `/+stats` counter set per index route (`pages`, `downloads`,
  `download_bytes`, `metadata`, `uploads`, `refreshes`, `pages_changed`, `stale_served`, `upstream_errors`, `rejected`).
