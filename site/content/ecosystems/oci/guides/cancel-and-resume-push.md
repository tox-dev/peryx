+++
title = "Cancel and resume an OCI push"
description = "Cancel an in-progress blob upload to reclaim its staged bytes with a DELETE, and resume a push that got a 416 from the Range peryx reports."
weight = 5
+++

A container push is a series of blob uploads, and an upload can be left half-done: a client crashes mid-layer, or a
chunk arrives out of order and peryx answers `416`. This guide covers the two moves that clean up after each. Both act
on an [upload session](@/ecosystems/oci/reference/upload-sessions.md), so both need the hosted index's `upload_token` as
the Basic-auth password (`-u _:<token>`). Each section stands alone.

## Cancel an in-progress upload

An open session holds a staged temp file on the server. If you abandon the push, that file sits until the session's
one-hour idle timeout reaps it. To release it now, `DELETE` the session URL, the `Location` peryx returned when the
session opened:

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

- [Upload sessions and referrers validation](@/ecosystems/oci/reference/upload-sessions.md): the statuses, headers, and
  digest rules these commands rely on.
- [Why an aborted push cleans up its own bytes](@/ecosystems/oci/upload-conformance.md): what cancel and the `416`
  coordinates buy a client.
- [HTTP endpoints](@/ecosystems/oci/reference/endpoints.md): every `/v2/` upload verb and its success code.
