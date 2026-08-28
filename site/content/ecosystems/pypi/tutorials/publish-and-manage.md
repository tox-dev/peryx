+++
title = "Publish and manage a release"
description = "Upload historical and MD5-only wheels, yank an equivalent PEP 440 version, and delete a verb-named project."
weight = 4
aliases = [ "/ecosystems/pypi/tutorials/md5-upload/", "/ecosystems/pypi/tutorials/legacy-wheel/", "/ecosystems/pypi/tutorials/version-match/", "/ecosystems/pypi/tutorials/reserved-name/"]
+++

Run four uploads that peryx once refused and now accepts against one running server. You publish a historical wheel
whose `.dist-info` directory predates PEP 503 normalization, publish another wheel declaring only a legacy `md5_digest`,
yank a release by a version spelling it was not uploaded with, and delete a project whose name is the verb `yank`. It
takes about half an hour, and shows that peryx takes the same uploads and addresses releases the same way pypi.org and
pip do.

## Prerequisites

You need a peryx binary ([installation](@/core/start/installation.md) lists the channels), Python 3 with
[pip](https://pip.pypa.io/), [build](https://build.pypa.io/), [twine](https://twine.readthedocs.io/), and
[uv](https://docs.astral.sh/uv/), and `curl`. Work in a scratch directory.

## Start peryx with an upload token

Uploads are off until a hosted index has a token. Write a config that sets one and start peryx; every part below reuses
it:

```toml
# peryx.toml
[[index]] # cached: read-through cache of pypi.org
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]] # hosted: your own uploads, gated by the token
ecosystem = "pypi"
name = "hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "demo-secret"
actions = ["write", "delete"]

[[index]] # virtual: uploads shadow upstream behind one URL
ecosystem = "pypi"
name = "root-pypi"
route = "root/pypi"
layers = ["hosted", "pypi"]
write_target = "hosted"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Leave it running and use a second terminal. peryx accepts any username on an upload;
the token is the password, matching the pypi.org `__token__` convention.

## Part 1: upload a historical wheel

[Flask 0.12](https://pypi.org/project/Flask/0.12/) shipped in 2016, before the ecosystem settled on normalized
`.dist-info` names. Download its wheel from pypi.org:

```shell
pip download Flask==0.12 --no-deps --only-binary :all: --dest dist
```

Look inside it and note the directory name:

```shell
unzip -l dist/Flask-0.12-py2.py3-none-any.whl | grep dist-info
```

The directory is `Flask-0.12.dist-info`, mixed case. The filename normalizes to `flask`, so the directory name and the
normalized filename are not byte-for-byte equal. pip installs this wheel every day; the question is whether peryx will
take it on upload. Inject `demo-secret` as `TWINE_PASSWORD` with `TWINE_USERNAME=__token__` through your secret
environment, then publish it:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ dist/Flask-0.12-py2.py3-none-any.whl
```

twine reports the upload succeeded. peryx matched `Flask-0.12.dist-info` to the `flask-0.12` filename by normalizing the
name and parsing the version, rather than demanding the exact bytes, so the wheel passed validation. Before the change
that made peryx accept these, this same upload returned a `400`.

Ask the index for the project page and find your file, then install it back through peryx to prove the round trip:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/flask/ | python3 -m json.tool | grep -A1 Flask-0.12

python -m venv check
check/bin/pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ Flask==0.12
```

The wheel you published, un-normalized `.dist-info` and all, installed straight back out of peryx. A directory that
named a different project or version would still be rejected: peryx accepts a different spelling of the right identity,
never the wrong one.

## Part 2: publish with an MD5-only client

twine always sends SHA-256, BLAKE2, and MD5 together, so to send MD5 alone you post the upload form yourself with
`curl`. Download a small pure-Python wheel and compute its MD5, the one digest you will declare:

```shell
pip download six==1.16.0 --no-deps --only-binary :all: --dest dist

MD5=$(python3 -c "import hashlib,sys;print(hashlib.md5(open(sys.argv[1],'rb').read()).hexdigest())" \
    dist/six-1.16.0-py2.py3-none-any.whl)
echo "$MD5"
```

That value is what an MD5-only client would put in the `md5_digest` field. Send it and nothing stronger: the file in the
`content` part, the MD5 declared, and `sha256_digest` and `blake2_256_digest` left off entirely:

```shell
curl -sS -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/ \
    -F ":action=file_upload" \
    -F "name=six" \
    -F "version=1.16.0" \
    -F "filetype=bdist_wheel" \
    -F "md5_digest=$MD5" \
    -F "content=@dist/six-1.16.0-py2.py3-none-any.whl"
```

The request returns `200` with no error body. peryx staged the wheel, saw that MD5 was the only digest you declared,
computed the MD5 of the bytes it received, found it equal to what you sent, and stored the file. Before the change that
made peryx accept this, the same MD5-only upload returned a `400`.

Change one character of the digest and submit the request again:

```shell
curl -sS -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/ \
    -F ":action=file_upload" \
    -F "name=six" \
    -F "version=1.16.0" \
    -F "filetype=bdist_wheel" \
    -F "md5_digest=00000000000000000000000000000000" \
    -F "content=@dist/six-1.16.0-py2.py3-none-any.whl"
```

peryx answers `400` with `md5_digest mismatch`. A declared MD5 is verified, not trusted: the right one passes and the
wrong one is refused, the same way SHA-256 is.

Ask the index for the project page and look at the hash on your file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/six/ | python3 -m json.tool | grep -A3 1.16.0
```

The entry carries a `sha256` hash and no `md5`. You declared MD5 on upload, but peryx content-addresses and serves the
file by SHA-256, so every installer verifies it the strong way. MD5 never appeared in what peryx published downstream.

## Part 3: yank a release by an equivalent version

Publish release `1.0`, then yank it by addressing `1.0.0`. The two spellings are PEP 440-equivalent. Create a minimal
project:

```shell
mkdir demo && cd demo
```

```toml
# pyproject.toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "demo-pkg"
version = "1.0"

[tool.setuptools]
py-modules = ["demo_pkg"]
```

```shell
touch demo_pkg.py
python -m build
```

The build writes `dist/demo_pkg-1.0.tar.gz` and `dist/demo_pkg-1.0-py3-none-any.whl`. The version on both filenames, and
in the metadata twine records, is `1.0`. Publish it and confirm the release is live and not yet yanked:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ dist/*

curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/demo-pkg/ | python3 -m json.tool | grep -A2 filename
```

The files list `"yanked": false`. Now yank the release, but address it as `1.0.0` rather than the `1.0` it was published
with:

```shell
curl -X PUT -u __token__:demo-secret \
    http://127.0.0.1:4433/root/pypi/demo-pkg/1.0.0/yank
```

peryx answers `200` with a non-zero count of files changed. `1.0.0` is the same release as `1.0` under
[PEP 440](https://peps.python.org/pep-0440/), so the request reached both files. Before peryx compared versions this
way, the same request matched nothing, returned zero, and left the release live. Read the project page again:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/demo-pkg/ | python3 -m json.tool | grep yanked
```

Both files now report `"yanked": true`. A resolver skips them, while a build pinned to the exact version can still fetch
them, exactly as [PEP 592](https://peps.python.org/pep-0592/) prescribes. A request for `1.0.0` would still never touch
`1.0.1`; equality is one release, not a range.

```shell
cd ..
```

## Part 4: delete a project named yank

`yank` is a real project name on pypi.org and a legal one under [PEP 503](https://peps.python.org/pep-0503/), yet it is
also the verb peryx puts in the URL that yanks a file. Give a throwaway project that name and build a wheel:

```shell
mkdir yank-demo && cd yank-demo
cat > pyproject.toml <<'EOF'
[project]
name = "yank"
version = "1.0"
EOF
mkdir -p src/yank && touch src/yank/__init__.py
uv build
```

`dist/` now holds `yank-1.0-py3-none-any.whl` and its sdist. Inject `demo-secret` as `UV_PUBLISH_TOKEN` through your
secret environment, then publish it and confirm it resolves:

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ dist/*
curl -s http://127.0.0.1:4433/root/pypi/simple/yank/ | grep yank
```

The project name and the action word are both `yank`, so the project segment comes first and the action second:

```shell
# yank every file of the project
curl -X PUT -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank

# un-yank it
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/yank
```

Each answers `200` with the number of files affected. Now the request that used to fail. Deleting the whole project
addresses it with a trailing slash and no action word:

```shell
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/yank/
```

peryx returns `200` and the file count, and the project is gone:

```shell
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4433/root/pypi/simple/yank/   # 404
```

Before the fix, peryx read the trailing `yank/` as an un-yank of a project with no name and answered `400`, leaving a
project named `yank` impossible to delete at the project level. The project segment in front of the action is what tells
peryx which one you mean.

## Compatibility results

peryx accepted a historical wheel with an un-normalized `.dist-info`, an upload whose only declared digest was a legacy
MD5, a yank addressed by an equivalent version spelling, and the project-level delete of a project named after a
mutation verb. Each was once a `400` or a silent no-op; each now matches what pypi.org and pip do, and peryx served
every file addressed by SHA-256 regardless of what the uploader declared.

## Related

- Do this in your own upload flow: [publish packages](@/ecosystems/pypi/guides/publish.md) and
  [yank and delete packages](@/ecosystems/pypi/guides/remove.md)
- The exact rules, tables, and error strings: [upload rules](@/ecosystems/pypi/reference/uploads.md)
- Accepted input and mutation rules: [upload behavior](@/ecosystems/pypi/uploads.md)
