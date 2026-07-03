+++
title = "Compose overlays"
description = "Serve several indexes under one URL, give each mirror its own private layer, and chain overlays."
weight = 4
+++

An overlay lists other indexes as `layers` and serves them under one route. Resolution is first-match per filename:
velodex walks the layers in order and keeps the first occurrence of each file, so a file in an earlier layer shadows
the same filename in a later one. Versions union across layers.

## A private layer over each mirror

```toml
[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"

[[index]]
name = "corp"
mirror = "https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
token = "<access-token>"

[[index]]
name = "team-local"
upload_token = "<secret>"

[[index]]
name = "team"
route = "team/dev"
layers = ["team-local", "corp"]
upload = "team-local"

[[index]]
name = "oss"
layers = ["team-local", "pypi"]
```

Clients using `/team/dev/simple/` see the team's uploads in front of the corporate mirror; clients using
`/oss/simple/` see the same uploads in front of pypi.org. One local store can back any number of overlays.

## Chaining

A layer can itself be an overlay, so inheritance chains work:

```toml
[[index]]
name = "staging"
layers = ["staging-local", "team"]
upload = "staging-local"
```

`staging` resolves through `staging-local`, then `team-local`, then `corp`.

## Where uploads land

`upload` names the local layer that receives POSTs to the overlay's route. Omit it and velodex picks the overlay's
first local layer; an overlay of only mirrors rejects uploads with `405`.

## Failure behavior

A layer that cannot answer (a down mirror with a cold cache) is skipped with a warning rather than failing the whole
page, so your local packages stay installable during an upstream outage. A mirror with a warm cache serves its cached
copy instead.


## Related

- The semantics behind layering and shadowing: [the index model](@/explanation/indexes.md)
- Every `[[index]]` key: [configuration](@/reference/configuration.md)
- Publish into the overlay you built: [publish](@/guides/publish.md)
