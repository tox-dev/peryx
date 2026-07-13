+++
title = "Work with registry behavior"
description = "Proxy an upstream that advertises manifest digests in sha512 or another algorithm and pin images by the digest peryx reports, and cancel or resume a blob upload when a push is abandoned or gets a 416."
weight = 5
aliases = [ "/ecosystems/oci/guides/non-sha256-registry/", "/ecosystems/oci/guides/cancel-and-resume-push/"]
+++

Two recurring registry tasks: fronting an upstream whose content digests are not sha256, and cleaning up a blob upload
that stalled. This guide covers both, the digest to pin by when the upstream uses sha512, and the two moves that reclaim
or resume a half-done push. The examples assume peryx at `http://127.0.0.1:4433`.

## Front a registry that uses non-sha256 digests

Most registries content-address with sha256, but the OCI spec allows sha512 and other registered algorithms, and some
registries advertise their `Docker-Content-Digest` in one of them. peryx proxies such an upstream with no special
configuration; the two things to get right are which digest to pin by and where the support stops.

### Point a cached index at it

Nothing about the digest algorithm is configurable, so a cached index is the usual one:

```toml
# peryx.toml
[[index]]
name = "reg"
route = "reg"
ecosystem = "oci"
cached = "https://registry.example.com"
```

Pull as normal. peryx fetches the manifest, hashes the exact bytes under its own sha256, and serves them:

```shell
crane manifest --insecure 127.0.0.1:4433/reg/team/app:1.0
```

### Pin by the digest peryx reports, not the upstream's

peryx addresses every manifest it stores by sha256, so the digest it hands your clients for a tag pull is a `sha256:`
value, even when the upstream advertised sha512. Read it from the response header:

```shell
curl -sI http://127.0.0.1:4433/v2/reg/team/app/manifests/1.0 | grep -i docker-content-digest
```

Pin deployments to that sha256. It is the digest peryx serves the image under and the one a client verifies the bytes
against. If you carry an upstream sha512 digest from elsewhere, a pull by it still works, and peryx serves the bytes
under the digest you request and echoes it back:

```shell
crane manifest --insecure 127.0.0.1:4433/reg/team/app@sha512:<hex>
```

### What still requires sha256

The relaxation is scoped to reading a manifest through a proxy. Three things stay sha256 only:

- **Blobs.** A blob pull, mount, or upload commit must use `sha256:`; any other algorithm answers `400 DIGEST_INVALID`
  with `only sha256 blob digests are supported`. A client that pushes a blob under a non-sha256 digest is rejected.
- **A wrong sha256 advertisement.** If the upstream advertises a `sha256:` digest that does not hash the bytes it sent,
  that is a corrupting hop, and peryx returns `502` and caches nothing, unchanged.
- **Offline mirror pins.** A [mirror](@/ecosystems/oci/guides/air-gapped.md) entry pinned by digest must be `sha256:`.
  `repo@sha512:…` fails the mirror's own sha256 comparison; mirror by tag instead, which stores under the canonical
  sha256.

### Verify the proxy

Confirm a tag pull succeeds and reports a sha256 digest:

```shell
curl -si http://127.0.0.1:4433/v2/reg/team/app/manifests/1.0 | head -3
```

A `200` with a `docker-content-digest: sha256:…` line is the proxy working. A `502` means the upstream advertised a
`sha256:` digest that did not match its bytes, a corrupting proxy between you and the upstream rather than an algorithm
peryx declined. The exact rules are in
[content digest algorithms](@/ecosystems/oci/reference/registry-behavior.md#content-digest-algorithms), and the
reasoning in [why peryx accepts a non-sha256 content digest](@/ecosystems/oci/registry-behavior.md#content-digests).

## Mount a blob from another repository

Use a cross-repository mount when you reuse a layer from another repository. Include the peryx index route in both
names. Read the digest from the source manifest, then request the mount with that full source name:

```shell
curl -sS -i -u _:<token> -X POST \
  "http://127.0.0.1:4433/v2/images/target/app/blobs/uploads/?mount=sha256:<hex>&from=images/source/app"
```

peryx returns `201 Created` after it links the target without transferring a layer body. peryx opens the upload session
in `Location` with `202 Accepted` when the source lacks the digest or the bytes. It takes the same path without `from`.
You need pull access for a private source; peryx returns the source repository's `401` challenge when the credential may
push the target but cannot pull `images/source/app`.

## Cancel an in-progress upload

A container push is a series of blob uploads, and an upload can be left half-done: a client crashes mid-layer, or a
chunk arrives out of order and peryx answers `416`. Both cleanups act on an
[upload session](@/ecosystems/oci/reference/registry-behavior.md#upload-sessions), so both need the hosted index's
`upload_token` as the Basic-auth password (`-u _:<token>`).

An open session holds a staged temp file on the server. After one hour without a status request or `PATCH` attempt,
peryx removes that file during the next maintenance pass; the pass runs once per minute. To release it before the
timeout, `DELETE` the session URL, the `Location` peryx returned when the session opened:

```shell
curl -sS -i -u _:<token> -X DELETE \
  http://127.0.0.1:4433/v2/images/<repo>/blobs/uploads/<session>
# 204 No Content
```

`204` means the session and its staged bytes are gone. A session id peryx does not know, including one you already
finished or cancelled, answers `404 BLOB_UPLOAD_UNKNOWN`:

```shell
curl -sS -i -u _:<token> -X DELETE \
  http://127.0.0.1:4433/v2/images/<repo>/blobs/uploads/<already-gone>
# 404 Not Found
```

Send follow-up requests to the exact `Location` from the opening response. peryx returns `404 BLOB_UPLOAD_UNKNOWN` when
the repository path differs, including when the caller can write both repositories.

Because sessions live in the peryx process, a restart drops every open one, so a `DELETE` after a restart also answers
`404`. Reach for cancel in a CI job that aborts a build, or a script that opens a session it then decides not to use, so
the server is not left holding bytes no one will finish.

## Resume a push that got a 416

peryx answers a `PATCH` whose `Content-Range` does not begin where the last chunk ended with
`416 Range Not Satisfiable`, and keeps the bytes it already has. The `416` reports the session coordinates you need to
continue:

```text
416 Range Not Satisfiable
Location: /v2/images/<repo>/blobs/uploads/<session>
Docker-Upload-UUID: <session>
Range: 0-<end>
```

Read `Range: 0-<end>`: it is the byte span peryx holds, so the next chunk must start at byte `<end> + 1`. Re-send the
chunk from there against the `Location` URL:

```shell
curl -sS -i -u _:<token> -X PATCH \
  -H 'Content-Type: application/octet-stream' \
  -H 'Content-Range: <end+1>-<new-end>' \
  --data-binary @chunk \
  http://127.0.0.1:4433/v2/images/<repo>/blobs/uploads/<session>
# 202 Accepted, Range: 0-<new-end>
```

If you have lost track of how much landed, ask the session directly. `GET` on the session URL reports progress as
`Range: 0-<end>` without changing anything, so you can read the offset before you resume:

```shell
curl -sS -i -u _:<token> \
  http://127.0.0.1:4433/v2/images/<repo>/blobs/uploads/<session>
# 204 No Content, Range: 0-<end>
```

Then finish the push with `PUT …?digest=sha256:<hex>` once the last chunk is in. `docker`, `podman`, and `crane` run
this recovery for you; you only drive it by hand when you are scripting an upload or debugging one that stalls, as in
[push a blob chunk by chunk](@/ecosystems/oci/tutorials/chunked-upload.md).

## Related

- The statuses, headers, and digest rules these commands rely on:
  [registry behavior](@/ecosystems/oci/reference/registry-behavior.md)
- Why peryx serves the registry this way:
  [why peryx serves the registry the way it does](@/ecosystems/oci/registry-behavior.md)
- Every `/v2/` upload verb and its success code: [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md) </content>
