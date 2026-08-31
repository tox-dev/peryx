+++
title = "HTTP endpoints"
description = "The routes every index serves, plus health and metrics."
weight = 2
+++

Every configured index route serves the same surface; `{route}` below is the index's `route`, for example `root/pypi`.
peryx resolves a request to the index with the longest matching route prefix. The
[API explorer](@/core/reference/api.md) breaks each endpoint down with copyable example requests and responses.

- `GET /{route}/simple/`: project list, JSON or HTML by `Accept`.
- `GET /{route}/simple/{project}/`: project detail, merged across virtual-index layers.
- `GET /{route}/simple` and `GET /{route}/simple/{project}`: `301` to the slash-terminated, name-normalized canonical
  URL ([trailing-slash redirects](@/ecosystems/pypi/reference/simple-api.md#trailing-slash-redirects)).
- `GET /{route}/{project}/json`: legacy PyPI project JSON: `info`, `releases`, and latest-release `urls`.
- `GET /{route}/{project}/{version}/json`: legacy PyPI release JSON for one version.
- `GET /{route}/files/{sha256}/{filename}`: artifact download, cached content-addressed.
- `GET /{route}/files/{sha256}/{filename}.metadata`: [PEP 658](https://peps.python.org/pep-0658/) core-metadata sibling.
- `GET /{route}/files/{sha256}/{filename}.provenance`: [PEP 740](https://peps.python.org/pep-0740/) provenance object
  (`application/vnd.pypi.integrity.v1+json` or the accepted upstream `application/json`) for a hosted file with
  attestations or an upstream file whose policy proxies or retains its provenance.
- `POST /{route}/`: upload ([legacy API](https://docs.pypi.org/api/upload/), used by
  [twine](https://twine.readthedocs.io/) and `uv publish`).
- `GET /{route}/+api`: index discovery, absolute URLs, capabilities, and redacted client config.
- `GET /{route}/+search`: search this index's cached packages.
- `GET /{route}/inspect/{sha256}/{filename}`: archive member listing as JSON.
- `GET /{route}/inspect/{sha256}/{filename}/{member}`: one archive member's content.
- `PUT /{route}/{project}/[{version}/]yank`: yank files ([PEP 592](https://peps.python.org/pep-0592/)); cached files get
  an override.
- `DELETE /{route}/{project}/[{version}/]yank`: un-yank.
- `DELETE /{route}/{project}/[{version}/]`: delete uploads (volatile only); hide cached files.
- `PUT /{route}/{project}/[{version}/]restore`: restore hidden cached files.
- `PUT /{route}/{project}/{version}/promote?from=...`: promote uploaded records from another route's hosted layer; needs
  `write` on the target and `read` on the named source route.
- `GET /+api`: server discovery, global URLs plus every configured index.
- `GET /+search`: search cached packages across every configured index.
- `GET /api-docs/openapi.json`: the public server's OpenAPI description. It excludes private availability-control and
  peer-replication routes.
- `GET /+health`: fixed, redacted process liveness for restart probes.
- `GET /+ready`: redacted local-store readiness for load balancers; add `?writes=true` to require a writer. See
  [load-balancer probes](@/core/availability/high-availability.md#load-balancer-probes) for response and deployment
  examples.
- `GET /+status`: JSON health, version, counters, index descriptions, filtered to the caller's class (public health and
  basic index list, operator counters, administrator upstream and upload state).
- `GET /+stats`: usage counters, drillable to project and file level; needs `operator:read`.
- `GET /metrics`: [Prometheus](https://prometheus.io/docs/instrumenting/exposition_formats/) text exposition; aggregate
  labels only, gate at the reverse proxy.
- `GET /_/oidc/audience`: trusted-publishing audience discovery; `404` without a publisher.
- `POST /_/oidc/mint-token`: exchange a verified CI identity for a short-lived upload token.

The web UI lives outside the index namespace: `GET /` (dashboard), `GET /admin/status` (read-only operational status),
`GET /browse` (package browser), `GET /search` (package search), `GET /stats` (usage drill-down), and `GET /pkg/*` (the
wasm bundle that hydrates the pages).

## Content negotiation

Peryx selects the representation with the highest quality that `Accept` permits. It uses specificity for equal-quality
matches and `application/vnd.pypi.simple.v1+json` ([PEP 691](https://peps.python.org/pep-0691/)) for the final tie.
Peryx treats a missing `Accept` field as `*/*`, which selects JSON. It returns `406 Not Acceptable` if neither JSON nor
`text/html` ([PEP 503](https://peps.python.org/pep-0503/)) qualifies. Responses carry `Vary: Accept` and advertise
`meta.api-version` 1.4 for hosted content and upstream pages that declare version 1.1 or newer. An upstream page that
declares 1.0 or no version gets 1.0; see the [version rule](@/ecosystems/pypi/reference/simple-api.md#version-rule).
peryx preserves upstream Simple API fields it understands, including `versions`, `size`, `upload-time`,
`project-status`, `provenance`, `gpg-sig`, and both `core-metadata` and `dist-info-metadata`. peryx drops the `gpg-sig`
marker for a file it content-addresses onto its own route because that route serves no `.asc`; see
[the gpg-sig marker](@/ecosystems/pypi/reference/simple-api.md#gpg-sig-marker).

Legacy PyPI JSON API responses use `application/json`. Peryx builds `/pypi/<project>/json`-style responses from the
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
same shape for one index. Peryx builds these documents from request headers and runtime index configuration; it does not
scan package pages or storage.

When the request carries an origin (`Host`, or `X-Forwarded-Host` plus `X-Forwarded-Proto` from a peer listed in
`[rate_limit].trusted_proxies`), URL fields are absolute and `client_configuration` includes copyable `pip.conf`,
`uv.toml`, and `.pypirc` text. Forwarding headers from any other peer are ignored. The `.pypirc` snippet uses
`__token__` as the username and `<upload-token>` as the password, and Peryx never returns the configured upload token.
Read-only indexes omit upload URLs and `.pypirc`.

Capability flags describe the current route only. `uploads`, `yanking`, and `volatile_deletes` follow the configured
hosted upload target; Simple HTML/JSON, PEP 658 metadata siblings, project status, provenance, and legacy JSON are true
for all indexes.

## Authentication

`POST`, `PUT`, and `DELETE` accept a configured long-lived Basic credential. A token from the OIDC exchange instead uses
the exact username `__token__` with the minted token as its password, or the raw Bearer scheme. Its grant includes the
public repository route and project; a token for a virtual route cannot write through a sibling or its hosted layer's
direct route. Promotion authenticates against the target route. The write proceeds when the grant covers the normalized
project and action.

Simple API, legacy JSON, metadata, artifact, and archive-inspection reads consult the index ACL. An index left
`anonymous_read = true`, the default, serves every caller. An index with `anonymous_read = false` needs a credential
whose `read` grant covers the normalized project; the list and redirect routes name no project and ask only for a read
of something in the index. A refusal answers `401` with `WWW-Authenticate: Basic realm="peryx"` when the request carries
no usable credential and `403` when the one it carries does not reach the resource, and it says the same thing whether
or not the project exists. A virtual route serves what its layers hold, so it is readable only by a credential every
index it composes admits: closing a layer closes the routes that surface it. Server-rendered project pages, the browser,
and search apply the same read ACLs, and additionally accept a signed-in browser session.

Responses:

- `200`: accepted; removal responses state how many files changed.
- `400`: malformed upload, bad promotion query, or unsafe path segment.
- `401`: missing or wrong token, including a read of an index that does not allow anonymous reads.
- `403`: uploads disabled, target project status rejects writes, index policy denies the request, the presented
  credential's grants do not reach the resource, or the index is not volatile.
- `404`: unknown route, project, or nothing matched.
- `405`: the route's index does not accept writes.
- `409`: promotion target already has the filename with different bytes.
- `429`: a route-class limit rejected the request, or a configured upstream concurrency cap could not free a slot within
  the wait window; retry after the `Retry-After` seconds.

## Webhooks

Configured webhooks run after a write commits. Peryx enqueues one delivery per matching `[[index.webhook]]` target, then
sends the JSON payload from a background task. Duplicate uploads with the same bytes and mutations that affect zero
files do not enqueue webhook deliveries.

Events emitted by the write endpoints are `upload`, `yank`, `unyank`, `delete`, and `restore`. The event filter also
accepts the reserved `promote`, `project-status`, and `management` names for management surfaces that use the webhook
runtime. Payloads contain `event`, `created_at`, `index`, `route`, `hosted_index`, `project`, `count`, and, when
present, `version`, `file`, `actor`, and `request_id`. Upload payloads include `file.filename` and `file.sha256`.
Payloads and delivery errors exclude `Authorization`, upload tokens, upstream credentials, webhook secrets, URL query
strings, and response bodies.

An upload through the default virtual route produces this format-specific body:

```json
{
  "event": "upload",
  "created_at": 1750000000,
  "index": "root-pypi",
  "route": "root/pypi",
  "hosted_index": "hosted",
  "project": "example",
  "version": "1.4.0",
  "file": {
    "filename": "example-1.4.0-py3-none-any.whl",
    "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
  },
  "count": 1,
  "actor": "ci-token",
  "request_id": "req-42"
}
```

Each request carries these headers:

| Header              | Meaning                               |
| ------------------- | ------------------------------------- |
| `X-Peryx-Event`     | Event name, such as `upload`          |
| `X-Peryx-Delivery`  | Delivery ID, stable across retries    |
| `X-Peryx-Timestamp` | Unix timestamp used for the signature |
| `X-Peryx-Signature` | `sha256=<hex>` HMAC-SHA256 signature  |
| `Content-Type`      | `application/json`                    |

The signature input is `{timestamp}.{delivery}.{body}`, where `body` is the exact request body bytes. Consumers should
compare the HMAC with the configured target secret and reject timestamps outside their replay window.

```python
import hashlib
import hmac


def verify(secret: str, headers, body: bytes) -> bool:
    timestamp = headers["x-peryx-timestamp"]
    delivery = headers["x-peryx-delivery"]
    message = f"{timestamp}.{delivery}.".encode() + body
    digest = hmac.new(secret.encode(), message, hashlib.sha256).hexdigest()
    return hmac.compare_digest(f"sha256={digest}", headers["x-peryx-signature"])
```

## Search

`GET /+search` searches readable projects across PyPI and other registered implementations. `GET /root/pypi/+search`
restricts the same query to the default PyPI route. PyPI records use `package` as `type_label`, a normalized project
name, and a summary from project metadata when one exists.

```json
{
  "query": "flask",
  "route": "root/pypi",
  "type": "all",
  "availability": "all",
  "page": 1,
  "page_size": 25,
  "total": 1,
  "results": [
    {
      "display_name": "Flask",
      "normalized_name": "flask",
      "route": "root/pypi",
      "index": "pypi",
      "ecosystem": "pypi",
      "type_label": "package",
      "type": "cached",
      "available": true,
      "summary": "A framework for web applications."
    }
  ]
}
```

The `availability=local` filter keeps projects with at least one distribution stored on this instance. Run
`peryx job reindex` after restoring metadata when the derived index needs a full rebuild. Search parameters and access
rules live in the [search API](@/core/repositories/search.md).

Uploads accept wheels, `.tar.gz` sdists, and `.zip` sdists ([PEP 527](https://peps.python.org/pep-0527/)). The server
validates the filename, form `name` and `version`, `filetype`, archive contents, and
[core metadata](https://packaging.python.org/en/latest/specifications/core-metadata/) before the artifact becomes
visible. Wheel validation requires normalized `.dist-info` paths, required `METADATA`/`WHEEL`/`RECORD` files, WHEEL
tag/build consistency, RECORD hashes, and matching RECORD sizes when present. Both sdist forms meet the same
[PEP 625](https://peps.python.org/pep-0625/) strictness: a name-version filename split at the last `-`, one safe
`{name}-{version}/` top-level directory, `pyproject.toml`, and a `PKG-INFO` whose `Metadata-Version` is at least `2.2`;
unsafe archive members and Metadata 2.4+ missing `License-File` entries are rejected. A `.egg` reports a legacy-egg
error and any other extension, such as `.tar.bz2`, is rejected as unsupported. Wheel uploads serve `METADATA` as the PEP
658/714 `.metadata` sibling. Sdist uploads serve the verified `PKG-INFO` the same way.

Archive inspection is broader than uploads. It can list and preview cached wheels, zips, zipped eggs, `.tar`, `.tar.gz`,
and `.tgz` archives, including supported archives nested inside them. Other legacy compressed tar formats stay
download-only until peryx adds decoders for them. Mirrored eggs remain downloadable when upstream lists them with a
sha256 hash, but they do not get PEP 658 metadata. A download-only archive still lands under the right release: peryx
reads a `.tar.bz2`, `.tar.xz`, `.tar.Z`, `.tgz`, or `.egg` version from its filename in the JSON views, rather than
carrying the extension into the version string.

Inspection releases the same bytes a download does, so it runs the file route's gates before it opens an archive: the
index serve policy at the `serve` action, then the project's stored status. A quarantined project or a filename the
policy denies answers `403` through `inspect` and through the archive browser exactly as it does through `files`, and
the refusal lands before peryx fetches the artifact from upstream.

## Rate limits

When `[rate_limit] enabled = true` and a client exceeds a configured route-class window, peryx returns
`429 Too Many Requests` before the handler reads multipart bodies, cache state, or upstreams. The response includes
`Retry-After` in seconds. A cached index leaves upstream fetches uncapped by default; when you set
`upstream_concurrency` and the cap is saturated, requests wait for a free slot instead of failing, and only a wait
longer than 30 seconds returns the same `429` with `Retry-After`.

The queue behind a saturated cap is itself bounded, because a waiter retains its whole request for the full 30-second
horizon. An index admits four times its `upstream_concurrency` as waiters, and the process admits 1024 upstream fetches
that are active or queued across every index and both the artifact and metadata gates. A request over either bound gets
the same `429` immediately rather than joining the queue, so one index's cold burst cannot consume the memory another
index's requests need.

Peryx writes a security log for each denial with `event = "rate_limit"`, the denied class or index, the retry delay, and
a `reason` separating an admission refusal from an expired wait. It never logs credentials. Prometheus includes allowed
and denied HTTP request counters by class plus process-wide upstream concurrency totals. Rate-limiter request counters
stay at zero while the request limiter is disabled.

PyPI maps project listings and detail pages to `listing`, `.metadata` siblings to `metadata`, artifact downloads and
archive inspection to `artifact`, mutations to `upload`, and status or discovery routes to `admin`. A `HEAD` request
uses the class of its resource.

## Status and usage

`GET /+status` filters its fields to the caller's class and answers `private, no-cache`, so a shared cache never keeps a
credentialed document:

- Public (any caller): `version`, `role`, coarse `health`, and the basic index list. Each index includes its `name`,
  `route`, `ecosystem`, `kind`, `endpoint`, `layers`, and upload target so the browser can navigate and pick an upload
  route.
- `operator:read`: `serial`, accepted HTTP `requests`, `blob_storage`, the `by_ecosystem` rollup, and `metric_families`.
- `administration:read`: each index's sanitized `upstream` (host, auth kind, cached status), `hosted` upload-token
  state, observed project counts, uploaded file counts, and capped recent uploads.

An anonymous or repository-only caller therefore sees the configured routes but no upstream host, upload-token state, or
upload metadata. Upstream URLs drop user info, query strings, and fragments; the document never carries upload-token
values, upstream usernames, passwords, bearer tokens, URL query secrets, or URL fragments. The administrator `upstream`
block includes `offline`, `true` when that cached index serves only cached data, and its summary scans metadata keys
once without fetching upstreams or reading cached artifact bytes.

Authenticate with a local user's Basic credential. The admin status page and dashboard render the same access levels, so
an unauthenticated page shows the routes but not the counters or the sensitive per-index fields.

`GET /+stats` needs `operator:read` because its tree names repositories and projects; a repository token reads its own
usage through `/+analytics/*` instead. It answers `no-store`, `401` without an operator credential, and `404` when the
credential holds no operator grant. It returns JSON counters aggregated off the request path, at three depths:

- No parameters: totals per index route.
- `?index={route}`: one index's totals plus a counter set per project.
- `?index={route}&project={name}`: one project's totals plus downloads, metadata hits, and bytes per file.

The counters are `pages`, `downloads`, `metadata`, `uploads`, `bytes`, `refreshes` (upstream revalidations), `changed`
(revalidations that found new upstream content), `stale_served` (pages served from cache with upstream down),
`upstream_errors` (failures with nothing cached), and `rejected` (downloads whose bytes failed digest verification and
were not cached). Counters reset on restart; scrape `/metrics` for durable time series.

## Prometheus metrics

`GET /metrics` exposes Prometheus counters and gauges:

- `peryx_requests_total`: HTTP requests the server accepts, including limiter rejections and unmatched routes.
- `peryx_rate_limit_allowed_total{class="<class>"}`: HTTP requests the local rate limiter allowed.
- `peryx_rate_limit_denied_total{class="<class>"}`: HTTP requests the local rate limiter denied.
- `peryx_upstream_rate_limit_denied_total`: cached-index concurrency waits that expired across the process.
- `peryx_upstream_admission_denied_total`: upstream fetches refused before queueing because an index allowance or the
  process admission count was full.
- `peryx_upstream_inflight_fetches`: current upstream fetches holding a concurrency slot across the process.
- `peryx_upstream_waiting_fetches`: current admitted upstream fetches queued for a concurrency slot across the process.

Serving counters carry only `{ecosystem="<ecosystem>",role="<role>"}`. Values from repositories with the same ecosystem
and role are summed before rendering. Each family is scoped to the role that reports it:

- Base (every role): `peryx_pages_served_total`, `peryx_artifacts_served_total`, `peryx_artifacts_served_bytes_total`,
  `peryx_artifacts_rejected_total`.
- Caching indexes only: `peryx_upstream_refreshes_total`, `peryx_upstream_pages_changed_total`,
  `peryx_stale_pages_served_total`, `peryx_upstream_errors_total`.
- Hosted indexes only: `peryx_artifacts_uploaded_total`.
- Ecosystem families: `peryx_metadata_served_total` is PyPI's PEP 658/714 `.metadata` sibling counter. A rising value
  proves clients resolve through the metadata fast path instead of downloading artifacts.
  `peryx_provenance_served_total` is the PEP 740 provenance-object counter.
- Hosted quota families: `peryx_pypi_quota_admitted_total` and `peryx_pypi_quota_rejected_total` count project quota
  decisions.
- Catalog families: `peryx_catalog_syncs_total`, `peryx_catalog_published_total`, `peryx_catalog_not_modified_total`,
  `peryx_catalog_errors_total`, and `peryx_catalog_projects` describe root-catalog synchronization.

Process and availability metric families can appear beside the PyPI families:

- Scheduler: `peryx_jobs_started_total`, `peryx_jobs_finished_total`, `peryx_jobs_rejected_total`, and
  `peryx_jobs_running`.
- Replica frontier: `peryx_ha_distributed_caught_up`, `peryx_ha_distributed_serial`,
  `peryx_ha_distributed_changes_total`, `peryx_ha_distributed_blobs_total`, `peryx_ha_distributed_sync_errors_total`,
  `peryx_ha_distributed_primary_serial`, and `peryx_ha_distributed_lag`.
- Replica apply: `peryx_availability_sync_cycles_total`, `peryx_availability_sync_errors_total`,
  `peryx_availability_pending_serials`, and `peryx_availability_apply_seconds`.
- Availability worker: `peryx_availability_worker_threads`, `peryx_availability_worker_slots`,
  `peryx_availability_worker_slots_active`, `peryx_availability_worker_rejected_total`, and
  `peryx_availability_worker_panics_total`.
- Datacenter durability: `peryx_dc_ack_durable_total`, `peryx_dc_ack_pending_total`, `peryx_dc_ack_unknown_total`,
  `peryx_dc_ack_quorum_acknowledged`, `peryx_dc_ack_quorum_required`, and `peryx_dc_ack_quorum_remaining`.

The label vocabulary uses bounded request groups, registered ecosystem IDs, and index roles. Repository names, package
names, user or actor identifiers, request paths, raw errors, credentials, tokens, and URLs never become metric names or
labels. Use `/+stats` when repository, project, or file detail is required. Keep `/metrics` access-controlled as part of
the operational surface even though these fields are absent.

This contract follows [Prometheus label guidance](https://prometheus.io/docs/practices/naming/) and OpenTelemetry's
[data-minimization guidance](https://opentelemetry.io/docs/security/handling-sensitive-data/): omit sensitive dimensions
when aggregate data answers the operational question.

### Metrics compatibility

The bounded series replace the per-repository series. Existing dashboards and alerts must use these names:

| Removed series                      | Replacement                          |
| ----------------------------------- | ------------------------------------ |
| `peryx_index_pages_total`           | `peryx_pages_served_total`           |
| `peryx_index_downloads_total`       | `peryx_artifacts_served_total`       |
| `peryx_index_download_bytes_total`  | `peryx_artifacts_served_bytes_total` |
| `peryx_index_rejected_total`        | `peryx_artifacts_rejected_total`     |
| `peryx_index_refreshes_total`       | `peryx_upstream_refreshes_total`     |
| `peryx_index_pages_changed_total`   | `peryx_upstream_pages_changed_total` |
| `peryx_index_stale_served_total`    | `peryx_stale_pages_served_total`     |
| `peryx_index_upstream_errors_total` | `peryx_upstream_errors_total`        |
| `peryx_index_uploads_total`         | `peryx_artifacts_uploaded_total`     |
| `peryx_index_metadata_total`        | `peryx_metadata_served_total`        |

The `index` label was also removed from `peryx_upstream_rate_limit_denied_total` and `peryx_upstream_inflight_fetches`.
Replace an instance-wide download query such as `sum by (ecosystem, role) (rate(peryx_index_downloads_total[5m]))` with
`rate(peryx_artifacts_served_total[5m])`. Replace `sum(rate(peryx_upstream_rate_limit_denied_total{index=~".+"}[5m]))`
with `rate(peryx_upstream_rate_limit_denied_total[5m])`. There is no per-repository Prometheus replacement because that
dimension was the source of unbounded cardinality and secret exposure; use `/+stats` for current per-repository totals.

## Repository management

The index API accepts a PyPI definition with the registered `pypi` identifier. This request creates a managed route
without inventing a neutral ecosystem name:

```shell
curl -sS -u "$ADMIN" https://packages.example/+repositories \
  -H 'content-type: application/json' \
  -d '{"route":"python/internal","display_name":"Internal Python packages","ecosystem":"pypi","definition":{}}'
```

The response returns a stable repository ID and an `ETag`. Updates, disable, and enable operations send that value in
`If-Match`; see the [repository management API](@/core/repositories/repositories.md) for conflict and authorization
rules.
