+++
title = "Block revoked PyPI content"
description = "How digest revocations govern PyPI discovery and downloads."
weight = 7
+++

An active [digest revocation](@/core/repositories/digest-revocations.md) blocks the matching PyPI distribution through
every index role. Peryx keeps the artifact and its metadata records as incident evidence. The decision is server-wide,
so another index route or cached source cannot make the same SHA-256 readable.

The revocation API and CLI accept the canonical key `sha256:<64 lowercase hex>`. JSON records represent that digest as
`{"sha256":"<hex>"}`. PyPI file routes resolve their stored SHA-256 to this key before checking the decision.

## Discovery and download behavior

Peryx removes the distribution from Simple HTML, Simple JSON, legacy JSON, and the package inspection page. This is
separate from [yanking](https://packaging.python.org/en/latest/specifications/file-yanking/): a yanked file remains
visible for exact pins, while a revoked file is not offered to an installer.

| Request                                               | Active revocation response |
| ----------------------------------------------------- | -------------------------- |
| Project discovery and package views                   | Omits the file             |
| Repository search                                     | Omits the file             |
| Artifact `GET`, `HEAD`, conditional request, or range | `404 Not Found`            |
| PEP 658 `.metadata` or PEP 740 `.provenance` sibling  | `404 Not Found`            |
| Archive member listing or preview under `inspect/`    | `404 Not Found`            |

The `404` does not expose the administrator's reason. A metadata-store failure prevents discovery from advertising
candidate files and prevents a direct route from returning bytes. Peryx resolves a file URL to its SHA-256 before it
checks local storage or opens an upstream connection. It does not scan projects or aliases on a download.

A search record describes only the files that survive the decision, so a revoked file contributes neither its filename
nor its local bytes to a match, and a project whose every file is revoked returns no record at all. Creating or lifting
a revocation retires the search view, so the next query re-derives it without an operator running `peryx job reindex`.
The project's declared version list is unchanged, matching the Simple page.

Lifting a revocation removes only this decision. A yanked file stays yanked, a trashed file stays absent, repository
policy still applies, and a missing blob stays missing.

## Cache bound

Successful PyPI read responses carry `Cache-Control: public, max-age=60, must-revalidate, no-transform`. A request with
an `Authorization` header uses `private` in place of `public`. Errors and canonical-URL redirects carry
`Cache-Control: no-store`.

A create or lift operation invalidates the origin's digest-decision cache before it returns. While any revocation is
active, Peryx bypasses cached Simple pages and filters the current project model before serialization. A filtered page
is not added to the rendered-page cache, so lifting the decision cannot leave a stale omission behind.

A compliant client or reverse proxy may retain a successful response carrying these directives for at most 60 seconds.
Peryx cannot purge a cache that strips them, ignores `private`, or already copied the bytes outside HTTP caching. A
request whose body started before the revocation may also finish; the origin denies later requests.

## Incident steps

Create the revocation through the live Peryx API or CLI so the serving process invalidates its exact digest decision.
Purge the artifact URL, its `.metadata` and `.provenance` siblings, affected project pages in every representation, and
archive-inspection URLs from each reverse proxy when the incident needs a bound below 60 seconds. Purge slashless Simple
URLs too if an older deployment cached their redirects without `no-store`.

Purge reverse-proxy entries once when upgrading from a release that did not emit the 60-second policy. Older artifact
responses used a year-long immutable lifetime, which the new origin cannot shorten after a proxy has stored them.

Verify the digest through each configured index route, then inspect both Simple representations, legacy JSON, and the
package page. Purge copies managed by upstreams or clients; Peryx cannot revoke a URL that bypasses Peryx. A digest
revocation cannot match files without a SHA-256, and Peryx does not rewrite them onto its content-addressed route.
