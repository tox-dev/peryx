+++
title = "MD5 on upload"
description = "Why peryx accepts an upload that declares only a legacy MD5 digest, why it skips MD5 when a stronger digest is present, and why it never re-serves MD5."
weight = 6
+++

peryx accepts an upload that declares only a legacy `md5_digest`, verifies it, and then never mentions MD5 again. This
page explains why an index built around SHA-256 still takes an MD5-only upload, why it does not bother computing MD5
when a stronger digest is already declared, and why the file it serves back carries no MD5 at all.

## Why accept MD5 at all

peryx stands in front of PyPI as a drop-in, and [Warehouse](https://pypi.org/) (the software pypi.org runs) accepts
`md5_digest` on its upload API. Clients and CI have declared MD5 to PyPI for years: some older tooling sends only
`md5_digest`, and hand-rolled upload scripts often compute the one hash that ships in the Python standard library
without a thought about which. An index that rejects those uploads is stricter than the one it emulates, and the gap
shows up exactly where peryx is meant to disappear: a `twine upload` or a mirrored publish that succeeds against
pypi.org fails against peryx.

peryx used to reject an MD5-only upload outright, even with a correct digest, because it never computed MD5 and so had
nothing to check the declared value against. It now computes MD5 over the staged content when that is the only digest
the client declared, verifies it, and stores the file. A correct `md5_digest` is accepted; a wrong one is rejected with
`md5_digest mismatch`, the same way a wrong SHA-256 is. The behavior matches Warehouse, so an upload that works against
pypi.org works against peryx.

## Why skip MD5 when a stronger digest is present

peryx already hashes every upload with SHA-256, which is how it content-addresses the file, and with BLAKE2b-256, both
computed in one pass as it reads the stream. Verifying a declared `sha256_digest` or `blake2_256_digest` is then a
comparison against a hash it already holds.

Computing MD5 is different: peryx does not need MD5 for anything else, so producing it means reading the staged content
a second time. When the client declared a `sha256_digest` or `blake2_256_digest`, verifying that digest already proves
the bytes are the ones the client sent. A matching MD5 on top would add no assurance, and MD5 is the weaker hash of the
set, so re-reading the file to check it would be work spent to confirm something already confirmed. peryx verifies MD5
only when it is the sole digest on offer, which is the one case where skipping it would leave the upload unchecked.

## Why MD5 is not re-served

Accepting MD5 on upload does not make peryx an MD5 index. The simple-index entry for a stored file carries a `sha256`
hash and nothing else, and that is the hash every installer uses to verify what it downloaded. MD5 has been broken
against collision attacks for years; re-publishing it as a content hash would advertise a guarantee peryx will not stand
behind. SHA-256 supersedes it for that job, peryx computes SHA-256 for every file regardless of what the uploader
declared, and that is the digest it serves.

So MD5 lives entirely at the upload boundary. peryx accepts it because Warehouse does, verifies it when nothing stronger
was declared, and drops it the moment the file is stored.

## In practice

- The exact fields, what is verified, and the errors:
  [upload digest fields](@/ecosystems/pypi/reference/upload-digests.md)
- Publish with a client that declares only MD5:
  [publish with an MD5-only client](@/ecosystems/pypi/tutorials/md5-upload.md)
- Verify a single-digest upload flow: [upload with one digest](@/ecosystems/pypi/guides/md5-upload.md)
