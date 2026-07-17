+++
title = "Simple API serving"
description = "The exact serving rules on the Simple API: how peryx derives the advertised api-version, when it keeps or drops the gpg-sig marker, and the 301 it returns for a slashless index or project URL."
weight = 4
aliases = [ "/ecosystems/pypi/reference/api-version/", "/ecosystems/pypi/reference/gpg-sig/", "/ecosystems/pypi/reference/trailing-slash/"]
+++

peryx serves a Simple page that advertises only what the bytes behind it guarantee: a version derived from the upstream,
a signature marker kept only where a signature is reachable, and a redirect to the one canonical URL. This page states
each rule exactly. For why peryx serves this way, see
[what peryx serves on the Simple API](@/ecosystems/pypi/serving.md); for the routes, see
[HTTP endpoints](@/ecosystems/pypi/reference/endpoints.md).

## The advertised Simple API version

Every Simple page peryx serves carries a version: `meta.api-version` in the [PEP 691](https://peps.python.org/pep-0691/)
JSON, `pypi:repository-version` in the [PEP 503](https://peps.python.org/pep-0503/) HTML `<meta>`. peryx does not stamp
a fixed number. It derives the version from what the upstream it proxies declared, so the advertised version never
promises a field the re-served payload can omit.

### The rule

peryx reads the version the upstream declared for the project, then maps it to what it serves:

| Upstream declares                                                     | peryx serves | Why                                                         |
| --------------------------------------------------------------------- | ------------ | ----------------------------------------------------------- |
| `1.1`, `1.2`, `1.3`, `1.4`, `1.5`, … (minor ≥ 1)                      | `1.4`        | PEP 700 makes `versions` and per-file `size` mandatory here |
| `1.0`                                                                 | `1.0`        | PEP 691 mandates neither field                              |
| nothing (a bare PEP 503 HTML index, or JSON that omits `api-version`) | `1.0`        | promises neither field                                      |
| a major other than `1` (`2.0`, …)                                     | rejected     | unsupported major; the upstream page is not served          |
| a version that does not parse (`1.x`, `abc`)                          | rejected     | invalid version; the upstream page is not served            |

`1.4` is peryx's own ceiling: the highest version it implements. The threshold that decides between the ceiling and the
base is [PEP 700](https://peps.python.org/pep-0700/)'s, minor version `1`. Above it, every guarantee through `1.4` is
one peryx meets by passing the upstream's fields through, so it advertises the full ceiling rather than echoing the
exact minor the upstream sent.

### What the version guarantees

PEP 700 raised the Simple API to `1.1` and made two fields mandatory in the JSON serialization:

- **`versions`**: a top-level array of every release version of the project.
- **`size`**: an integer byte count on every file entry.

A page that advertises `1.1` or higher promises both are present; `1.0` promises neither. peryx advertises `1.4` only
when the upstream declared `1.1+`, where those fields are guaranteed in the bytes it re-serves, and falls back to `1.0`
otherwise.

### Virtual indexes take the weakest layer

A virtual index merges the project pages of its layers, and it is only as capable as its least capable layer. peryx
starts the merged page at its `1.4` ceiling and drops it to `1.0` the moment any layer that resolved the project serves
`1.0`. A single pre-PEP 700 layer therefore caps the merged page at `1.0`, because the merged payload can no longer
guarantee `versions` and `size` for every file.

The cap is per project. A layer only lowers the version when it returns a page for the requested project; a layer that
does not carry the project has no say in its version.

### JSON only; HTML is unaffected

PEP 700 changes the JSON serialization alone. The HTML serialization defines no `versions` array and no per-file `size`,
so it carries none of PEP 700's guarantees at any version number. The derivation sets the version on the served `meta`,
which both serializations render, but the honesty concern is JSON-only: an HTML page has no PEP 700 field to
over-advertise.

### What the version derivation does not do

- It does not synthesize `versions` or `size` to reach `1.4`. When the upstream promises neither, peryx lowers the
  version rather than inventing the fields.
- It does not echo the upstream's exact minor. Any `1.1+` maps to `1.4`, peryx's ceiling, not to the number the upstream
  sent.
- It does not serve an unsupported major or an unparseable version. Those are errors, not a page.

## The gpg-sig marker

The Simple API can mark a file as having a detached OpenPGP signature next to it.
[PEP 503](https://peps.python.org/pep-0503/) spells the marker `data-gpg-sig` on the HTML anchor,
[PEP 691](https://peps.python.org/pep-0691/) spells it `gpg-sig` on the JSON file object, and the legacy PyPI JSON API
spells it `has_sig`. All three mean the same thing: a signature is served as an `.asc` sibling of the file URL, at
`{file_url}.asc`.

### When peryx keeps it

peryx keeps the marker when it serves a file at its **upstream URL** unchanged, a pass-through. That happens when peryx
has no `sha256` to content-address the file by, so it does not rewrite the URL. The upstream `.asc` sits next to the
upstream file, which is still where the file URL points, so the marker stays true and peryx passes it through.

### When peryx drops it

peryx drops the marker when it **content-addresses** the file, rewriting the file URL to its own
`/{route}/files/{sha256}/{filename}` route (see [endpoints](@/ecosystems/pypi/reference/endpoints.md)). At that route
peryx serves the blob and the [PEP 658](https://peps.python.org/pep-0658/) `.metadata` sibling, and nothing else. There
is no `.asc` there, so peryx clears the marker rather than advertise a signature it will not serve. A file carries a
`sha256` in almost every real index, so this is the common case.

The rule holds across all three surfaces, and both serving paths agree on it:

| Surface          | Marker         | Content-addressed file | Pass-through file |
| ---------------- | -------------- | ---------------------- | ----------------- |
| PEP 691 JSON     | `gpg-sig`      | omitted                | passed through    |
| PEP 503 HTML     | `data-gpg-sig` | omitted                | passed through    |
| Legacy PyPI JSON | `has_sig`      | `false`                | reflects upstream |

The JSON simple API served to `pip` and `uv` streams through one transformer; the HTML page and the legacy JSON are
rendered from the buffered resolve path. Both clear the marker on the same condition, so a file reads the same way
whichever surface a client asks for.

### What peryx does not serve for a file

For a content-addressed file, peryx serves exactly two things under its file route: the artifact blob at
`/{route}/files/{sha256}/{filename}`, and its core-metadata at `.../{filename}.metadata`. It does **not** serve an
`.asc` at `.../{filename}.asc`; that route returns `404`. The detached signature only ever lived at the upstream URL,
which peryx has replaced with its own for a content-addressed file, so dropping the marker keeps the page honest about
what is reachable.

## Provenance and attestations

A file uploaded with [PEP 740](https://peps.python.org/pep-0740/) attestations advertises a provenance object; a file
without them advertises none. `{route}` below is the index's route.

| Surface      | Key / attribute   | With attestations                               | Without |
| ------------ | ----------------- | ----------------------------------------------- | ------- |
| PEP 691 JSON | `provenance`      | `/{route}/files/{sha256}/{filename}.provenance` | omitted |
| PEP 503 HTML | `data-provenance` | the same URL, as an escaped attribute           | omitted |

- **Route.** `GET /{route}/files/{sha256}/{filename}.provenance` returns the provenance object, resolved from the
  artifact's SHA-256 by a single keyed lookup. Media type `application/vnd.pypi.integrity.v1+json`, cached immutable. A
  digest with no stored provenance returns `404`.
- **Body.** `{"version": 1, "attestation_bundles": [{"publisher": null, "attestations": [...]}]}`. The `publisher` is
  `null` — peryx does not resolve a Trusted Publisher identity. The uploaded attestations are served verbatim.
- **Upstream provenance.** A `provenance` URL a cached upstream advertises is preserved as-is, pointing at the upstream;
  peryx only mints its own provenance URL for a distribution uploaded to a hosted index with attestations.
- **Visibility.** The provenance is reachable only through the file's `provenance` URL, so it tracks the file: a yanked
  file keeps it, a trashed file drops it, a restore returns it. The upload rules cover the
  [validation and limits](@/ecosystems/pypi/reference/uploads.md#attestations).

## Trailing-slash redirects

The [Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) canonical URLs end in a
slash. A request that drops the slash on the index or a project is redirected to the slashed form rather than answered
with a `404`. `{route}` below is the index's route, for example `root/pypi`.

### Rule

| Request                          | Response | `Location`                      |
| -------------------------------- | -------- | ------------------------------- |
| `GET /{route}/simple`            | `301`    | `/{route}/simple/`              |
| `GET /{route}/simple/{project}`  | `301`    | `/{route}/simple/{normalized}/` |
| `GET /{route}/simple/`           | `200`    | served directly, not redirected |
| `GET /{route}/simple/{project}/` | `200`    | served directly, not redirected |

The status is `301 Moved Permanently`, the same status pypi.org (Warehouse) returns. `{normalized}` is `{project}` after
[PEP 503](https://peps.python.org/pep-0503/) normalization.

### Details

- **Normalization.** The project segment in the `Location` is normalized: lowercased, with every run of `.`, `-`, or `_`
  collapsed to a single `-`. `Flask.Test` redirects to `/{route}/simple/flask-test/`. An already-canonical name
  redirects to itself with the slash appended.
- **Query string.** Any query string on the request is preserved on the `Location` unchanged.
  `GET /{route}/simple/Flask.Test?extra=1` redirects to `/{route}/simple/flask-test/?extra=1`.
- **Location form.** The `Location` is a path (host-absolute), built from the request path with the route prefix intact,
  so the redirect stays on the same origin and works behind a proxy or under a nested route.
- **A project segment with a slash is not redirected.** The redirect fires only for a single segment after `simple/`. A
  path with a further slash, such as `/{route}/simple/some/thing`, is not a project name, is not redirected, and falls
  through to a `404`.
- **Already-slashed URLs are served, not redirected.** `/{route}/simple/` and `/{route}/simple/{project}/` are the
  canonical URLs; they return their content directly. Content negotiation, policy, and caching apply as normal.
- **Method.** The redirect is defined for `GET` on these two Simple read paths. It does not change the upload, yank,
  delete, files, inspect, or legacy JSON routes.

## In practice

- Why peryx serves this way: [what peryx serves on the Simple API](@/ecosystems/pypi/serving.md)
- Diagnose a mirror stuck at `1.0`, move a tool off the marker, or follow the redirect:
  [diagnose Simple API serving](@/ecosystems/pypi/guides/simple-api.md)
- Watch all three behaviors end to end:
  [observe Simple API behavior](@/ecosystems/pypi/tutorials/simple-api-behavior.md)
- The standards these implement: [standards](@/ecosystems/pypi/reference/standards.md) </content>
