+++
title = "Upload with one digest"
description = "Publish to peryx from a client or CI that declares only md5_digest, or any single digest, and read a digest rejection when one occurs."
weight = 8
+++

You have an upload path that declares a single content digest rather than the full SHA-256, BLAKE2, and MD5 that twine
sends, often a legacy tool or a CI script that computes only `md5_digest`. peryx accepts it, the same way pypi.org does,
as long as the digest matches the bytes. This guide covers posting such an upload and reading a rejection when a digest
disagrees.

## Post the upload

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

## Compute the digest your client sends

If your uploader lets you set the digest, compute it over the exact bytes you send. For MD5:

```shell
python3 -c "import hashlib,sys;print(hashlib.md5(open(sys.argv[1],'rb').read()).hexdigest())" \
    dist/<project>-<version>-py3-none-any.whl
```

Use `hashlib.sha256` or `hashlib.blake2b(..., digest_size=32)` for the other two. The value must be lowercase hex of the
field's length: 32 characters for MD5, 64 for SHA-256 and BLAKE2b-256.

## When only MD5 is declared

peryx computes MD5 over the staged content only when `md5_digest` is the sole digest on the form. If your client also
sends `sha256_digest` or `blake2_256_digest`, peryx verifies the stronger one and leaves the declared MD5 unchecked,
since the stronger digest already covers the same bytes. Either way the upload succeeds when the digest peryx verifies
matches. You do not need to strip the extra fields to get an MD5-only upload accepted; you need them only if MD5 is all
your client can produce.

## Read a digest rejection

A digest that does not match the content is a `400` naming the field that disagreed:

- `md5_digest mismatch`, `sha256_digest mismatch`, or `blake2_256_digest mismatch`: the declared digest did not equal
  the one peryx computed over the bytes it received. The file was corrupted in transit, or the digest was computed over
  different bytes than you uploaded. Recompute the digest over the exact file and post again.
- `<field> value "<value>" is not lowercase hex with the expected length`: the digest is malformed, uppercase, or the
  wrong length. Emit lowercase hex of the right width: 32 for MD5, 64 for SHA-256 and BLAKE2b-256.

A wrong `md5_digest` only surfaces when MD5 is the sole declared digest; when a stronger digest is present peryx checks
that one, and a bad MD5 alongside it goes unnoticed.

## Related

- Walk an MD5-only upload end to end: [publish with an MD5-only client](@/ecosystems/pypi/tutorials/md5-upload.md)
- The fields, what is verified, and every error: [upload digest fields](@/ecosystems/pypi/reference/upload-digests.md)
- Why peryx accepts MD5 and why it does not re-serve it: [MD5 on upload](@/ecosystems/pypi/upload-digests.md)
