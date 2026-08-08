+++
title = "Standards"
description = "The packaging PEPs and specifications peryx implements for PyPI, and how they fit together."
weight = 1
+++

peryx targets the interoperability standards a modern Python index and its clients rely on. The
[Simple Repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) is the living
consolidation of most of them; peryx serves `meta.api-version` 1.4.

## What a pip install asks for

Knowing the request sequence makes the table below concrete. For `pip install requests` against any standards-compliant
index:

{% mermaid() %}
sequenceDiagram
participant P as pip / uv
participant I as index
P->>+I: GET /simple/requests/ (Accept: PEP 691 JSON)
I-->>-P: file list: names, URLs, sha256, yanked, core-metadata
P->>+I: GET …requests-2.32.5…whl.metadata (PEP 658)
I-->>-P: core metadata: dependencies, requires-python
Note over P: resolve, repeating metadata fetches<br/>for candidates as needed
P->>+I: GET …requests-2.32.5…whl
I-->>-P: the wheel, which pip verifies against its sha256
{% end %}

Every hop names a standard: the page format is PEP 503/691, its fields are PEP 700, the yank markers are PEP 592, the
metadata shortcut is PEP 658/714, and the filename [pip](https://pip.pypa.io/) parsed to pick a wheel is PEP 427. peryx
sits on both sides of this conversation, a server to your clients and a client to its upstreams, which is why the table
below mixes "served" and "parsed".

| Standard                                                                                                                                                                                      | Role in peryx                                     |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| [PEP 503](https://peps.python.org/pep-0503/)                                                                                                                                                  | HTML Simple API and project-name normalization    |
| [PEP 691](https://peps.python.org/pep-0691/)                                                                                                                                                  | JSON Simple API and content negotiation           |
| [PEP 629](https://peps.python.org/pep-0629/)                                                                                                                                                  | Simple API version marker                         |
| [PEP 700](https://peps.python.org/pep-0700/)                                                                                                                                                  | `versions`, `size`, and `upload-time` fields      |
| [PEP 592](https://peps.python.org/pep-0592/)                                                                                                                                                  | Yanked-file metadata                              |
| [PEP 658](https://peps.python.org/pep-0658/) and [PEP 714](https://peps.python.org/pep-0714/)                                                                                                 | `.metadata` siblings for package metadata         |
| [PEP 740](https://peps.python.org/pep-0740/)                                                                                                                                                  | Index-hosted attestations                         |
| [PEP 792](https://peps.python.org/pep-0792/)                                                                                                                                                  | Project status markers                            |
| [PEP 440](https://packaging.python.org/en/latest/specifications/version-specifiers/)                                                                                                          | Version ordering and `Requires-Python` validation |
| [PEP 427](https://packaging.python.org/en/latest/specifications/binary-distribution-format/) and [PEP 625](https://packaging.python.org/en/latest/specifications/source-distribution-format/) | Wheel and source-distribution validation          |
| [PEP 527](https://peps.python.org/pep-0527/)                                                                                                                                                  | Zip source distributions                          |
| [Core metadata](https://packaging.python.org/en/latest/specifications/core-metadata/)                                                                                                         | `METADATA`, `PKG-INFO`, and import declarations   |
| [PEP 508](https://peps.python.org/pep-0508/)                                                                                                                                                  | Dependency specifiers                             |
| [PEP 639](https://peps.python.org/pep-0639/)                                                                                                                                                  | SPDX license expressions and files                |
| [PEP 685](https://peps.python.org/pep-0685/)                                                                                                                                                  | Normalized extra names                            |
| [PEP 643](https://peps.python.org/pep-0643/)                                                                                                                                                  | Dynamic metadata fields                           |
| [PEP 753](https://peps.python.org/pep-0753/)                                                                                                                                                  | Canonical project URL labels                      |
| [Legacy JSON API](https://docs.pypi.org/api/json/)                                                                                                                                            | Project and release compatibility responses       |
| [Legacy upload API](https://docs.pypi.org/api/upload/)                                                                                                                                        | Multipart uploads used by twine and `uv publish`  |
| [`.pypirc`](https://packaging.python.org/en/latest/specifications/pypirc/)                                                                                                                    | Upload and mirror authentication conventions      |

## Metadata validation on upload

peryx parses the core metadata of a hosted upload and rejects the whole upload when a field is malformed, so a broken
`METADATA` never reaches a resolver. It checks each field with the library that owns the grammar, as it does the wire
formats.

- `Requires-Dist`, `Provides-Dist`, and `Obsoletes-Dist` must parse as PEP 508 dependency specifiers.
- `License-Expression` must be a PEP 639 SPDX expression of known, non-deprecated identifiers, and may not accompany the
  legacy `License` field.
- `Provides-Extra` names collide when they normalize equal under PEP 685.
- `Dynamic` may name only a field PEP 643 lets vary, never `Name`, `Version`, or `Metadata-Version`.
- `Classifier` values must be known, non-deprecated trove classifiers, and `Author-email`/`Maintainer-email` must be RFC
  822 address lists.
- Every field must appear at or after the `Metadata-Version` that introduced it; a 2.5-only `Import-Name` on a 2.1
  document is rejected.

The [upload reference](@/ecosystems/pypi/reference/uploads.md#what-peryx-validates) lists the accept and reject tables
with the exact error strings.

## PEP 714 and the `core-metadata` key

PEP 658 shipped with a bug in its `dist-info-metadata` key name, and PEP 714 renamed it to `core-metadata`. Indexes such
as pypi.org emit both keys for compatibility. peryx parses both spellings, prefers `core-metadata` when both are
present, and emits both spellings downstream for older clients.

## Graceful degradation

Some upstreams implement only part of the stack; [Artifactory](https://jfrog.com/artifactory/) and GitLab serve HTML
alone. peryx negotiates JSON first, parses PEP 503 HTML as the fallback, and re-serves the modern formats downstream, so
a client gets api-version 1.4. Features the upstream cannot express (a missing `.metadata` sibling, absent sizes)
degrade per file rather than per index. An upstream that advertises another Simple API major version is rejected with a
502 response; peryx supports Simple API 1.x.

The discovery documents at `/+api` and `/{route}/+api` report only capabilities peryx implements today. They advertise
Simple HTML/JSON, api-version 1.4, PEP 658 metadata siblings, project status, provenance, and legacy JSON. The legacy
JSON responses are derived from Simple detail pages, so fields outside that source, such as ownership and vulnerability
data, are empty.

## In practice

- The machinery that serves these: [architecture](@/core/architecture.md)
- The endpoints they map to: [HTTP endpoints](@/ecosystems/pypi/reference/endpoints.md)
- How PEP 427/503/440 combine to match a wheel's `.dist-info` on upload:
  [wheel .dist-info matching](@/ecosystems/pypi/reference/uploads.md#wheel-dist-info-matching)
