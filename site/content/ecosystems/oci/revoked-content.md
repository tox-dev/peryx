+++
title = "Block revoked OCI content"
description = "How OCI digest revocations affect pulls and caches."
weight = 6
+++

An active [digest revocation](@/core/repositories/digest-revocations.md) blocks the matching OCI bytes for every index
kind. Peryx keeps revoked manifests and blobs in storage as incident evidence. The serving decision is server-wide, so
another repository link or tag cannot make the same digest readable.

The revocation API and CLI accept the canonical key `sha256:<64 lowercase hex>`. JSON records represent that digest as
`{"sha256":"<hex>"}`. OCI manifest and blob routes use the same digest spelling.

## Pull and discovery behavior

Peryx resolves a tag to its manifest digest before it reads stored bytes. A cached index asks its upstream for the tag
target with `HEAD`; a revoked target stops before the manifest download. A pull by digest checks the decision before the
local store or upstream. Legacy manifest negotiation checks the selected `linux/amd64` child as a separate digest.

| Request                                        | Active revocation response                  |
| ---------------------------------------------- | ------------------------------------------- |
| Manifest by tag or digest, `GET` or `HEAD`     | `404 MANIFEST_UNKNOWN`                      |
| Blob, config, layer, or range                  | `404 BLOB_UNKNOWN`                          |
| Layer browser under `/blobs/<digest>/contents` | `404 BLOB_UNKNOWN`                          |
| Tag listing                                    | Omits tags that point to a revoked manifest |
| Referrers for a revoked subject                | `200` with an empty OCI image index         |
| Referrers containing a revoked manifest        | Omits the revoked descriptor                |

These responses do not expose the administrator's reason. A metadata failure prevents the origin from serving the
candidate digest and returns a gateway error.

Only a `404` confirms that the upstream registry no longer resolves a tag. After authentication, throttling, server, or
transport errors, Peryx uses the target it recorded within `max_stale_secs` and applies the revocation policy to that
digest. When Peryx serves a tag list from the [stale window](@/core/operations/configuration.md), it reads those
recorded targets and skips per-tag revalidation against the failed registry. If a listed tag has no target inside the
same bound, Peryx returns the upstream error instead of a shortened `200`.

Revoking a config or layer does not hide its clear parent manifest. The client can inspect the manifest, then receives
`BLOB_UNKNOWN` when it requests the revoked digest. If two repositories link the same layer, Peryx blocks both requests.
Peryx checks one indexed decision for each content request and does not walk the manifest graph.

Lifting a revocation removes this decision. Stored content becomes readable when its repository link and policy still
allow it. A lift does not restore deleted content or override an independent repository policy.

## Cache bound

Successful anonymous content and discovery responses carry
`Cache-Control: public, max-age=60, must-revalidate, no-transform`. An authenticated response uses `private` in place of
`public`, which prevents a shared proxy from storing private repository output. Errors carry `Cache-Control: no-store`.

A create or lift operation invalidates the origin's cached decision before it returns. A compliant client or reverse
proxy may retain an older successful response for at most 60 seconds. Peryx cannot purge a cache that strips these
directives or ignores `private`.

## Incident steps

Create the revocation through the live Peryx API or CLI so the serving process invalidates its decision cache. Purge the
manifest and blob digest URLs from each reverse proxy when the incident needs a bound below 60 seconds. Include tag URLs
because a client may have cached the tag response that names the manifest.

Purge reverse-proxy entries once when upgrading from a release that did not emit these cache directives. If an operator
overrides `private`, the cache must vary on `Authorization`; otherwise one caller's repository access can leak content
to another caller. Verify the blocked digest through its direct URL and each known tag, then inspect tag and referrer
discovery. Lifting the record needs no storage rewrite, but external caches may retain a prior `404` only when they
ignored `no-store`.
