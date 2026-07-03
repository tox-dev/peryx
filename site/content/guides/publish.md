+++
title = "Publish packages"
description = "Upload distributions with twine or uv publish, authenticated by a shared token."
weight = 5
+++

velodex accepts the [legacy upload API](https://docs.pypi.org/api/upload/), the wire protocol both [twine](https://twine.readthedocs.io/) and
[`uv publish`](https://docs.astral.sh/uv/guides/package/) speak. Uploads need a local index with an `upload_token`; the default topology's `local` index has none,
so uploads are off until you set one:

```toml
[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"

[[index]]
name = "local"
upload_token = "<secret>"

[[index]]
name = "root/pypi"
layers = ["local", "pypi"]
upload = "local"
```

Then publish to the overlay's route. velodex accepts any username; the token is the password, matching the pypi.org
`__token__` convention:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
# or
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
```

velodex verifies the declared sha256 digest against the received bytes, stores the file content-addressed, and serves
it from `/root/pypi/simple/<project>/` alongside the mirror's packages. Your file shadows an upstream file of the
same name. For wheels, velodex extracts the `METADATA` document and serves it as the PEP 658 sibling, exactly as
pypi.org does, so resolvers get the fast path for your uploads too and the web UI can show the full package page.

## In `.pypirc`

The [`.pypirc` file](https://packaging.python.org/en/latest/specifications/pypirc/) holds the repository and credentials:

```ini
[distutils]
index-servers = velodex

[velodex]
repository = http://127.0.0.1:4433/root/pypi/
username = __token__
password = <secret>
```

`twine upload -r velodex dist/*` then works without flags.


## Related

- What shadowing an upstream name buys you: [the index model](@/explanation/indexes.md)
- Undo a bad release: [yank and delete](@/guides/remove.md)
- The upload protocol itself: [HTTP endpoints](@/reference/endpoints.md)
