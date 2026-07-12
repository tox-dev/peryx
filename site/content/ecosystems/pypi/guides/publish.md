+++
title = "Publish packages"
description = "Upload distributions with twine or uv publish, authenticated by a shared token, including wheels from older tooling and clients that declare a single digest."
weight = 5
aliases = [
    "/ecosystems/pypi/guides/legacy-wheel/",
    "/ecosystems/pypi/guides/md5-upload/",
]
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
digests while streaming the artifact into a staged blob. When `md5_digest` is the only digest a client declares, peryx
computes and verifies it too, the way [Warehouse](https://pypi.org/) does, so a legacy MD5-only upload is accepted; see
[upload with a single digest](#upload-with-a-single-digest).

Before publishing the staged blob, peryx validates the project name, [PEP 440](https://peps.python.org/pep-0440/)
version, safe filename shape, `filetype`, archive readability, and metadata identity. Wheel uploads must contain one
`{name}-{version}.dist-info/` directory that
[matches the filename by normalized name and version](@/ecosystems/pypi/reference/dist-info.md), with `METADATA`,
`WHEEL`, and `RECORD`. The `WHEEL` tags and optional build field must match the filename, and `RECORD` must cover each
archive file except `RECORD` and deprecated RECORD signatures with sha256-or-better hashes. When `RECORD` includes a
size, the size must match the archive member.

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

## Publish a wheel from older tooling

You have a wheel built by older tooling, or restored from a backup, whose `.dist-info` directory is not spelled the
normalized way current build backends write it, say `Flask-0.12.dist-info` for a `flask-0.12` filename, or a version
written `1.0.0` where the filename says `1.0`. peryx accepts it, the same way pip and pypi.org do.

Nothing special is required. Upload the wheel as you would any other:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ \
    -u __token__ -p <secret> dist/Flask-0.12-py2.py3-none-any.whl
```

peryx reads the `.dist-info` directory from the archive, splits its stem into name and version at the last hyphen, and
compares them to the filename by [PEP 503](https://peps.python.org/pep-0503/) name normalization and
[PEP 440](https://peps.python.org/pep-0440/) version equality. An un-normalized but equivalent directory passes:

- `Flask-0.12.dist-info` for `Flask-0.12-py2.py3-none-any.whl`: `Flask` and `flask` normalize the same.
- `Foo.Bar-1.0.dist-info` for `foo_bar-1.0-py3-none-any.whl`: `Foo.Bar` and `foo_bar` both normalize to `foo-bar`.
- `pkg-1.0.0.dist-info` for `pkg-1.0-py3-none-any.whl`: `1.0` and `1.0.0` are equal under PEP 440.

### Check the directory before you upload

If you want to know what peryx will compare, read the directory name out of the archive:

```shell
unzip -l dist/your_pkg-1.0-py3-none-any.whl | grep dist-info
```

Normalize the name in your head (lowercase, and fold every run of `-`, `_`, or `.` to one `-`), then confirm the version
parses to the filename's version. If both agree, the upload will pass regardless of the directory's casing or
separators.

### When a legacy wheel is rejected

A `400` with `invalid wheel: .dist-info directory <dir> does not match expected <expected>` means the directory names a
genuinely different release, not merely a different spelling. peryx builds `<expected>` from the filename, so the
message shows both:

- **Different project.** `other-1.0.dist-info` in a `flask-1.0` wheel. The wheel was mislabeled or repackaged wrong;
  rebuild it or rename the file to match its contents.
- **Different version.** `flask-2.0.dist-info` in a `flask-1.0` wheel. The filename and the metadata disagree on the
  version; fix whichever is wrong.
- **No version segment.** `flask.dist-info`, with no hyphen to split, has no version to compare. The archive is
  malformed; rebuild it.

peryx also rejects an archive with no `.dist-info` directory (`missing .dist-info directory`) or more than one
(`multiple .dist-info directories found: ...`). These are structural faults in the wheel, not spelling differences, so
normalization does not change the outcome. Repacking a wheel by hand is the usual cause; rebuild it with a real backend
instead.

## Upload with a single digest

You have an upload path that declares a single content digest rather than the full SHA-256, BLAKE2, and MD5 that twine
sends, often a legacy tool or a CI script that computes only `md5_digest`. peryx accepts it, the same way pypi.org does,
as long as the digest matches the bytes.

The upload form needs the file in a `content` part, the project `name`, `version`, and `filetype`, and whichever digest
your client computes. Declare only that digest and leave the others off. With `curl`:

```shell
curl -sS -u __token__:<secret> https://peryx.example/root/pypi/ \
    -F ":action=file_upload" \
    -F "name=<project>" \
    -F "version=<version>" \
    -F "filetype=bdist_wheel" \
    -F "md5_digest=<md5-hex>" \
    -F "content=@dist/<project>-<version>-py3-none-any.whl"
```

Swap `md5_digest` for `sha256_digest` or `blake2_256_digest` if that is the one your client produces; any single field
is enough. peryx verifies whichever you declared against the content it staged and stores the file on a `200`. Declaring
no digest at all is also accepted, because peryx computes the SHA-256 it addresses the file by regardless.

### Compute the digest your client sends

If your uploader lets you set the digest, compute it over the exact bytes you send. For MD5:

```shell
python3 -c "import hashlib,sys;print(hashlib.md5(open(sys.argv[1],'rb').read()).hexdigest())" \
    dist/<project>-<version>-py3-none-any.whl
```

Use `hashlib.sha256` or `hashlib.blake2b(..., digest_size=32)` for the other two. The value must be lowercase hex of the
field's length: 32 characters for MD5, 64 for SHA-256 and BLAKE2b-256.

### When only MD5 is declared

peryx computes MD5 over the staged content only when `md5_digest` is the sole digest on the form. If your client also
sends `sha256_digest` or `blake2_256_digest`, peryx verifies the stronger one and leaves the declared MD5 unchecked,
since the stronger digest already covers the same bytes. Either way the upload succeeds when the digest peryx verifies
matches. You do not need to strip the extra fields to get an MD5-only upload accepted; you need them only if MD5 is all
your client can produce.

### Read a digest rejection

A digest that does not match the content is a `400` naming the field that disagreed:

- `md5_digest mismatch`, `sha256_digest mismatch`, or `blake2_256_digest mismatch`: the declared digest did not equal
  the one peryx computed over the bytes it received. The file was corrupted in transit, or the digest was computed over
  different bytes than you uploaded. Recompute the digest over the exact file and post again.
- `<field> value "<value>" is not lowercase hex with the expected length`: the digest is malformed, uppercase, or the
  wrong length. Emit lowercase hex of the right width: 32 for MD5, 64 for SHA-256 and BLAKE2b-256.

A wrong `md5_digest` only surfaces when MD5 is the sole declared digest; when a stronger digest is present peryx checks
that one, and a bad MD5 alongside it goes unnoticed.

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
- The exact accept and reject rules, tables, and error strings: [upload rules](@/ecosystems/pypi/reference/uploads.md)
- Why peryx accepts these uploads: [what peryx accepts on upload](@/ecosystems/pypi/uploads.md)
- Walk a legacy wheel and an MD5-only upload end to end:
  [publish and manage a release](@/ecosystems/pypi/tutorials/publish-and-manage.md)
