+++
title = "Capability matrix"
description = "Which roles and cross-cutting features peryx supports per ecosystem, plus what each ecosystem implements of its own protocol."
weight = 5
+++

peryx is a `(role × ecosystem)` server: every index is one of three [roles](@/core/glossary.md#roles) paired with an
[ecosystem](@/core/glossary.md#ecosystem). The matrix below covers the roles and cross-cutting features shared by every
ecosystem; a per-ecosystem list follows for the protocol features each one implements on its own.

`✓` means shipping today; **planned** means the architecture reserves it but no driver is built yet.

## Roles per ecosystem

Every ecosystem supports all three roles.

| Role                                         | PyPI | OCI |
| -------------------------------------------- | ---- | --- |
| [cached](@/core/glossary.md#roles) (proxy)   | ✓    | ✓   |
| [hosted](@/core/glossary.md#roles) (uploads) | ✓    | ✓   |
| [virtual](@/core/glossary.md#roles) (layers) | ✓    | ✓   |

## Cross-cutting features per ecosystem

These features are ecosystem-neutral: the same subsystem serves every ecosystem, so the matrix tracks where a driver is
wired in rather than whether the feature exists.

| Feature                                                     | PyPI | OCI |
| ----------------------------------------------------------- | ---- | --- |
| Read-through caching with streaming                         | ✓    | ✓   |
| Content-addressed artifact store                            | ✓    | ✓   |
| [Shadowing](@/core/glossary.md#shadowing) / virtual resolve | ✓    | ✓   |
| Publish / upload API                                        | ✓    | ✓   |
| Yank or delete                                              | ✓    | ✓   |
| Range / partial artifact reads                              | ✓    | ✓   |
| Single-flight upstream fetch                                | ✓    | ✓   |
| Usage metrics (pages, downloads, uploads)                   | ✓    | ✓   |
| `peryx mirror` sync + offline                               | ✓    | ✓   |
| Policy: name allow/deny + size limits                       | ✓    | ✓   |
| Signed webhooks                                             | ✓    | ✓   |
| Search (find packages and images)                           | ✓    | ✓   |
| Web UI browse (projects/repositories, versions/tags)        | ✓    | ✓   |
| Web UI archive/layer content inspection                     | ✓    | ✓   |

Cross-cutting subsystems that carry no per-ecosystem status (metrics transport, rate limits, logging, backup/restore,
[TLS](@/core/configuration.md#tls)) are ecosystem-neutral and apply to every index whatever its role or ecosystem.

## What each ecosystem implements

The shared matrix stops at the features every ecosystem shares. Each ecosystem also implements its own wire protocol,
and those protocol features have no cross-ecosystem counterpart, so they live here rather than as `n/a` rows above. For
the full protocol map, see each ecosystem's [standards](@/core/standards.md) page.

### PyPI

- PEP 691 JSON and PEP 503 HTML Simple index, negotiated and canonicalized in both directions
- PEP 658/714 `.metadata` fast path: advertised, fetched, back-filled from wheels by byte range, and cached
- PEP 592 yank markers and PEP 700 `versions`/`size`/`upload-time` fields
- Wheel and sdist filename, `.dist-info`, and `PKG-INFO` validation on upload
- Wheel and sdist archive inspection in the web UI
- Policy rules for version specifiers (PEP 440), package types, and wheel tags, on top of the neutral name and size
  rules
- Legacy JSON API and the multipart legacy upload API

### OCI

- Distribution-spec `/v2/` pull and push, with byte-exact manifests addressed by their own digest
- Bearer-token pull-through: peryx runs the `401` + `WWW-Authenticate: Bearer` handshake against upstreams
- Referrers API and the `OCI-Subject` header for attestations and signatures
- Chunked and monolithic blob uploads, plus cross-repo blob mount by digest
- Tag listing with `n`/`last` pagination and a `Link` next-page header
- Single-flight blob fetch so concurrent pulls of one cached layer share one upstream transfer
- Layer tar content inspection in the web UI, listing a layer's files and previewing text members

## Related

- What the roles mean: [the index model](@/core/indexes.md)
- Per-ecosystem setup: [ecosystems](@/ecosystems/_index.md)
- The terms in one place: [glossary](@/core/glossary.md)
