+++
title = "Simple API response guarantees"
description = "Upstream-derived API versions, reachable signature markers, provenance objects, and canonical URL redirects."
weight = 6
aliases = [ "/ecosystems/pypi/api-version/", "/ecosystems/pypi/gpg-sig/", "/ecosystems/pypi/trailing-slashes/"]
+++

A Simple API version defines the fields a page may carry. A `gpg-sig` marker promises a reachable signature, and each
URL identifies a resource. peryx derives those claims from the bytes it can serve: the upstream API version, reachable
signatures, provenance objects, and the canonical resource URL.

## Simple API version

A Simple API version declares the fields present in a page. [PEP 700](https://peps.python.org/pep-0700/) requires a
top-level `versions` array and per-file `size` at version `1.1` or higher. Clients may read those fields without
checking for them. peryx used to stamp `1.4` on every page it served, including pages re-served from an upstream that
promised neither field. It now derives the version from what the upstream provides.

### Version contract

The Simple API is versioned so a client can tell what a page is allowed to contain. PEP 700 raised the minimum to `1.1`
and made two fields mandatory: `versions`, the list of every release of the project, and `size`, the byte count on every
file. PEP 700 guarantees both fields from `1.1`. A resolver can read `size` to plan a download or `versions` to
enumerate releases without guarding each access.

### Over-advertising failures

An upstream that speaks PEP 691 `1.0`, or a plain PEP 503 HTML index that declares no version at all, promises neither
field. Its pages can, and do, omit `size` on a file and carry no `versions` array. Re-serving such a page under a `1.4`
label hands the client a document that contradicts its own header.

A PEP 700-aware client trusts the label. It reads `file["size"]` to size a progress bar or a disk-space check, and
`page["versions"]` to list the releases, because `1.4` told it they are there. When they are not, the lookup fails: a
missing key raises, a total-bytes sum is wrong, a release enumeration comes back empty. The failure lands in the client,
far from peryx, and looks like a malformed index rather than an overstated version. The bytes were fine for what they
were; the label claimed more than the bytes carried.

### Version derivation

peryx now advertises the version the payload satisfies. An upstream that declares `1.1` or higher promises PEP 700's
fields, peryx passes them through, and it keeps its `1.4` ceiling. An upstream at `1.0`, or one that declares no
version, promises neither, so peryx serves `1.0`, and a client reads that page knowing `size` and `versions` may be
absent. The number now matches the guarantees of the bytes underneath it.

Derivation answers the upstream that promises little. The opposite case is an upstream that promises `1.1` and then
sends a page without `versions`, or with a file that carries no `size`. Lowering that page's version would launder a
broken upstream into a well-formed peryx page and hide the fault; peryx rejects the response instead, leaves the
previously published generation in place, and the client keeps reading the page it already had. The HTML form is held at
`1.0` for the same reason: PEP 700 leaves it unchanged from `1.0`, so its `pypi:repository-version` promises nothing
about the JSON peryx re-serves from it.

The alternative, always satisfying `1.4` by synthesizing the missing fields, was a heavier contract than a cache should
sign. Deriving `size` for every file means knowing every file's length, which a cold cache does not; deriving `versions`
means the merged list is authoritative even when a layer was skipped. Lowering the version instead keeps peryx honest
without making it pretend to know more than it does.

### Lowest upstream version

A virtual index inherits the lowest version of its layers. One pre-PEP 700 layer caps the merged page at `1.0`, because
a merged page can only guarantee a field that every contributing layer guarantees. If even one layer can serve a file
without a `size`, the merged page cannot promise `size` for all files, so it must not claim `1.1+`. The rule is the same
correctness principle applied to a stack: advertise the guarantees the whole payload meets, which is the intersection of
what the layers meet, not the maximum.

The principle carries the section: advertise only what the payload provides. A version number a cache serves is a claim
about the bytes it is serving right now, not about the protocol the cache happens to implement. peryx implements `1.4`,
but it serves `1.4` only where the page it hands back carries `1.4`'s guarantees; everywhere else it serves the honest
lower number and lets the client plan accordingly.

## GPG signatures

peryx no longer advertises a GPG signature for the files it content-addresses onto its own route. Serving the blob
without the signature forces that choice, and dropping the marker heads off a client failure.

### Served file metadata

When peryx content-addresses an upstream file, it rewrites the file URL to its own `/{route}/files/{sha256}/{filename}`
route and serves the file from there. Under that route it serves two things: the artifact blob, and the
[PEP 658](https://peps.python.org/pep-0658/) `.metadata` sibling that lets a resolver read dependency metadata without
downloading the whole wheel. It does not serve the detached OpenPGP signature, the `.asc` sibling that
[PEP 503](https://peps.python.org/pep-0503/) places next to the file URL. That signature only ever existed at the
upstream URL, and peryx has replaced that URL with its own.

The `gpg-sig` marker (`data-gpg-sig` in HTML, `has_sig` in the legacy JSON) is a promise about the file URL: it says an
`.asc` is reachable at `{file_url}.asc`. Upstream, the marker rode along with the file record when peryx rewrote the
URL, so peryx kept advertising a signature at a route where none exists.

### Missing signature failure

A client that trusts the marker fetches `{file_url}.asc`. Before this change, the URL used peryx's file route, which
does not serve `.asc`, and returned `404`. The marker named an unavailable file.

Two ways make the page honest again. peryx could fetch and cache the upstream `.asc` and serve it next to the blob, the
way it serves the `.metadata` sibling. Or it could drop the marker whenever it rewrites the URL, so it never promises a
signature it will not serve. peryx takes the second:
[PyPI deprecated GPG signatures in 2023](https://blog.pypi.org/posts/2023-05-23-removing-pgp/) and stopped serving them,
so wiring up a whole fetch-and-serve path for a signature the ecosystem is retiring would be effort spent on a dead
surface. Dropping a marker peryx cannot back is the smaller fix.

### Signature-marker retention

The marker is not gone from peryx. A file peryx serves at its **upstream URL** unchanged, a pass-through, keeps it,
because the upstream `.asc` is still reachable next to that URL. Pass-through happens when peryx has no `sha256` to
content-address the file by and so leaves the URL alone. There the marker is still true, so peryx passes it through
untouched. The marker tracks one fact only: whether the URL peryx hands out has a signature next to it.

## PEP 658 metadata claims

A `.metadata` sibling is what one index's page claims about a file it publishes, not a property of the distribution's
bytes. Two indexes can publish the same wheel and advertise different sidecars, and one of them can advertise none at
all. peryx records each claim against the publication that made it: the cached index, the project, the artifact digest,
and the filename. A `.metadata` request resolves the claim belonging to the index the request arrived on, and fetches it
through that index's own credentials.

A virtual index resolves the claim the same way its pages merge: it walks its layers in shadow order and stops at the
first layer that publishes the file. A file whose winning publication advertises no sidecar therefore inherits none from
a layer behind it.

Metadata peryx reads out of the artifact itself, extracted from the wheel or uploaded alongside it, is a function of the
digest rather than a claim. That stays shared: every publication of the same digest resolves the same bytes.

## Provenance and attestations

A file uploaded with [PEP 740](https://peps.python.org/pep-0740/) attestations carries a `provenance` key on its Simple
API entry (a `data-provenance` attribute in HTML), pointing at the provenance object peryx serves for that distribution.
The URL is peryx's own, alongside the file's download URL: `.../files/{sha256}/{filename}.provenance`, the same
digest-addressed shape as the `.metadata` sibling.

### Provenance belongs to the publication

A bundle is what one publisher attested about one release, not a property of the distribution's bytes, so peryx records
it against the publication that carried it: the hosted index, the normalized project, the artifact digest, and the
filename. Two hosted indexes can accept different bundles for byte-identical distributions, and each route serves its
own; neither inherits the other's. The blob store still deduplicates the bundle bytes underneath when they match.

The route resolves the requesting index's publication in one keyed lookup rather than scanning the project's releases.
It serves `{"version": 1, "attestation_bundles": [...]}` with the `application/vnd.pypi.integrity.v1+json` media type
and caches it as immutable, because that publication's bundle never moves: peryx has no path that rewrites one, and a
re-upload of the same bytes carrying different attestations is refused rather than applied.

An upstream provenance URL is mutable even though the distribution digest is not. Repository policy can leave that URL
direct or replace it with a local route. The local route can proxy each request or retain and revalidate the body.
Retained records keep the upstream source, media type, validators, and freshness state. A record stores at most 2 MiB of
structurally accepted, unverified JSON. A fresh retained body needs no upstream request. peryx revalidates stale bodies
and can use the previous structurally accepted body within the stale bound after a transient failure. Responses expose
source and availability as described in the
[Simple API reference](@/ecosystems/pypi/reference/simple-api.md#provenance-and-attestations).

### Provenance bundle

peryx wraps the attestations a publisher uploaded into one provenance object. The `publisher` field is `null`: peryx
does not resolve the uploader to a Trusted Publisher identity, and PEP 740 makes the field nullable for exactly that
case. peryx serves each attestation's envelope, signature, certificate, transparency entries, and predicate without
changes, so a verifier sees the signed content. peryx stores the bundle in its content-addressed blob store, with a
small digest-keyed pointer row, the way it stores a PEP 658 metadata sibling; the metadata store never buffers the whole
bundle.

For upstream provenance, peryx checks the media type, size, and version-1 document structure. It does not verify
signatures or certificates. It neither resolves publisher identities nor consults transparency entries, and it does not
label an upstream identity as verified. `no-cache` forces revalidation, while `no-store` clears any retained body and
validators.

### Visibility tracks the distribution

The provenance is reachable only through the file's advertised `provenance` URL, so it appears and disappears with the
file. A yanked file keeps its provenance (it is still served, just marked yanked); a trashed file loses it from every
page; a restore brings it back, since the bundle is kept through the trash and re-advertised with the file. A direct
fetch of a provenance URL for a distribution the index no longer holds returns `404`, the same as a fetch for the file
itself would.

## Canonical trailing slash

A Simple API index is `.../simple/` and a project is `.../simple/{project}/`. Both end in a slash. Ask for either
without it and peryx sends back a `301` to the slashed form rather than a `404`.

### Canonical URL

[PEP 503](https://peps.python.org/pep-0503/) defines Simple API URLs with a trailing slash and directs indexes to
redirect slashless requests. A `404` would incorrectly report that the project does not exist.

`301 Moved Permanently` identifies the canonical URL. Clients follow it, and caching clients can reuse it.

### Client compatibility

pypi.org, served by [Warehouse](https://github.com/pypi/warehouse), returns `301` for a slashless Simple URL. Matching
that response preserves installer and script compatibility.

### Redirect behavior

A slashless request returns `301` with the canonical URL. The first request adds one round trip; a caching client can
reuse the redirect target.

### Normalized redirects

The redirect adds the slash and normalizes the project name. PEP 503 folds a name to lowercase and collapses any run of
`.`, `-`, or `_` to a single `-`, so `Flask.Test`, `flask_test`, and `flask-test` are all the same project. That project
has one canonical page, at `.../simple/flask-test/`. A slashless request for any spelling is a request for that page
under a non-canonical name. Its `Location` contains the normalized, slashed URL.

## Operational checks

- The exact rules across JSON, HTML, and legacy JSON, with every edge and status:
  [Simple API serving](@/ecosystems/pypi/reference/simple-api.md)
- Diagnose a mirror stuck at api-version `1.0`, move a tool off the gpg-sig marker, or follow the trailing-slash
  redirect: [diagnose Simple API serving](@/ecosystems/pypi/guides/simple-api.md)
- Version derivation, signature filtering, and slashless redirects:
  [Simple API behavior](@/ecosystems/pypi/tutorials/simple-api-behavior.md)
