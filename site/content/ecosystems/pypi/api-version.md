+++
title = "An honest Simple API version"
description = "Why peryx derives the advertised meta.api-version from the upstream it proxies instead of stamping a fixed 1.4, so a PEP 700-aware client is never told to expect versions and size fields the payload can omit."
weight = 7
+++

A Simple API version number is a promise. When a page advertises [PEP 700](https://peps.python.org/pep-0700/)'s `1.1` or
higher, it tells the client that a top-level `versions` array and a per-file `size` are present, and a client written
for that version reads them without checking. peryx used to stamp `1.4` on every page it served, including pages
re-served from an upstream that promised neither field. This page explains why that was wrong, and why peryx now derives
the version from what the upstream provides.

## The promise a version makes

The Simple API is versioned so a client can tell what a page is allowed to contain. PEP 700 raised the minimum to `1.1`
and made two fields mandatory: `versions`, the list of every release of the project, and `size`, the byte count on every
file. From `1.1` on, a client may treat both as always present. That is the whole point of the version bump: it lets a
resolver read `size` to plan a download, or read `versions` to enumerate releases, without a guard around each access.

## What over-advertising breaks

An upstream that speaks PEP 691 `1.0`, or a plain PEP 503 HTML index that declares no version at all, promises neither
field. Its pages can, and do, omit `size` on a file and carry no `versions` array. Re-serving such a page under a `1.4`
label hands the client a document that contradicts its own header.

A PEP 700-aware client trusts the label. It reads `file["size"]` to size a progress bar or a disk-space check, and
`page["versions"]` to list the releases, because `1.4` told it they are there. When they are not, the lookup fails: a
missing key raises, a total-bytes sum is wrong, a release enumeration comes back empty. The failure lands in the client,
far from peryx, and looks like a malformed index rather than an overstated version. The bytes were fine for what they
were; the label claimed more than the bytes carried.

## Derive, do not assert

peryx now advertises the version the payload satisfies. An upstream that declares `1.1` or higher promises PEP 700's
fields, peryx passes them through, and it keeps its `1.4` ceiling. An upstream at `1.0`, or one that declares no
version, promises neither, so peryx serves `1.0`, and a client reads that page knowing `size` and `versions` may be
absent. The number now matches the guarantees of the bytes underneath it.

The alternative, always satisfying `1.4` by synthesizing the missing fields, was a heavier contract than a cache should
sign. Deriving `size` for every file means knowing every file's length, which a cold cache does not; deriving `versions`
means the merged list is authoritative even when a layer was skipped. Lowering the version instead keeps peryx honest
without making it pretend to know more than it does.

## The weakest layer wins

A virtual index inherits the lowest version of its layers. One pre-PEP 700 layer caps the merged page at `1.0`, because
a merged page can only guarantee a field that every contributing layer guarantees. If even one layer can serve a file
without a `size`, the merged page cannot promise `size` for all files, so it must not claim `1.1+`. The rule is the same
correctness principle applied to a stack: advertise the guarantees the whole payload meets, which is the intersection of
what the layers meet, not the maximum.

## The principle

Advertise only what the payload provides. A version number a cache serves is a claim about the bytes it is serving right
now, not about the protocol the cache happens to implement. peryx implements `1.4`, but it serves `1.4` only where the
page it hands back carries `1.4`'s guarantees; everywhere else it serves the honest lower number and lets the client
plan accordingly.

## In practice

- The exact mapping and its edges: [the advertised Simple API version](@/ecosystems/pypi/reference/api-version.md)
- See it happen across two upstreams:
  [watch the advertised version follow the upstream](@/ecosystems/pypi/tutorials/api-version.md)
- Work out why a mirror reports `1.0`:
  [diagnose a mirror that reports api-version 1.0](@/ecosystems/pypi/guides/api-version.md)
