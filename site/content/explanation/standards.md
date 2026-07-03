+++
title = "Standards"
description = "The packaging PEPs and specifications velodex implements, and how they fit together."
weight = 4
+++

velodex targets the interoperability standards a modern index and its clients rely on. The
[Simple Repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) is the living
consolidation of most of them; velodex serves `meta.api-version` 1.4.

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
Note over P: resolve; repeat metadata fetches<br/>for candidates as needed
P->>+I: GET …requests-2.32.5…whl
I-->>-P: the wheel; pip verifies its sha256
{% end %}

Every hop names a standard: the page format is PEP 503/691, its fields are PEP 700, the yank markers are PEP 592, the
metadata shortcut is PEP 658/714, and the filename pip parsed to pick a wheel is PEP 427. velodex sits on both sides of
this conversation, a server to your clients and a client to its upstreams, which is why the table below mixes "served"
and "parsed".

| Standard | Role in velodex |
| -------- | ------------- |
| [PEP 503](https://peps.python.org/pep-0503/) | The HTML simple index and project-name normalization; served to clients that do not ask for JSON, and parsed from HTML-only upstreams |
| [PEP 691](https://peps.python.org/pep-0691/) | The JSON simple index and its content negotiation; the primary wire format both directions |
| [PEP 629](https://peps.python.org/pep-0629/) | Version marker on responses so clients can detect capabilities |
| [PEP 700](https://peps.python.org/pep-0700/) | The `versions`, `size`, and `upload-time` fields introduced in api-version 1.1 |
| [PEP 592](https://peps.python.org/pep-0592/) | Yanked files: parsed from upstreams, re-served, and settable on uploads |
| [PEP 658](https://peps.python.org/pep-0658/) / [PEP 714](https://peps.python.org/pep-0714/) | The `.metadata` sibling that lets resolvers skip wheel downloads; advertised, fetched, verified, and cached |
| [PEP 740](https://peps.python.org/pep-0740/) | Provenance URLs on Simple API file entries; preserved when an upstream provides them |
| [PEP 440](https://packaging.python.org/en/latest/specifications/version-specifiers/) | Version parsing, ordering, and `Requires-Python` validation |
| [PEP 427](https://packaging.python.org/en/latest/specifications/binary-distribution-format/) / [PEP 625](https://packaging.python.org/en/latest/specifications/source-distribution-format/) | Wheel filename, `.dist-info`, `WHEEL`, and `RECORD` checks; sdist filename handling |
| [Core metadata](https://packaging.python.org/en/latest/specifications/core-metadata/) | `METADATA` and `PKG-INFO` parsing for upload identity checks and PEP 658 siblings |
| [Legacy upload API](https://docs.pypi.org/api/upload/) | The multipart upload protocol twine and `uv publish` speak |
| [`.pypirc`](https://packaging.python.org/en/latest/specifications/pypirc/) | The `__token__` authentication convention for uploads and upstream mirrors |

## PEP 714 and the `core-metadata` key

PEP 658 shipped with a bug in its `dist-info-metadata` key name, and PEP 714 renamed it to `core-metadata`. Indexes such
as pypi.org emit both keys for compatibility. velodex parses both spellings, prefers `core-metadata` when both are
present, and emits both spellings downstream for older clients.

## Graceful degradation

Some upstreams implement only part of the stack; Artifactory and GitLab serve HTML alone. velodex negotiates JSON first,
parses PEP 503 HTML as the fallback, and re-serves the modern formats downstream, so a client gets api-version 1.4.
Features the upstream cannot express (a missing `.metadata` sibling, absent sizes) degrade per file rather than per
index. An upstream that advertises another Simple API major version is rejected with a 502 response; velodex supports
Simple API 1.x.

The discovery documents at `/+api` and `/{route}/+api` report only capabilities Velodex implements today. They
advertise Simple HTML/JSON, api-version 1.1, and PEP 658 metadata siblings. `project_status`, `provenance`, and
`legacy_json` are false until Velodex preserves Simple API 1.4 fields and serves the legacy JSON API.

## In practice

- The machinery that serves these: [architecture](@/explanation/architecture.md)
- The endpoints they map to: [HTTP endpoints](@/reference/endpoints.md)
