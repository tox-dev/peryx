+++
title = "Proxy a private upstream"
description = "Point peryx at Artifactory, GitLab, or any other PEP 503 index, with credentials."
weight = 3
+++

Declare a cached index whose `[[index.upstream]]` source points at the upstream's simple-index URL. Two authentication
styles cover the common servers; a bearer token wins when you set both.

## Artifactory or GitLab (bearer token)

```toml
[[index]]
ecosystem = "pypi"
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
token = "<access-token>"
```

## pypi.org-style Basic auth

pypi.org tokens use the `__token__` username convention from the
[`.pypirc` specification](https://packaging.python.org/en/latest/specifications/pypirc/):

```toml
[[index]]
ecosystem = "pypi"
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://private.example/simple/"
username = "__token__"
password = "<token>"
```

Start peryx with `--config` and install through `http://<host>:<port>/corp/simple/`.

## Keep the token out of the config file

Use a `*_file` or `*_env` sibling for a `password` or `token` to keep the secret in a mounted file or injected
environment variable:

```toml
[[index]]
ecosystem = "pypi"
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://private.example/simple/"
username = "__token__"
password_file = "/run/secrets/corp-token" # or password_env = "PERYX_CORP_TOKEN"
```

peryx reads the source at startup. It reports missing, empty, or oversized files and unset variables without printing
their values. The [configuration reference](@/core/configuration.md#upstream-credential-sources) covers systemd and
Kubernetes secret layouts, precedence, redaction, and migration from inline credentials.

## Read Basic credentials from netrc

One opt-in netrc file can hold Basic credentials outside `peryx.toml`. This uses the same `machine`, `login`, and
`password` form as [pip](https://pip.pypa.io/en/stable/topics/authentication/):

```toml
netrc = "/run/secrets/upstream.netrc"

[[index]]
ecosystem = "pypi"
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://private.example/simple/"
```

```text
machine private.example
login __token__
password pypi-token
```

Run `chmod 600 /run/secrets/upstream.netrc` on Unix. A `token`, or a complete `username` and `password` pair on the
index, overrides netrc. The [configuration reference](@/core/configuration.md#upstream-netrc-credentials) covers custom
ports, `default` entries, startup errors, and redirect isolation.

## Sync for offline use

`peryx mirror sync` uses the same upstream URL and credentials as serving. Configure the working set next to the cached
index, then populate and verify the cache while the upstream is reachable:

```toml
[[index]]
ecosystem = "pypi"
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://private.example/simple/"
token = "<token>"

[index.prefetch]
requirements = ["requirements.txt"]
```

```shell
peryx mirror sync corp --config peryx.toml
peryx mirror verify corp --config peryx.toml
peryx serve --config peryx.toml --offline
```

Set `offline = true` on the cached index when only that upstream should stay cache-only. Use the top-level
`offline = true` or `serve --offline` when every cached index in the process must avoid network access.

### Sync every project name

Set `mode = "all"` when the mirror must discover all projects exposed by the upstream root:

```toml
[index.prefetch]
mode = "all"
```

The sync negotiates the
[PyPA Simple Repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) and
[PEP 691](https://peps.python.org/pep-0691/) JSON root first, then accepts the
[PEP 503](https://peps.python.org/pep-0503/) HTML form. It records project names only. Project pages, release metadata,
artifact files, and metadata siblings remain subject to the prefetch filters and their normal fetches. Warehouse's
[root implementation](https://github.com/pypi/warehouse/blob/main/warehouse/api/simple.py) establishes the production
shape this path targets: display names, canonical links, a last-serial extension, and a root large enough to require
streaming.

peryx writes the root transfer to a temporary file before changing catalog metadata. The parser writes batches of 10,000
canonical/display-name pairs into a staging generation. After reaching a valid end of document, peryx publishes the
generation with one pointer change. A truncated transfer, malformed document, unsupported Simple API major, invalid
name, or failed batch leaves the previous generation active. The next sync removes abandoned staging and retired
generations in bounded batches. The server's persistent `writer_identity` claim enforces one writer across processes;
concurrent sync calls within one process share a per-index lock and fetch. devpi's
[`ProjectNamesCache`](https://github.com/devpi/devpi/blob/main/server/devpi_server/mirror.py) also retains its previous
name set after a refresh failure.
[bandersnatch's mirror](https://github.com/pypa/bandersnatch/blob/main/src/bandersnatch/mirror.py) does not advance the
completed serial after a synchronization error. Peryx applies both rules to batched root parsing: it may discard durable
staging work, but readers see the last complete generation.

Peryx sends `If-None-Match` on the next sync when the upstream supplied an ETag. `If-Modified-Since` is the fallback
when only `Last-Modified` is available, matching the precedence in
[HTTP conditional requests](https://www.rfc-editor.org/rfc/rfc9110.html#name-preconditions). A `304 Not Modified` keeps
the generation and name rows, updates the fetch time, and merges only validator headers present on the response, as
[HTTP cache validation](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.3.4) requires. Validators belong to the
configured upstream source, so a routed fallback never receives another source's validator.

peryx limits the decompressed root to 256 MiB and 2,000,000 entries. In July 2026, Warehouse's JSON and HTML roots are
about 42 to 44 MiB and list fewer than one million projects. The limit bounds disk use, parser work, and recovery while
allowing the root to grow. The redirect policy permits at most ten redirects. peryx strips user information, query
strings, and fragments from persisted source and final URLs.

`/metrics` reports `peryx_catalog_syncs_total`, `peryx_catalog_published_total`, `peryx_catalog_not_modified_total`,
`peryx_catalog_errors_total`, and the `peryx_catalog_projects` gauge. These series use the bounded `ecosystem` and
`role` labels. They omit upstream names, URLs, index names, and project names from Prometheus labels.

### Sync project file metadata

Name discovery populates the root. A project-detail sync fetches its HTML or JSON Simple response and records remote
file metadata without downloading distribution bytes. The mirror records each file's identity, hash, and size before
fetching the artifact.

Each admitted file records its filename, hashes, size, upload time, yank state, metadata-sibling link, provenance link,
and upstream URL, parsed from the
[PyPA Simple Repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) with the
per-file [PEP 700](https://peps.python.org/pep-0700/) `size` and `upload-time`,
[PEP 592](https://peps.python.org/pep-0592/) yanks, [PEP 658](https://peps.python.org/pep-0658/) metadata siblings, and
[PEP 740](https://peps.python.org/pep-0740/) provenance. The generation around them retains the source index that
produced it, the `ETag`/`Last-Modified`/last-serial validators, the observation time, and a monotonic generation number.
HTML and JSON responses parse into the same fields, so an upstream serving either form yields identical records.

peryx applies the repository policy before admitting a file, so denied files do not reach installers. It also skips a
file without a `sha256` because it cannot content-address that file. Admission registers the digest-keyed download
source and metadata sibling. Until a cache miss fetches the bytes by digest, the metadata describes a file that remains
upstream.

peryx writes the detail transfer to a bounded temporary file before creating staging rows, so the upstream request holds
no metadata transaction open. The parser streams the file array and commits batches of 10,000 records into a staging
generation; a generated project with one million files does not occupy memory or one transaction. After reaching a valid
end of document, peryx publishes the generation with one pointer change and sweeps the displaced generation in bounded
batches. A truncated transfer, malformed document, unsupported Simple API major, or failed publication leaves the
previous generation active. The same retain-on-failure discipline appears in
[bandersnatch](https://github.com/pypa/bandersnatch/blob/main/src/bandersnatch/mirror.py) applies to per-release
metadata, and the reason [devpi](https://github.com/devpi/devpi/blob/main/server/devpi_server/mirror.py) keeps its last
good project serial when a refresh errors.

Peryx sends `If-None-Match` on the next sync when the active generation carried an `ETag`. A `304 Not Modified` reuses
that generation without moving an artifact. Peryx advances the observation time and merges validators present on the
response, as [HTTP cache validation](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.3.4) requires. A `404` leaves
the prior generation in place. Peryx limits a detail response to 256 MiB and 2,000,000 files. The redirect policy
permits at most ten redirects, and concurrent syncs of one project inside a process share a lock and fetch. Peryx strips
user information, query strings, and fragments from persisted source and final URLs.

### Schedule bounded metadata refreshes

A catalog job combines the atomic root and project-generation paths without downloading artifact bytes. Schedule it on
an online cached index when installers should find project metadata before their first request:

```toml
[[jobs.schedule]]
job = "catalog_sync"
interval_secs = 21600
repository = "corp"
max_projects = 10000
concurrency = 4
timeout_secs = 900
```

Run the identical job once while validating an upstream or warming a new node:

```shell
peryx job run --config peryx.toml --repository corp --max-projects 10000 --concurrency 4 --timeout-secs 900
```

peryx publishes the root before project work begins. Cancellation or timeout stops new project requests and drops
in-flight transfers; completed generations remain available. A run admits projects in canonical-name order up to
`max_projects`. It bounds concurrent metadata requests apart from the node's job-worker limit and emits at most 100
progress updates. Named multi-source routes use their fallback rules unless `source` selects one configured upstream.
Set an interval longer than a typical run and schedule it outside peak request periods.

## HTML upstreams

Some upstreams, including [Artifactory](https://jfrog.com/artifactory/), serve the
[PEP 503](https://peps.python.org/pep-0503/) HTML form instead of PEP 691 JSON. peryx requests
[PEP 691](https://peps.python.org/pep-0691/) JSON first, parses HTML when the upstream returns it, and serves JSON to
[pip](https://pip.pypa.io/) and [uv](https://docs.astral.sh/uv/). Content negotiation occurs per response and needs no
configuration. The upstream response must send a Simple API content type (`text/html`,
`application/vnd.pypi.simple.v1+html`, or `application/vnd.pypi.simple.v1+json`); other content types return `502` with
the upstream URL in the error body.

## Notes

- Inline credentials make the config file secret, so restrict it: `chmod 600 peryx.toml`.
- Each cached index keeps its own credentials. A cached file remembers which cached index it came from, and a later
  cache-miss fetch reuses that index's authentication.
- Peryx asks upstream for `Accept-Encoding: identity` during artifact downloads. This makes the bytes pip and uv verify
  match the cached bytes. Same-origin redirects keep the cached index's credentials; cross-origin requests do not.
- `cache_ttl_secs` (default 1800) controls how long peryx serves a cached project page before revalidating it against
  the upstream with `If-None-Match`.
- Peryx caches upstream `404` misses for project pages and `.metadata` siblings for 30 seconds.

## Related

- Why one URL with shadowing beats `--extra-index-url`: [the index model](@/core/indexes.md)
- Serve a network with no internet route: [air-gapped](@/guides/air-gapped.md)
- Upstream capability differences peryx papers over: [standards](@/reference/standards.md)
