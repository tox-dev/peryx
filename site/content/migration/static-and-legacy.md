+++
title = "From static and legacy setups"
description = "dumb-pypi, --find-links directories, pip2pi, nginx caches, and the other duct tape, mapped to one server."
weight = 10

[extra]
logos = ["logos/pypi.svg"]
+++

A family of small solutions covers pieces of the job. Each is fine at its piece; the migrations below are for
when the pieces stop composing.

## dumb-pypi

[dumb-pypi](https://github.com/chriskuehl/dumb-pypi) generates a static PEP 503 index from a list of filenames;
the files live wherever a URL can reach (S3, nginx), and having no server is the design. Publishing means
regenerating the site; PyPI itself still needs `--extra-index-url`; name normalization is your web server's
problem. In velodex, uploads are the write path (`twine upload`, index updated transactionally) and the same
process mirrors PyPI, so clients keep one `index-url`. Migration is a one-time twine loop over the bucket's
files.

## Plain directories and `python -m http.server`

pip's [`--find-links`](https://pip.pypa.io/en/stable/cli/pip_install/) accepts a directory listing, and
`--index-url` accepts any PEP 503-shaped tree, so a shared folder works with zero tooling. It also serves whole
wheels to every resolve (no PEP 658 metadata), lists everything in one flat page, offers no fallback to PyPI,
and authenticates nobody. It remains the right tool for a laptop and a handful of wheels.

## pip2pi

[pip2pi](https://github.com/wolever/pip2pi) snapshots a requirements set into a static index; its last release
and commit are from August 2021. The workflow it automated (resolve on a connected side, carry the result) is
the [air-gap guide](@/guides/air-gapped.md) with a cache instead of a script: the carried data directory keeps
hashes, metadata, and the JSON API.

## nginx caching proxies

[nginx_pypi_cache](https://github.com/hauntsaninja/nginx_pypi_cache) is an nginx config that reverse-proxies
pypi.org and files.pythonhosted.org with `proxy_cache`: the read-through half of the job in one file, with the
upstreams hard-coded and no uploads or private packages. velodex adds the index-aware half: digest verification,
`Cache-Control`-driven refresh with a background sweep, private hosting, and [usage counters](@/guides/monitor.md).
(Flask-Pypi-Proxy, sometimes cited alongside it, last released in 2014 and predates every modern index PEP.)

## The renames

| Setup | velodex |
| ----- | ------- |
| dumb-pypi regenerate + S3 | `twine upload` to a local index |
| `--find-links /shared/wheels` | `--index-url http://host:4433/{route}/simple/` |
| `pip2pi` snapshot + rsync | warm the cache, carry `data_dir` |
| nginx `proxy_cache` config | a mirror index |
