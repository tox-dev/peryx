+++
title = "Build a team index"
description = "Design a topology in TOML: separate team indexes, one shared cache, and private packages that shadow PyPI."
weight = 2
+++

In this tutorial you will replace the default configuration with a topology of your own: a shared pypi.org cached index,
two team indexes with their own uploads, and one route where a private package shadows its public namesake. You will see
exactly what each index role contributes. It takes about fifteen minutes and builds on
[getting started](@/core/getting-started.md).

## The goal

Two teams, `data` and `web`, each publish private packages. Both install through one cache. A package published by
either team must never be fetched from pypi.org, even if someone registers the same name there.

## Write the topology

Save this as `velodex.toml`:

```toml
data_dir = "velodex-data"

[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "data-hosted"
hosted = true
upload_token = "data-secret"

[[index]]
name = "web-hosted"
hosted = true
upload_token = "web-secret"

[[index]]
name = "data"
layers = ["data-hosted", "pypi"]

[[index]]
name = "web"
layers = ["web-hosted", "pypi"]
```

Read it bottom-up: `data` and `web` are virtual indexes, each serving its team's hosted index first and the shared
cached index second. The cached index appears once, so both teams share one cache; the hosted indexes are separate, so
teams cannot overwrite each other's uploads.

The virtual index names also become their routes because this example does not set `route`. Use simple URL-safe names
here: letters, digits, `-`, `.`, `_`, and `~` are accepted, and `/` creates nested routes such as `team/data`.

Start it:

```shell
velodex serve --config velodex.toml
```

The dashboard at `http://127.0.0.1:4433/` draws the topology you just wrote: two virtual-index cards, `data` and `web`,
each showing its layer stack in resolution order, with the shared `pypi` cached index appearing inside both and the
upload target marked. The building-block indexes have no cards of their own; they live inside the virtual indexes that
serve them.

## Install through a team route

```shell
uv venv demo
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/data/simple/ httpx
```

The cached layer fetched httpx from pypi.org, cached it, and will serve the `web` route from the same copy: install
httpx again through `http://127.0.0.1:4433/web/simple/` and watch it come from disk.

## Publish a private package

Build any small package (or reuse the one from [getting started](@/core/getting-started.md)) and upload it to the `data`
route:

```shell
twine upload --repository-url http://127.0.0.1:4433/data/ -u __token__ -p data-secret dist/*
```

The upload landed in `data-hosted` because that virtual index lists it as its first hosted layer. The `web` route cannot
see it; compare what the two routes serve for the name (the `data` route lists your file, the `web` route either knows
nothing or serves only what pypi.org happens to have under that name):

```shell
curl -s http://127.0.0.1:4433/data/simple/mypkg/ | grep -c "mypkg-1.0.0"   # 1: your upload
curl -s http://127.0.0.1:4433/web/simple/mypkg/ | grep -c "mypkg-1.0.0"    # 0, or 404
```

## Watch shadowing defend the name

Your package's name now resolves only to your upload on the `data` route. Prove it: ask for a project that exists both
locally and upstream, and check where the files come from.

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/data/simple/mypkg/ | python3 -m json.tool | grep url
```

Every URL points back at velodex, and the versions listed are yours alone. If someone registers `mypkg` on pypi.org
tomorrow with version `99.0`, nothing changes: the hosted layer answers first, and the cached index is never consulted
for a name the hosted layer has. [The index model](@/core/indexes.md) explains why this ordering is the
dependency-confusion defense.

## Where next

- Nest virtual indexes and route several upstreams:
  [compose virtual indexes](@/ecosystems/pypi/guides/compose-overlays.md)
- Add an upstream that needs credentials: [proxy a private upstream](@/ecosystems/pypi/guides/private-mirror.md)
- See what each team is installing: [monitoring](@/core/monitor.md)
