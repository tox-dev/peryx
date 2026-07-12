+++
title = "Publish with an MD5-only client"
description = "Upload a wheel declaring only a legacy md5_digest, the way older tooling does, and watch peryx accept it just as Warehouse would."
weight = 5
+++

In this tutorial you upload a wheel while declaring only a legacy `md5_digest`, no SHA-256 and no BLAKE2, the way some
older tooling and hand-rolled CI scripts still do. You watch peryx compute the MD5 over what you sent, verify it, and
store the file, then serve it back addressed by SHA-256. It takes about ten minutes and shows that peryx takes the same
uploads pypi.org does.

## Prerequisites

You need a peryx binary ([installation](@/core/installation.md) lists the channels), Python 3 with
[pip](https://pip.pypa.io/), and `curl`. Work in a scratch directory. twine always sends SHA-256, BLAKE2, and MD5
together, so to send MD5 alone you post the upload form yourself with `curl`.

## Start peryx with an upload token

Uploads are off until a hosted index has a token. Write a config that sets one and start peryx:

```toml
# peryx.toml
[[index]] # cached: read-through cache of pypi.org
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]] # hosted: your own uploads, gated by the token
name = "hosted"
upload_token = "demo-secret"

[[index]] # virtual: uploads shadow upstream behind one URL
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Leave it running and use a second terminal.

## Get a wheel and its MD5

Download a small pure-Python wheel from pypi.org:

```shell
pip download six==1.16.0 --no-deps --only-binary :all: --dest dist
```

Compute its MD5, the one digest you will declare:

```shell
MD5=$(python3 -c "import hashlib,sys;print(hashlib.md5(open(sys.argv[1],'rb').read()).hexdigest())" \
    dist/six-1.16.0-py2.py3-none-any.whl)
echo "$MD5"
```

That prefix is the value an MD5-only client would put in the `md5_digest` field. You will send it and nothing stronger.

## Post the upload with only md5_digest

The legacy upload form is a multipart POST to the index route. Send the file in the `content` part and declare the MD5,
leaving `sha256_digest` and `blake2_256_digest` off entirely. peryx accepts any username; the token is the password:

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

## See a wrong digest rejected

Change one character of the digest and post again to watch the check fire:

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

## Confirm what peryx serves

Ask the index for the project page and look at the hash on your file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/six/ | python3 -m json.tool | grep -A3 1.16.0
```

The entry carries a `sha256` hash and no `md5`. You declared MD5 on upload, but peryx content-addresses and serves the
file by SHA-256, so every installer verifies it the strong way. Install it back to prove the round trip:

```shell
python -m venv check
check/bin/pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ six==1.16.0
```

## What you saw

peryx accepted a wheel whose only declared digest was a legacy MD5, verified that MD5 against the bytes you sent, and
then served the file addressed by SHA-256. A correct MD5 passed and a wrong one was rejected; MD5 never appeared in what
peryx published downstream.

## Where next

- Do this in your own upload flow: [upload with one digest](@/ecosystems/pypi/guides/md5-upload.md)
- The fields and their exact rules: [upload digest fields](@/ecosystems/pypi/reference/upload-digests.md)
- Why peryx accepts MD5 but never re-serves it: [MD5 on upload](@/ecosystems/pypi/upload-digests.md)
