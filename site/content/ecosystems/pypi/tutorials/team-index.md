+++
title = "Build a team index"
description = "Design separate team indexes with one shared cache and project-level source isolation."
weight = 2
+++

Replace the default configuration with a shared pypi.org cached index and two team indexes with their own uploads. Each
team route excludes cached candidates for projects in its hosted layer. The tutorial takes about fifteen minutes and
builds on [getting started](@/core/start/getting-started.md).

## Target configuration

Two teams, `data` and `web`, each publish private packages. Both install through one cache. A distribution from pypi.org
must not appear on a team route after that team publishes the same project name.

## Write the topology

Save this as `peryx.toml`:

```toml
data_dir = "peryx-data"

[[index]]
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
ecosystem = "pypi"
name = "data-hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "data-secret"
actions = ["write", "delete"]

[[index]]
ecosystem = "pypi"
name = "web-hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "web-secret"
actions = ["write", "delete"]

[[index]]
ecosystem = "pypi"
name = "data"
layers = ["data-hosted", "pypi"]

[index.policy]
fallback_mode = "private-first"

[[index]]
ecosystem = "pypi"
name = "web"
layers = ["web-hosted", "pypi"]

[index.policy]
fallback_mode = "private-first"
```

Read it bottom-up: `data` and `web` are virtual indexes, each serving its team's hosted index first and the shared
cached index second. The cached index appears once, so both teams share one cache; the hosted indexes are separate, so
teams cannot overwrite each other's uploads.

The virtual index names also become their routes because this example does not set `route`. Use simple URL-safe names
here: letters, digits, `-`, `.`, `_`, and `~` are accepted, and `/` creates nested routes such as `team/data`.

Start it:

```shell
peryx serve --config peryx.toml
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

The cached layer fetched httpx from pypi.org and serves the `web` route from the same copy. Install httpx again through
`http://127.0.0.1:4433/web/simple/`; the request uses cached bytes.

## Publish a private package

Build any small package (or reuse the one from [getting started](@/core/start/getting-started.md)). Inject `data-secret`
as `TWINE_PASSWORD` with `TWINE_USERNAME=__token__` through your secret environment, then upload it to the `data` route:

```shell
twine upload --repository-url http://127.0.0.1:4433/data/ dist/*
```

The upload landed in `data-hosted` because that virtual index lists it as its first hosted layer. The `web` route cannot
see it; compare what the two routes serve for the name (the `data` route lists your file, the `web` route either knows
nothing or serves only what pypi.org happens to have under that name):

```shell
curl -s http://127.0.0.1:4433/data/simple/mypkg/ | grep -c "mypkg-1.0.0"   # 1: your upload
curl -s http://127.0.0.1:4433/web/simple/mypkg/ | grep -c "mypkg-1.0.0"    # 0, or 404
```

## Verify project isolation

Your package's name now resolves only to your upload on the `data` route. Prove it: ask for a project that exists both
locally and upstream, and check where the files come from.

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/data/simple/mypkg/ | python3 -m json.tool | grep url
```

Every URL points back at peryx, and the versions listed are yours alone. If someone registers `mypkg` version `99.0` on
pypi.org tomorrow, `private-first` keeps that cached candidate out of the response because the hosted layer contains the
project.

Without `private-first`, the default `fallback` mode merges distinct filenames from both layers. A hosted `mypkg-1.0.0`
and upstream `mypkg-99.0` would both appear, and an installer could select `99.0`. Filename precedence only resolves a
collision when two layers supply the same filename.
[Policy settings](@/ecosystems/pypi/reference/policy.md#project-isolation) explains the three fallback modes and
protected names.

## Next steps

- Nest virtual indexes and route several upstreams:
  [compose virtual indexes](@/ecosystems/pypi/guides/compose-overlays.md)
- Add an upstream that needs credentials: [proxy a private upstream](@/ecosystems/pypi/guides/private-mirror.md)
- See what each team is installing: [monitoring](@/core/operations/monitor.md)
