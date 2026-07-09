+++
title = "From pypiserver"
description = "What pypiserver and peryx share, why its fallback redirect is not a cache, what peryx adds, and how to move."
weight = 3
[extra]
logos = [ "logos/pypiserver.png"]
+++

[pypiserver](https://github.com/pypiserver/pypiserver) is a [Bottle](https://bottlepy.org/docs/dev/) app that serves a
directory of your own packages over the simple API, with htpasswd-gated uploads. Its upstream story is a redirect:
`--fallback-url` sends the client to pypi.org for anything the directory lacks, and nothing comes back into a cache. It
serves under whichever WSGI server is importable (waitress if installed, otherwise the single-threaded stdlib server),
and the project advertises that it is looking for new maintainers.

## Comparison against peryx

### Overlap

- **Hosting your own packages** over the PEP 503 simple API.
- **twine uploads** as the write path, authenticated against a credential file.
- **sha256 in file links** so installers verify what they download.

### Extra: what pypiserver does that peryx does not

- **Per-action authentication.** pypiserver's `-a download,list,update` gates reads, listings, and uploads independently
  against an htpasswd file. peryx authenticates uploads only; reads are open to the network the port lives on.
- **A hand-editable package directory.** You can drop files into pypiserver's directory and it lists them. peryx has no
  drop-in directory; uploads are the only write path.

### Missing: what peryx adds

- **A real cached index.** pypiserver's fallback is a `302` redirect to pypi.org; the file never enters its directory,
  so every machine still needs pypi.org access and every miss pays full upstream latency. peryx's cached layer serves
  misses through itself and keeps them: one egress point, [cold installs at upstream speed](@/core/performance.md), and
  a content-addressed store that dedupes.
- **Outage resilience.** An upstream outage takes pypiserver's fallback installs down with it. peryx serves the last
  good page while the upstream is unreachable, so a pypi.org blip degrades to stale-but-working.
- **Shadowing.** Your uploads [shadow upstream names](@/core/indexes.md) instead of coexisting with a redirect.
- **PEP 658 metadata.** pypiserver serves none; peryx serves it by default.

### Performance vs peryx

The [benchmark suite](@/core/performance.md) runs both from their published packages. In the install rows, pypiserver's
near-zero server CPU and flat cold-versus-warm columns are the redirect showing through: it does no work on a miss
because it caches nothing.

{{ bench(file="install-uv", only="peryx,pypiserver") }}

{{ bench(file="load", only="peryx,pypiserver") }}

## How to migrate

Your package directory does not drop in: re-upload it once with twine, and peryx derives hashes and metadata
server-side. Map the flags across:

| pypiserver                                           | peryx                                                      |
| ---------------------------------------------------- | ---------------------------------------------------------- |
| `pypi-server run -p 8080 ~/packages`                 | `peryx serve`                                              |
| `http://host:8080/simple/`                           | `http://host:4433/{route}/simple/`                         |
| `-P htpasswd.txt -a update`                          | `upload_token` on the hosted index                         |
| `--fallback-url https://pypi.org/simple/` (redirect) | a cached layer under the virtual index (served and cached) |
| `--disable-fallback`                                 | a hosted-only index, no cached layer                       |
| `twine upload -r local dist/*`                       | the same command, pointed at the virtual route             |

Re-upload the directory in one pass:

```shell
for f in packages/*; do twine upload --repository-url http://host:4433/{route}/ "$f"; done
```

## Gotchas

- **Reads are open.** pypiserver's per-action auth (`-a download,list,update`) has no counterpart; peryx authenticates
  uploads only, and reads are open to the network the port lives on. Put peryx on a trusted network or behind your own
  gateway if reads must be restricted.
- **No hand-editing the directory.** If you relied on editing files in the package directory by hand, that workflow is
  gone; uploads are the write path.
- **Clients stop talking to pypi.org.** Under pypiserver's redirect every client still reached pypi.org directly; behind
  peryx they do not, which is the point, but check that nothing downstream assumed direct upstream access.
