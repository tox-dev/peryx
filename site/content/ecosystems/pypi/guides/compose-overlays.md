+++
title = "Compose virtual indexes"
description = "Serve several indexes under one URL, give each cached index its own private layer, and chain virtual indexes."
weight = 4
+++

A virtual index lists other indexes as `layers` and serves them under one route. peryx walks the layers in order and
keeps the first occurrence of each filename. A file in an earlier layer shadows the same filename in a later layer;
versions form a union across layers.

## A private layer over each cached index

```toml
[[index]]
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
name = "corp"

[[index.upstream]]
name = "primary"
url = "https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
token = "<access-token>"

[[index]]
name = "team-hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "<secret>"
actions = ["write", "delete"]

[[index]]
name = "team"
route = "team/dev"
layers = ["team-hosted", "corp"]
upload = "team-hosted"

[[index]]
name = "oss"
layers = ["team-hosted", "pypi"]
```

Clients using `/team/dev/simple/` see the team's uploads in front of the corporate cached index; clients using
`/oss/simple/` see the same uploads in front of pypi.org. One hosted store can back any number of virtual indexes.

Choose stable URL prefixes for routes. Segments may contain ASCII letters, digits, `-`, `.`, `_`, and `~`; use `/` for
nested routes. Peryx validates routes at startup and rejects collisions with built-in endpoints such as `browse`,
`stats`, `+stats`, and `+status`.

## Chaining

A layer can itself be a virtual index, so inheritance chains work:

```toml
[[index]]
name = "staging"
layers = ["staging-hosted", "team"]
upload = "staging-hosted"
```

`staging` resolves through `staging-hosted`, then `team-hosted`, then `corp`.

## Where uploads land

`upload` names the hosted layer that receives POSTs to the virtual index's route. Omit it and peryx picks the virtual
index's first hosted layer; a virtual index of only cached indexes rejects uploads with `405`.

## Failure behavior

If an upstream is down and its cache is cold, peryx skips that layer with a warning. Other layers remain available. A
warm cached index serves its stored copy.

## Related

- The semantics behind layering and shadowing: [the index model](@/core/indexes.md)
- Every `[[index]]` key: [configuration](@/core/configuration.md)
- Publish into the virtual index you built: [publish](@/ecosystems/pypi/guides/publish.md)
