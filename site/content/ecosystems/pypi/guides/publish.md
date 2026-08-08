+++
title = "Publish packages"
description = "Upload distributions with twine or uv publish, authenticated by a shared token, including wheels from older tooling and clients that declare a single digest."
weight = 5
aliases = [ "/ecosystems/pypi/guides/legacy-wheel/", "/ecosystems/pypi/guides/md5-upload/"]
+++

peryx accepts the [legacy upload API](https://docs.pypi.org/api/upload/), the wire protocol both
[twine](https://twine.readthedocs.io/) and [`uv publish`](https://docs.astral.sh/uv/guides/package/) speak. Uploads need
a hosted index with a write-granting `[[index.access_token]]`; the default topology's `hosted` index has none, so
uploads are off until you add one:

```toml
[[index]]
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
name = "hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "<secret>"
actions = ["write", "delete"]

[[index]]
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

Publish to the virtual index's route. peryx accepts any username; the token is the password, matching the pypi.org
`__token__` convention:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
# or
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/*
```

peryx accepts wheels and both source-distribution forms [PEP 527](https://peps.python.org/pep-0527/) defines: a
`.tar.gz` and a `.zip`. It rejects `.egg` and the older compressed-tar formats such as `.tar.bz2` on upload; those files
can still be mirrored if an upstream index lists them. During upload, peryx checks the declared sha256 and blake2b-256
digests while streaming the artifact into a staged blob. When `md5_digest` is the only digest a client declares, peryx
computes and verifies it as [Warehouse](https://pypi.org/) does, so it accepts a legacy MD5-only upload. See
[upload with a single digest](#upload-with-a-single-digest).

Before publishing the staged blob, peryx validates the project name, [PEP 440](https://peps.python.org/pep-0440/)
version, safe filename shape, `filetype`, archive readability, and metadata identity. Wheel uploads must contain one
`{name}-{version}.dist-info/` directory that
[matches the filename by normalized name and version](@/ecosystems/pypi/reference/uploads.md#wheel-dist-info-matching),
with `METADATA`, `WHEEL`, and `RECORD`. The `WHEEL` tags and optional build field must match the filename, and `RECORD`
must cover each archive file except `RECORD` and deprecated RECORD signatures with sha256-or-better hashes. When
`RECORD` includes a size, the size must match the archive member.

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

The metadata document must also parse as a well-formed email message, the format core metadata uses. A header line
without a colon, a line with no field name, or a document opening with a folded continuation line is a defect.
`email.parser` stops reading headers at that line, and every field below it disappears. peryx rejects the upload rather
than reading past the defect, the same as pypi.org.

peryx stores accepted files by content digest and serves them from `/root/pypi/simple/<project>/` alongside the cached
index's packages. Your file shadows an upstream file of the same name. For wheels, peryx extracts `METADATA`; for
sdists, it extracts the verified `PKG-INFO`. peryx serves both as [PEP 658/714](https://peps.python.org/pep-0658/)
`.metadata` siblings, giving resolvers the metadata path and the web UI the full package data.

## Publishing a `.zip` sdist

Most build backends emit a `.tar.gz` sdist. Some produce a zip through `python setup.py sdist --formats=zip` or backend
configuration. Upload a `.zip` with the same command as other artifacts; `dist/*` needs no extra flag:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <secret> dist/example_pkg-1.0.zip
```

peryx applies the sdist rules above to the zip, including `{name}-{version}/PKG-INFO`, `pyproject.toml`, the
`Metadata-Version` floor, and name/version identity checks. It serves the stored file's `PKG-INFO` as a PEP 658
`.metadata` sibling.

peryx takes the zip form because [PEP 527](https://peps.python.org/pep-0527/) lists it as a valid source distribution,
and [Warehouse](https://pypi.org/) (pypi.org), [devpi](https://devpi.net/), and pypiserver all accept it. Refusing a
`.zip` that pypi.org would take made peryx the stricter target, so a project that published a zip sdist to PyPI could
not publish the same file to the index in front of it. Accepting it keeps peryx a drop-in for the upstream it shadows.

## Publish a wheel from older tooling

A wheel from older tooling or a backup may use a non-normalized `.dist-info` directory, such as `Flask-0.12.dist-info`
for a `flask-0.12` filename or version `1.0.0` for a filename with `1.0`. peryx accepts the same equivalent spellings as
pip and pypi.org.

Upload the wheel with the standard command:

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

Read the directory name from the archive to inspect the value that peryx compares:

```shell
unzip -l dist/your_pkg-1.0-py3-none-any.whl | grep dist-info
```

Normalize the name in your head (lowercase, and fold every run of `-`, `_`, or `.` to one `-`), then confirm the version
parses to the filename's version. If both agree, the upload will pass regardless of the directory's casing or
separators.

### When a legacy wheel is rejected

A `400` with `invalid wheel: .dist-info directory <dir> does not match expected <expected>` means the directory names a
different release. peryx builds `<expected>` from the filename, so the message shows both values:

- **Different project.** `other-1.0.dist-info` in a `flask-1.0` wheel. The wheel was mislabeled or repackaged wrong;
  rebuild it or rename the file to match its contents.
- **Different version.** `flask-2.0.dist-info` in a `flask-1.0` wheel. The filename and the metadata disagree on the
  version; fix whichever is wrong.
- **No version segment.** `flask.dist-info`, with no hyphen to split, has no version to compare. The archive is
  malformed. Rebuild it.

peryx also rejects an archive with no `.dist-info` directory (`missing .dist-info directory`) or more than one
(`multiple .dist-info directories found: ...`). These are structural faults in the wheel, not spelling differences, so
normalization does not change the outcome. Hand repacking often causes these faults. Rebuild the wheel with a package
backend.

## Upload with a single digest

Legacy tools and CI scripts may declare one content digest, such as `md5_digest`, while twine sends SHA-256, BLAKE2, and
MD5. peryx and pypi.org accept a single declared digest when it matches the uploaded bytes.

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

Use `sha256_digest` or `blake2_256_digest` when the client produces that field. peryx verifies the declared digest
against the staged content and stores the file on a `200`. It also accepts an upload without a declared digest because
it computes the SHA-256 used to address the file.

### Compute the digest your client sends

If your uploader lets you set the digest, compute it over the exact bytes you send. For MD5:

```shell
python3 -c "import hashlib,sys;print(hashlib.md5(open(sys.argv[1],'rb').read()).hexdigest())" \
    dist/<project>-<version>-py3-none-any.whl
```

Use `hashlib.sha256` or `hashlib.blake2b(..., digest_size=32)` for the other two. The value must be lowercase
hexadecimal with 32 characters for MD5 or 64 for SHA-256 and BLAKE2b-256.

### When only MD5 is declared

peryx computes MD5 over the staged content only when `md5_digest` is the sole digest on the form. If your client also
sends `sha256_digest` or `blake2_256_digest`, peryx verifies the stronger one and leaves the declared MD5 unchecked,
since the stronger digest covers the same bytes. The upload succeeds when the digest peryx verifies matches. Keep
stronger digest fields when the client provides them; use MD5 alone when that is the client's available digest.

### Read a digest rejection

A digest that does not match the content is a `400` naming the field that disagreed:

- `md5_digest mismatch`, `sha256_digest mismatch`, or `blake2_256_digest mismatch`: the declared digest did not equal
  the one peryx computed over the bytes it received. The file was corrupted in transit, or the digest was computed over
  different bytes than you uploaded. Recompute the digest over the exact file and post again.
- `<field> value "<value>" is not lowercase hex with the expected length`: the digest is malformed, uppercase, or the
  wrong length. Emit lowercase hex of the right width: 32 for MD5, 64 for SHA-256 and BLAKE2b-256.

peryx reports a wrong `md5_digest` when MD5 is the sole declared digest. With a stronger digest present, peryx checks
that digest and does not inspect MD5.

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

## Publish with attestations

peryx accepts [PEP 740](https://peps.python.org/pep-0740/) attestations attached to a hosted upload, binds each one to
the distribution, and serves it as provenance on the Simple API. The build system generates the attestations; peryx
stores and serves the signed data without holding signing material.

### From GitHub Actions

`pypa/gh-action-pypi-publish` signs each distribution with the workflow's OIDC identity and uploads the attestations
alongside the files. Point it at your peryx index and turn attestations on:

```yaml
jobs:
  publish:
    runs-on: ubuntu-latest
    permissions:
      id-token: write  # mint the OIDC token the attestation is signed with
    steps:
      - uses: actions/download-artifact@v4
        with: {name: dist, path: dist/}
      - uses: pypa/gh-action-pypi-publish@release/v1
        with:
          repository-url: https://peryx.example/root/pypi/
          attestations: true
          password: ${{ secrets.PERYX_TOKEN }}
```

### From twine

[twine](https://twine.readthedocs.io/) 6.1+ generates and uploads attestations when you pass `--attestations` (it needs
an ambient OIDC identity, so this runs in CI, not on a laptop):

```shell
twine upload --attestations -r peryx dist/*
```

### Confirm the provenance

After a successful upload, the file's Simple API entry carries a `provenance` URL; fetch it to see the bundle:

```shell
curl -s https://peryx.example/root/pypi/simple/mypkg/ \
  -H 'Accept: application/vnd.pypi.simple.v1+json' | jq '.files[].provenance'
# https://peryx.example/root/pypi/files/<sha256>/mypkg-1.0-py3-none-any.whl.provenance

curl -s https://peryx.example/root/pypi/files/<sha256>/mypkg-1.0-py3-none-any.whl.provenance | jq '.version'
# 1
```

An attestation whose subject digest or filename does not match the distribution fails the upload with `400`; peryx
publishes neither the file nor its provenance. The [upload rules](@/ecosystems/pypi/reference/uploads.md#attestations)
list each check and limit.

## Upload failures

Validation failures return `400` with the field or archive check that failed. Common causes:

- The filename is not a wheel, `.tar.gz` sdist, or `.zip` sdist.
- The filename's normalized project name or version does not match the form fields.
- The archive is corrupt, lacks required wheel or sdist files, has unsafe tar entries, or has a bad `RECORD`.
- Core metadata names a different project or version.
- A Metadata 2.4+ sdist lists a `License-File` that is missing from the archive.
- A declared sha256 or blake2b-256 digest does not match the received bytes.
- The same filename was already uploaded with different bytes.
- An attached attestation does not bind to the distribution, or the `attestations` field is malformed or oversized.

## Related

- What shadowing an upstream name buys you: [the index model](@/core/indexes.md)
- Undo a bad release: [yank and delete](@/ecosystems/pypi/guides/remove.md)
- The upload protocol itself: [HTTP endpoints](@/ecosystems/pypi/reference/endpoints.md)
- The exact accept and reject rules, tables, and error strings: [upload rules](@/ecosystems/pypi/reference/uploads.md)
- Why peryx accepts these uploads: [what peryx accepts on upload](@/ecosystems/pypi/uploads.md)
- Walk a legacy wheel and an MD5-only upload end to end:
  [publish and manage a release](@/ecosystems/pypi/tutorials/publish-and-manage.md)
