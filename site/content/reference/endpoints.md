+++
title = "HTTP endpoints"
description = "The routes every index serves, plus health and metrics."
weight = 2
+++

Every configured index route serves the same surface; `{route}` below is the index's `route`, for example `root/pypi`.
velodex resolves a request to the index with the longest matching route prefix. The [API explorer](@/reference/api.md)
breaks each endpoint down with copyable example requests and responses.

- `GET /{route}/simple/`: project list, JSON or HTML by `Accept`.
- `GET /{route}/simple/{project}/`: project detail, merged across virtual-index layers.
- `GET /{route}/{project}/json`: legacy PyPI project JSON: `info`, `releases`, and latest-release `urls`.
- `GET /{route}/{project}/{version}/json`: legacy PyPI release JSON for one version.
- `GET /{route}/files/{sha256}/{filename}`: artifact download, cached content-addressed.
- `GET /{route}/files/{sha256}/{filename}.metadata`: [PEP 658](https://peps.python.org/pep-0658/) core-metadata sibling.
- `POST /{route}/`: upload ([legacy API](https://docs.pypi.org/api/upload/), used by twine and `uv publish`).
- `GET /{route}/+api`: index discovery, absolute URLs, capabilities, and redacted client config.
- `GET /{route}/inspect/{sha256}/{filename}`: archive member listing as JSON.
- `GET /{route}/inspect/{sha256}/{filename}/{member}`: one archive member's content.
- `PUT /{route}/{project}/[{version}/]yank`: yank files ([PEP 592](https://peps.python.org/pep-0592/)); cached files get
  an override.
- `DELETE /{route}/{project}/[{version}/]yank`: un-yank.
- `DELETE /{route}/{project}/[{version}/]`: delete uploads (volatile only); hide cached files.
- `PUT /{route}/{project}/[{version}/]restore`: restore hidden cached files.
- `PUT /{route}/{project}/{version}/promote?from=...`: promote uploaded records from another route's hosted layer.
- `GET /+api`: server discovery, global URLs plus every configured index.
- `GET /+status`: JSON health, version, counters, index descriptions.
- `GET /+stats`: usage counters, drillable to project and file level.
- `GET /metrics`: [Prometheus](https://prometheus.io/docs/instrumenting/exposition_formats/) text exposition.

The web UI lives outside the index namespace: `GET /` (dashboard), `GET /admin/status` (read-only operational status),
`GET /browse` (package browser), `GET /stats` (usage drill-down), and `GET /pkg/*` (the wasm bundle that hydrates the
pages).

## Content negotiation

Simple-API responses honor the `Accept` header: `application/vnd.pypi.simple.v1+json`
([PEP 691](https://peps.python.org/pep-0691/)) when the client asks for JSON, `text/html`
([PEP 503](https://peps.python.org/pep-0503/)) otherwise. Responses carry `Vary: Accept` and advertise
`meta.api-version` 1.4. velodex preserves upstream Simple API fields it understands, including `versions`, `size`,
`upload-time`, `project-status`, `provenance`, `gpg-sig`, and both `core-metadata` and `dist-info-metadata`.

Legacy PyPI JSON API responses use `application/json`. Velodex builds `/pypi/<project>/json`-style responses from the
resolved Simple detail page for the requested index route, so `releases`, `urls`, hashes, yanked markers, upload time,
size, and `requires_python` match the Simple API. Simple pages do not carry PyPI's upload-form metadata, vulnerability
database, ownership data, download counts, last serial values, or MD5/BLAKE2 hashes when the upstream did not advertise
them; those fields are null, empty, `0`, or `-1`.

## Index policy

Policy rules configured under `[index.policy]` run before Simple API bytes leave the server. Project-list responses omit
blocked projects. Project-detail responses omit blocked files and remove their versions from PEP 691 `versions`; when a
project-level rule blocks the whole page, the response is `403` with a JSON policy denial. Search results use the same
effective policy before packages enter the derived search index.

Upload and direct file-download denials use the same JSON shape:

```json
{
  "action": "upload",
  "project": "flask",
  "filename": "flask-1.0-py3-none-any.whl",
  "version": "1.0",
  "rule": "max-file-size",
  "field": "size",
  "reason": "file size 2048 exceeds limit 1024"
}
```

`action` is one of `upload`, `mirror`, or `serve`. `rule` names the policy key that denied the artifact or project, and
`field` names the matched value.

## Discovery

`GET /+api` returns a compact JSON document for the server and every configured index. `GET /{route}/+api` returns the
same shape for one index. Velodex builds these documents from request headers and runtime index configuration; it does
not scan package pages or storage.

When the request carries an origin (`Host`, or `X-Forwarded-Host` plus `X-Forwarded-Proto` behind a proxy), URL fields
are absolute and `client_configuration` includes copyable `pip.conf`, `uv.toml`, and `.pypirc` text. The `.pypirc`
snippet uses `__token__` as the username and `<upload-token>` as the password, and Velodex never returns the configured
upload token. Read-only indexes omit upload URLs and `.pypirc`.

Capability flags describe the current route only. `uploads`, `yanking`, and `volatile_deletes` follow the configured
hosted upload target; Simple HTML/JSON, PEP 658 metadata siblings, project status, provenance, and legacy JSON are true
for all indexes.

## Authentication

`POST`, `PUT`, and `DELETE` require `Authorization: Basic` where the password is the `upload_token` for the target
hosted index; the username is ignored. Promotion authenticates against the target route. Responses:

- `200`: accepted; removal responses state how many files changed.
- `400`: malformed upload, bad promotion query, or unsafe path segment.
- `401`: missing or wrong token.
- `403`: uploads disabled, target project status rejects writes, index policy denies the request, or the index is not
  volatile.
- `404`: unknown route, project, or nothing matched.
- `405`: the route's index does not accept writes.
- `409`: promotion target already has the filename with different bytes.
- `429`: a route-class limit rejected the request, or a configured upstream concurrency cap could not free a slot
  within the wait window; retry after the `Retry-After` seconds.

## Webhooks

Configured webhooks run after a write commits. Velodex enqueues one delivery per matching `[[index.webhook]]` target,
then sends the JSON payload from a background task. Duplicate uploads with the same bytes and mutations that affect zero
files do not enqueue webhook deliveries.

Events emitted by the write endpoints are `upload`, `yank`, `unyank`, `delete`, and `restore`. Payloads contain `event`,
`created_at`, `index`, `route`, `local_index`, `project`, `count`, and, when present, `version`, `file`, `actor`, and
`request_id`. Upload payloads include `file.filename` and `file.sha256`. Payloads and delivery errors exclude
`Authorization`, upload tokens, upstream credentials, webhook secrets, URL query strings, and response bodies.

Each request carries these headers:

| Header                | Meaning                               |
| --------------------- | ------------------------------------- |
| `X-Velodex-Event`     | Event name, such as `upload`          |
| `X-Velodex-Delivery`  | Delivery ID, stable across retries    |
| `X-Velodex-Timestamp` | Unix timestamp used for the signature |
| `X-Velodex-Signature` | `sha256=<hex>` HMAC-SHA256 signature  |
| `Content-Type`        | `application/json`                    |

The signature input is `{timestamp}.{delivery}.{body}`, where `body` is the exact request body bytes. Consumers should
compare the HMAC with the configured target secret and reject timestamps outside their replay window.

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
`Retry-After` in seconds. A cached index leaves upstream fetches uncapped by default; when you set
`upstream_concurrency` and the cap is saturated, requests wait for a free slot instead of failing, and only a wait
longer than 30 seconds returns the same `429` with `Retry-After`.

Velodex writes a security log for each denial with `event = "rate_limit"`, the denied class or index, and the retry
delay. It never logs credentials. Prometheus includes allowed and denied HTTP request counters by class, plus upstream
concurrency denials by cached index. HTTP request counters stay at zero while the request limiter is disabled.

## Status and usage

`GET /+status` returns version, serial, request counters, configured index descriptions, cached index status, and
redacted token metadata. It includes sanitized upstream URLs with user info, query strings, and fragments removed. It
does not include upload-token values, upstream usernames, passwords, bearer tokens, URL query secrets, or URL fragments.
Cached index entries also include `upstream.offline`, which is `true` when that cached index is serving only cached
data.

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

- `velodex_requests_total`: HTTP requests served. This is the one global counter; everything else below is per index.
- `velodex_rate_limit_allowed_total{class="<class>"}`: HTTP requests the local rate limiter allowed.
- `velodex_rate_limit_denied_total{class="<class>"}`: HTTP requests the local rate limiter denied.
- `velodex_upstream_rate_limit_denied_total{index="<name>"}`: cached index concurrency cap denials.
- `velodex_upstream_inflight_fetches{index="<name>"}`: current upstream fetches holding a concurrency slot.

Every per-index counter carries `{index="<route>",ecosystem="<ecosystem>",role="<role>"}` labels, and each family is
scoped to the role that reports it:

- Base (every role): `velodex_index_pages_total`, `velodex_index_downloads_total`, `velodex_index_download_bytes_total`,
  `velodex_index_rejected_total`.
- Caching indexes only: `velodex_index_refreshes_total`, `velodex_index_pages_changed_total`,
  `velodex_index_stale_served_total`, `velodex_index_upstream_errors_total`.
- Hosted indexes only: `velodex_index_uploads_total`.
- Ecosystem families (declared by the ecosystem driver): `velodex_index_metadata_total` is PyPI's PEP 658/714
  `.metadata` sibling counter; a rising value proves clients resolve via the metadata fast path rather than by
  downloading artifacts. Sum it across indexes for the instance-wide total.
