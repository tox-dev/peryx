+++
title = "Capability matrix"
description = "Roles and shared features implemented by the PyPI and OCI crates."
weight = 5
+++

## Roles

| Role                                | PyPI | OCI |
| ----------------------------------- | ---- | --- |
| [Cached](@/core/glossary.md#roles)  | Yes  | Yes |
| [Hosted](@/core/glossary.md#roles)  | Yes  | Yes |
| [Virtual](@/core/glossary.md#roles) | Yes  | Yes |

## Shared features

| Feature                        | PyPI | OCI |
| ------------------------------ | ---- | --- |
| Streaming read-through cache   | Yes  | Yes |
| Content-addressed blob storage | Yes  | Yes |
| Virtual-index shadowing        | Yes  | Yes |
| Publish API                    | Yes  | Yes |
| Delete or withdraw             | Yes  | Yes |
| Partial reads                  | Yes  | Yes |
| Single-flight upstream fetch   | Yes  | Yes |
| Usage metrics                  | Yes  | Yes |
| Offline mirror                 | Yes  | Yes |
| Name and size policy           | Yes  | Yes |
| Signed webhooks                | Yes  | Yes |
| Search                         | Yes  | Yes |
| Web browse                     | Yes  | Yes |
| Archive or layer inspection    | Yes  | Yes |

Metrics transport, rate limiting, and logging apply to each index. Backup, restore, and TLS also apply.

## Protocol features

PyPI implements Simple API JSON and HTML, metadata sidecars, yank state, and upload metadata validation. It also
implements archive inspection, version and wheel-tag policy, legacy JSON, and multipart uploads. See
[PyPI standards](@/ecosystems/pypi/reference/standards.md).

OCI implements distribution pull and push, bearer-token authentication, referrers, and chunked uploads. It also
implements blob mounts, tag pagination, and layer inspection. See
[OCI standards](@/ecosystems/oci/reference/standards.md).
