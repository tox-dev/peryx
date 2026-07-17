+++
title = "What peryx serves on the Simple API"
description = "Why peryx advertises only what its bytes back up: an api-version derived from the upstream, a gpg-sig marker dropped when it rewrites a file URL, and a 301 to the canonical slashed URL."
weight = 6
aliases = [ "/ecosystems/pypi/api-version/", "/ecosystems/pypi/gpg-sig/", "/ecosystems/pypi/trailing-slashes/"]
+++

A Simple API response makes promises: a version number says which fields the page is allowed to carry, a `gpg-sig`
marker says a signature sits next to a file, a URL says where a resource lives. peryx serves a cache and a merge of
upstreams, so every one of those promises has to hold for the bytes peryx actually hands back, not for the protocol
peryx happens to implement. This page covers three places where peryx trims what it advertises down to what it can back
up: the derived api-version, the dropped signature marker, and the redirect to the canonical URL.

## An honest Simple API version

A Simple API version number is a promise. When a page advertises [PEP 700](https://peps.python.org/pep-0700/)'s `1.1` or
higher, it tells the client that a top-level `versions` array and a per-file `size` are present, and a client written
for that version reads them without checking. peryx used to stamp `1.4` on every page it served, including pages
re-served from an upstream that promised neither field. It now derives the version from what the upstream provides.

### The promise a version makes

The Simple API is versioned so a client can tell what a page is allowed to contain. PEP 700 raised the minimum to `1.1`
and made two fields mandatory: `versions`, the list of every release of the project, and `size`, the byte count on every
file. From `1.1` on, a client may treat both as always present. That is the whole point of the version bump: it lets a
resolver read `size` to plan a download, or read `versions` to enumerate releases, without a guard around each access.

### What over-advertising breaks

An upstream that speaks PEP 691 `1.0`, or a plain PEP 503 HTML index that declares no version at all, promises neither
field. Its pages can, and do, omit `size` on a file and carry no `versions` array. Re-serving such a page under a `1.4`
label hands the client a document that contradicts its own header.

A PEP 700-aware client trusts the label. It reads `file["size"]` to size a progress bar or a disk-space check, and
`page["versions"]` to list the releases, because `1.4` told it they are there. When they are not, the lookup fails: a
missing key raises, a total-bytes sum is wrong, a release enumeration comes back empty. The failure lands in the client,
far from peryx, and looks like a malformed index rather than an overstated version. The bytes were fine for what they
were; the label claimed more than the bytes carried.

### Derive, do not assert

peryx now advertises the version the payload satisfies. An upstream that declares `1.1` or higher promises PEP 700's
fields, peryx passes them through, and it keeps its `1.4` ceiling. An upstream at `1.0`, or one that declares no
version, promises neither, so peryx serves `1.0`, and a client reads that page knowing `size` and `versions` may be
absent. The number now matches the guarantees of the bytes underneath it.

The alternative, always satisfying `1.4` by synthesizing the missing fields, was a heavier contract than a cache should
sign. Deriving `size` for every file means knowing every file's length, which a cold cache does not; deriving `versions`
means the merged list is authoritative even when a layer was skipped. Lowering the version instead keeps peryx honest
without making it pretend to know more than it does.

### The weakest layer wins

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

### What peryx serves for a file

When peryx content-addresses an upstream file, it rewrites the file URL to its own `/{route}/files/{sha256}/{filename}`
route and serves the file from there. Under that route it serves two things: the artifact blob, and the
[PEP 658](https://peps.python.org/pep-0658/) `.metadata` sibling that lets a resolver read dependency metadata without
downloading the whole wheel. It does not serve the detached OpenPGP signature, the `.asc` sibling that
[PEP 503](https://peps.python.org/pep-0503/) places next to the file URL. That signature only ever existed at the
upstream URL, and peryx has replaced that URL with its own.

The `gpg-sig` marker (`data-gpg-sig` in HTML, `has_sig` in the legacy JSON) is a promise about the file URL: it says an
`.asc` is reachable at `{file_url}.asc`. Upstream, the marker rode along with the file record when peryx rewrote the
URL, so peryx kept advertising a signature at a route where none exists.

### The failure it prevents

A client that trusts the marker does the obvious thing: it fetches `{file_url}.asc` to get the signature. Before this
change, that URL was peryx's own file route, where no `.asc` is served, so the client got a `404`. The marker named a
file that was not there.

Two ways make the page honest again. peryx could fetch and cache the upstream `.asc` and serve it next to the blob, the
way it serves the `.metadata` sibling. Or it could drop the marker whenever it rewrites the URL, so it never promises a
signature it will not serve. peryx takes the second:
[PyPI deprecated GPG signatures in 2023](https://blog.pypi.org/posts/2023-05-23-removing-pgp/) and stopped serving them,
so wiring up a whole fetch-and-serve path for a signature the ecosystem is retiring would be effort spent on a dead
surface. Dropping a marker peryx cannot back is the smaller fix.

### Where the marker survives

The marker is not gone from peryx. A file peryx serves at its **upstream URL** unchanged, a pass-through, keeps it,
because the upstream `.asc` is still reachable next to that URL. Pass-through happens when peryx has no `sha256` to
content-address the file by and so leaves the URL alone. There the marker is still true, so peryx passes it through
untouched. The marker tracks one fact only: whether the URL peryx hands out has a signature next to it.

## Provenance and attestations

A file uploaded with [PEP 740](https://peps.python.org/pep-0740/) attestations carries a `provenance` key on its Simple
API entry (a `data-provenance` attribute in HTML), pointing at the provenance object peryx serves for that distribution.
The URL is peryx's own, alongside the file's download URL: `.../files/{sha256}/{filename}.provenance`, the same
digest-addressed shape as the `.metadata` sibling.

### Served by digest, not by history

The provenance route resolves the distribution's stored bundle from the artifact's SHA-256 alone, a single keyed lookup,
never a scan of the project's releases. That keeps a provenance fetch as cheap as a metadata fetch no matter how many
versions a project has, and it is why the provenance is keyed by the file's own digest rather than by project and
version. The response is the provenance object — `{"version": 1, "attestation_bundles": [...]}` — served with the
`application/vnd.pypi.integrity.v1+json` media type PEP 740 assigns it, and cached as immutable, because a digest names
exactly one set of bytes.

### What the bundle holds

peryx wraps the attestations a publisher uploaded into one provenance object. The `publisher` field is `null`: peryx
does not resolve the uploader to a Trusted Publisher identity, and PEP 740 makes the field nullable for exactly that
case. The attestations themselves — envelope, signature, certificate, transparency entries, predicate — are served back
verbatim, so a verifier sees precisely what was signed. peryx stores the bundle in its blob store, content-addressed,
with a small digest-keyed pointer row, the way it stores a PEP 658 metadata sibling; the metadata store never buffers
the whole bundle.

### Visibility tracks the distribution

The provenance is reachable only through the file's advertised `provenance` URL, so it appears and disappears with the
file. A yanked file keeps its provenance (it is still served, just marked yanked); a trashed file loses it from every
page; a restore brings it back, since the bundle is kept through the trash and re-advertised with the file. A direct
fetch of a provenance URL for a distribution the index no longer holds returns `404`, the same as a fetch for the file
itself would.

## The canonical trailing slash

A Simple API index is `.../simple/` and a project is `.../simple/{project}/`. Both end in a slash. Ask for either
without it and peryx sends back a `301` to the slashed form rather than a `404`.

### The canonical URL has a slash

[PEP 503](https://peps.python.org/pep-0503/) defines the Simple API URLs with a trailing slash, and says a client that
requests a URL without it should be redirected to the version with it. The slashed URL is the canonical one; the
slashless URL is a request for a resource that lives one redirect away. Answering it with a `404` would be telling the
client the project does not exist, which is wrong: it exists, at the URL one hop over.

A redirect says the resource has a canonical location and the client should use it from now on, which is what
`301 Moved Permanently` means. A well-behaved client follows the hop, and a caching one remembers it and skips the round
trip next time.

### Matching what clients already expect

pypi.org, served by [Warehouse](https://github.com/pypi/warehouse), returns exactly this `301` for a slashless Simple
URL. Tools written against pypi.org, and the installers themselves, are built for that behavior. A cache that fronts or
stands in for pypi.org should not answer differently: a client that works against the real index should work against
peryx unchanged. Returning a `404` where pypi.org returns a `301` is the kind of difference that surfaces only in the
one script that drops the slash, and only in production.

### Saving a client a failed request

Without the redirect, a slashless request is a dead end. The client gets a `404`, and the person or tool behind it has
to notice the missing slash, add it, and try again, or worse, conclude the package is gone. The redirect turns that dead
end into a working request: the client is handed the right URL and gets the page. One request that would have failed
becomes one that succeeds, at the cost of a single extra round trip that a caching client pays only once.

### The normalization tie-in

The redirect does not only add the slash; it also normalizes the project name. PEP 503 folds a name to lowercase and
collapses any run of `.`, `-`, or `_` to a single `-`, so `Flask.Test`, `flask_test`, and `flask-test` are all the same
project. That project has one canonical page, at `.../simple/flask-test/`. A slashless request for any spelling is a
request for that one page under a non-canonical name, so the natural target of the redirect is the normalized, slashed
URL. Adding the slash and normalizing the name are the same act: routing the request to the single canonical URL for the
resource it named.

This is why the `Location` is always the canonical form, slash and normalization together, rather than the requested
path with a slash tacked on.

## In practice

- The exact rules across JSON, HTML, and legacy JSON, with every edge and status:
  [Simple API serving](@/ecosystems/pypi/reference/simple-api.md)
- Diagnose a mirror stuck at api-version `1.0`, move a tool off the gpg-sig marker, or follow the trailing-slash
  redirect: [diagnose Simple API serving](@/ecosystems/pypi/guides/simple-api.md)
- Watch the derived version, the dropped signature, and the slashless redirect happen:
  [observe Simple API behavior](@/ecosystems/pypi/tutorials/simple-api-behavior.md) </content>
