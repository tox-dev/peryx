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
serves under whichever WSGI server is importable ([waitress](https://docs.pylonsproject.org/projects/waitress/) if
installed, otherwise the single-threaded stdlib server), and the project advertises that it is looking for new
maintainers.

## Comparison against peryx

### Overlap

- **Hosting your own packages** over the [PEP 503](https://peps.python.org/pep-0503/) simple API.
- **[twine](https://twine.readthedocs.io/) uploads** as the write path, authenticated against a credential file.
- **sha256 in file links** so installers verify what they download.

### pypiserver-only behavior

- **htpasswd authentication.** pypiserver gates actions against an htpasswd file. peryx uses per-index access tokens
  with `read`, `write`, and `delete` grants instead.
- **A live package directory.** You can drop files into pypiserver's directory and it lists them. peryx validates files
  through the upload API or the explicit `peryx import-dir` command.

### peryx-only behavior

- **A real cached index.** pypiserver's fallback is a `302` redirect to pypi.org; the file never enters its directory,
  so every machine still needs pypi.org access and every miss pays full upstream latency. peryx's cached layer serves
  misses through itself and keeps them: one egress point,
  [cold installs at upstream speed](@/core/operations/performance.md), and a content-addressed store that dedupes.
- **Outage resilience.** An upstream outage takes pypiserver's fallback installs down with it. peryx serves the last
  good page while the upstream is unreachable, so a pypi.org blip degrades to stale-but-working.
- **Shadowing.** Your uploads [shadow upstream names](@/core/repositories/indexes.md) instead of coexisting with a
  redirect.
- **[PEP 658](https://peps.python.org/pep-0658/) metadata.** pypiserver serves none; peryx serves it by default.

### Performance vs peryx

The [benchmark suite](@/core/operations/performance.md) runs both from their published packages. In the install rows,
pypiserver's near-zero server CPU and flat cold-versus-warm columns are the redirect showing through: it does no work on
a miss because it caches nothing.

{{<bench file="install-uv" only="peryx,pypiserver" owner="pypi" />}}

{{<bench file="load" only="peryx,pypiserver" owner="pypi" />}}

## Migration procedure

Your package directory does not drop in: re-upload it once with twine, and peryx derives hashes and metadata
server-side. Map the flags across:

| pypiserver                                           | peryx                                                         |
| ---------------------------------------------------- | ------------------------------------------------------------- |
| `pypi-server run -p 8080 ~/packages`                 | `peryx serve`                                                 |
| `http://host:8080/simple/`                           | `http://host:4433/{route}/simple/`                            |
| `-P htpasswd.txt -a update`                          | a write-granting `[[index.access_token]]` on the hosted index |
| `--fallback-url https://pypi.org/simple/` (redirect) | a cached layer under the virtual index (served and cached)    |
| `--disable-fallback`                                 | a hosted-only index, no cached layer                          |
| `twine upload -r local dist/*`                       | the same command, pointed at the virtual route                |

Re-upload the directory in one pass:

```shell
for f in packages/*; do twine upload --repository-url http://host:4433/{route}/ "$f"; done
```

## Gotchas

- **Auth files do not migrate.** Replace htpasswd entries with scoped access tokens. Set `anonymous_read = false` when
  reads require a credential.
- **Directory changes are not watched.** Run `peryx import-dir` after adding files, or publish through twine or uv.
- **Clients stop talking to pypi.org.** Under pypiserver's redirect every client still reached pypi.org directly; behind
  peryx they do not, which is the point, but check that nothing downstream assumed direct upstream access.
