+++
title = "Publish packages"
description = "Upload distributions with twine or uv publish, authenticated by a shared token."
weight = 5
+++

peryx accepts the [legacy upload API](https://docs.pypi.org/api/upload/), the wire protocol both
[twine](https://twine.readthedocs.io/) and [`uv publish`](https://docs.astral.sh/uv/guides/package/) speak. Uploads need
a hosted index with an `upload_token`; the default topology's `hosted` index has none, so uploads are off until you set
one:

```toml
[[index]]
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]]
name = "hosted"
upload_token = "<secret>"

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

Then publish to the virtual index's route. peryx accepts any username; the token is the password, matching the pypi.org
`__token__` convention:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
# or
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
```

peryx accepts wheels and both source-distribution forms [PEP 527](https://peps.python.org/pep-0527/) defines, a
`.tar.gz` and a `.zip`. It rejects `.egg` and the older compressed-tar formats such as `.tar.bz2` on upload; those files
can still be mirrored if an upstream index lists them. During upload, peryx checks the declared sha256 and blake2b-256
digests while streaming the artifact into a staged blob. A lone md5 digest is rejected.

Before publishing the staged blob, peryx validates the project name, [PEP 440](https://peps.python.org/pep-0440/)
version, safe filename shape, `filetype`, archive readability, and metadata identity. Wheel uploads must contain one
normalized `{name}-{version}.dist-info/` directory with `METADATA`, `WHEEL`, and `RECORD`. The `WHEEL` tags and optional
build field must match the filename, and `RECORD` must cover each archive file except `RECORD` and deprecated RECORD
signatures with sha256-or-better hashes. When `RECORD` includes a size, the size must match the archive member.

A source distribution is a `.tar.gz` or a `.zip`, and peryx holds both to the same
[PEP 625](https://peps.python.org/pep-0625/) strictness. The filename splits its name from its version at the last `-`,
so a hyphenated project such as `python-dateutil` keeps its dashes, and the archive must contain one top-level
`{name}-{version}/` directory with `pyproject.toml` and a `PKG-INFO` whose `Metadata-Version` is at least `2.2`. peryx
rejects archive entries with absolute paths, traversal, unsafe links, special files, or device entries. For Metadata 2.4
and newer, every `License-File` header must name a file inside the sdist.

The filename, form fields, and `METADATA` or `PKG-INFO` `Name` and `Version` must agree. `Metadata-Version`,
`Requires-Python`, license fields, extras, and project URLs are compared when the upload form supplies them and the
metadata model can represent them. `Requires-Python`, when present in the form or metadata, must parse as Python version
specifiers.

Accepted files are stored content-addressed and served from `/root/pypi/simple/<project>/` alongside the cached index's
packages. Your file shadows an upstream file of the same name. For wheels, peryx extracts `METADATA`; for sdists, it
extracts the verified `PKG-INFO`. Both are served as [PEP 658/714](https://peps.python.org/pep-0658/) `.metadata`
siblings, so resolvers get the fast path for your uploads and the web UI can show the full package page.

## Publishing a `.zip` sdist

Most build backends emit a `.tar.gz` sdist, but some still produce a zip one (`python setup.py sdist --formats=zip`, or
a backend configured that way). A `.zip` uploads through the same command as any other artifact, so `dist/*` covers it
and needs no extra flag:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/example_pkg-1.0.zip
```

peryx validates the zip against the sdist rules above: the `{name}-{version}/PKG-INFO`, the `pyproject.toml`, the
`Metadata-Version` floor, and the name/version identity cross-checks all apply, and the stored file gets its `PKG-INFO`
served as a PEP 658 `.metadata` sibling like a `.tar.gz` sdist does.

peryx takes the zip form because [PEP 527](https://peps.python.org/pep-0527/) lists it as a valid source distribution,
and [Warehouse](https://pypi.org/) (pypi.org), [devpi](https://devpi.net/), and pypiserver all accept it. Refusing a
`.zip` that pypi.org would take made peryx the stricter target, so a project that published a zip sdist to PyPI could
not publish the same file to the index in front of it. Accepting it keeps peryx a drop-in for the upstream it shadows.

## In `.pypirc`

The [`.pypirc` file](https://packaging.python.org/en/latest/specifications/pypirc/) holds the repository and
credentials:

```ini
[distutils]
index-servers = peryx

[peryx]
repository = http://127.0.0.1:4433/root/pypi/
username = __token__
password = <secret>
```

`twine upload -r peryx dist/*` then works without flags.

`GET /root/pypi/+api` returns the same `.pypirc` shape when the request reaches Peryx with the public `Host` header. The
discovery document keeps the password as `<upload-token>`; replace it with the hosted index token before publishing. For
offline setup, print the same snippet from the config file:

```shell
peryx config-snippet --base-url http://127.0.0.1:4433 --index root/pypi .pypirc
```

## Upload failures

Validation failures return `400` with the field or archive check that failed. Common causes:

- The filename is not a wheel, `.tar.gz` sdist, or `.zip` sdist.
- The filename's normalized project name or version does not match the form fields.
- The archive is corrupt, lacks required wheel or sdist files, has unsafe tar entries, or has a bad `RECORD`.
- Core metadata names a different project or version.
- A Metadata 2.4+ sdist lists a `License-File` that is missing from the archive.
- A declared sha256 or blake2b-256 digest does not match the received bytes.
- The same filename was already uploaded with different bytes.

## Related

- What shadowing an upstream name buys you: [the index model](@/core/indexes.md)
- Undo a bad release: [yank and delete](@/ecosystems/pypi/guides/remove.md)
- The upload protocol itself: [HTTP endpoints](@/ecosystems/pypi/reference/endpoints.md)
