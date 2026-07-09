+++
title = "HTTP endpoints"
description = "The OCI distribution-spec /v2/ routes peryx serves: manifests, blobs, uploads, tags, and referrers."
weight = 2
+++

peryx serves the [OCI distribution spec](https://github.com/opencontainers/distribution-spec) `/v2/` pull-and-push API.
Every route is `/v2/<name>/…`, and `<name>` carries the index route as a prefix: peryx matches the longest configured
OCI index route that segment-aligns with `<name>`, and the remainder is the upstream repository. An index at route
`dockerhub` serves Docker Hub's `library/alpine` as `/v2/dockerhub/library/alpine/…`. A request whose `<name>` matches
no OCI index route answers `404 NAME_UNKNOWN`. For the concept map, see [OCI](@/ecosystems/oci/_index.md); for the wire
standards, see [standards](@/ecosystems/oci/reference/standards.md).

`<name>` is one or more lowercase path components (`[a-z0-9._-]`, no bare `.`/`..`, ≤ 255 chars). A manifest
`<reference>` is a tag (`[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`) or a digest (`algorithm:encoded`). Blob digests must be
`sha256:…`; any other algorithm is `400 DIGEST_INVALID`.

## Endpoints

| Method       | Path                                 | Purpose                                | Success       |
| ------------ | ------------------------------------ | -------------------------------------- | ------------- |
| `GET`        | `/v2/`                               | API version check                      | `200`         |
| `GET` `HEAD` | `/v2/<name>/manifests/<reference>`   | Pull a manifest by tag or digest       | `200`         |
| `PUT`        | `/v2/<name>/manifests/<reference>`   | Push a manifest                        | `201`         |
| `DELETE`     | `/v2/<name>/manifests/<reference>`   | Delete a manifest or untag             | `202`         |
| `GET` `HEAD` | `/v2/<name>/blobs/<digest>`          | Pull a blob (range-capable)            | `200` / `206` |
| `DELETE`     | `/v2/<name>/blobs/<digest>`          | Delete a blob                          | `202`         |
| `GET`        | `/v2/<name>/blobs/<digest>/contents` | List a layer's files, or preview one   | `200`         |
| `POST`       | `/v2/<name>/blobs/uploads/`          | Begin, mount, or monolithically push   | `202` / `201` |
| `GET`        | `/v2/<name>/blobs/uploads/<session>` | Report upload progress                 | `204`         |
| `PATCH`      | `/v2/<name>/blobs/uploads/<session>` | Append a chunk                         | `202`         |
| `PUT`        | `/v2/<name>/blobs/uploads/<session>` | Finish an upload                       | `201`         |
| `GET`        | `/v2/<name>/tags/list`               | List tags, paginated                   | `200`         |
| `GET`        | `/v2/<name>/referrers/<digest>`      | List manifests referring to `<digest>` | `200`         |

## Version check

`GET /v2/` (with or without the trailing slash) answers `200` with `Docker-Distribution-API-Version: registry/2.0` and
an empty body. It takes no authentication and is the first request every container client sends.

## Manifests

peryx stores a manifest byte-for-byte and addresses it by the sha256 of those exact bytes, so the
`Docker-Content-Digest` a client verifies always matches what it pushed or pulled.

`GET`/`HEAD /v2/<name>/manifests/<reference>` resolves the reference through the index's members hosted-first (a hosted
image shadows the same name upstream, the [dependency-confusion defense](@/core/glossary.md#shadowing)). A hosted member
reads its stored tag mapping; an online proxy member revalidates the tag against upstream and caches the result. A pull
by digest is served from the content-addressed store when present, else pulled through. The response carries the stored
`Content-Type`, `Docker-Content-Digest`, and `Content-Length`; a `HEAD` returns those headers with an empty body. A
reference no member can serve is `404 MANIFEST_UNKNOWN`.

`PUT /v2/<name>/manifests/<reference>` stores the request body under its canonical `sha256:` digest and, when
`<reference>` is a tag, points that tag at the digest. The `Content-Type` header is recorded as the manifest's media
type (defaulting to `application/vnd.oci.image.manifest.v1+json`); bodies over 4 MiB are rejected. When `<reference>` is
a digest, the body must hash to it or the response is `400 DIGEST_INVALID`. Success is `201` with `Location`,
`Docker-Content-Digest`, and (when the manifest declares a `subject`) an `OCI-Subject` header echoing that subject's
digest. A declared subject is also recorded for the referrers API.

`DELETE /v2/<name>/manifests/<reference>` removes the manifest by digest, or drops the tag mapping when `<reference>` is
a tag. Success is `202`; a reference that was not present is `404 MANIFEST_UNKNOWN`.

## Blobs

Blobs are content-addressed and shared across every index, so a store hit serves all of them and a delete removes the
bytes globally.

`GET`/`HEAD /v2/<name>/blobs/<digest>` serves the blob from the store, pulling it through the index's online proxy
members on a miss; concurrent misses for one digest share a single upstream fetch. The response carries
`Content-Type: application/octet-stream`, `Accept-Ranges: bytes`, `Docker-Content-Digest`, and `Content-Length`. A
single `Range: bytes=…` request answers `206` with `Content-Range`; an unsatisfiable or malformed `bytes` range is `416`
with `Content-Range: bytes */<size>`; a range in any other unit is ignored and the full blob served. A digest no proxy
member has is `404 BLOB_UNKNOWN`; a non-`sha256` digest is `400 DIGEST_INVALID`.

`DELETE /v2/<name>/blobs/<digest>` removes the bytes and answers `202`, or `404 BLOB_UNKNOWN` when they were absent.

### Layer contents

`GET /v2/<name>/blobs/<digest>/contents` is peryx's own layer browser, not a distribution-spec route (a plain registry
answers `404` here, so it never collides with a pull). It ensures the layer blob is present (fetching it once through
the single-flight gate on a miss), then reads it as a tar. Without a query it answers `200` with
`{"members": [{"path", "size", "kind", "previewable"}, …]}`, listing the layer's files. With `?member=<path>&offset=<n>`
it previews one text member: `text/plain` bytes plus `x-peryx-member-size`, `x-peryx-member-offset`, and (when more
follows) `x-peryx-next-offset` headers, so a large member pages in bounded chunks. A binary member is `415`, an unknown
member `404`, an offset past the member `416`, and an unreadable layer `422`. The web UI's file browser reads this route
to show a layer's contents.

### Uploads

A push writes blobs through an upload session started with `POST /v2/<name>/blobs/uploads/`. Three shapes:

- **Cross-repo mount**: `POST …/uploads/?mount=<digest>[&from=<repo>]`. When `<digest>` is already stored, peryx answers
  `201` immediately with `Location: /v2/<name>/blobs/<digest>` and `Docker-Content-Digest`; no bytes transfer. When it
  is not stored, the mount is ignored and an ordinary session opens.
- **Monolithic**: `POST …/uploads/?digest=<digest>` with the blob as the body. peryx streams it in, verifies the digest
  on commit, and answers `201`.
- **Chunked**: a bare `POST …/uploads/` opens a session and answers `202` with
  `Location: /v2/<name>/blobs/uploads/<session>`, `Docker-Upload-UUID`, and `Range: 0-<n>`. The client appends with
  `PATCH` requests, then finishes with `PUT …/uploads/<session>?digest=<digest>`.

`PATCH /v2/<name>/blobs/uploads/<session>` appends a chunk and answers `202` with the updated `Range` and
`Docker-Upload-UUID`. A chunk whose `Content-Range` does not begin where the last one ended is
`416 Range Not Satisfiable` and the session keeps its bytes so the client can resend.

`PUT /v2/<name>/blobs/uploads/<session>?digest=<digest>` appends any trailing body, then verifies and commits under
`<digest>`, answering `201` with `Location` and `Docker-Content-Digest`. A digest mismatch on commit is
`400 DIGEST_INVALID`; a missing `digest` query is also `400 DIGEST_INVALID`.

`GET /v2/<name>/blobs/uploads/<session>` reports progress: `204` with `Location`, `Docker-Upload-UUID`, and
`Range: 0-<n>`. An unknown session (including one already committed) is `404 BLOB_UPLOAD_UNKNOWN`. Sessions are
in-memory and process-local; they do not survive a restart.

## Tags

`GET /v2/<name>/tags/list` answers `200` with `application/json` `{"name": "<name>", "tags": [...]}`. A lone online
proxy index passes the upstream response through verbatim, forwarding the client's query. Every other case (a hosted
index or a virtual index) unions its members' tags under the requested name, sorted, then applies pagination:
`?n=<count>` caps the page and `?last=<tag>` resumes after a tag. When `n` truncates the set, the response adds a
`Link: </v2/<name>/tags/list?n=<n>&last=<marker>>; rel="next"` header pointing at the next page.

## Referrers

`GET /v2/<name>/referrers/<digest>` returns an OCI image index (`application/vnd.oci.image.index.v1+json`) whose
`manifests` are the descriptors of every pushed manifest that declared `<digest>` as its `subject`, aggregated across
the index's members. Each descriptor carries `mediaType`, `digest`, `size`, and (when the source manifest had them)
`artifactType` and `annotations`. When nothing refers to `<digest>`, `manifests` is empty. peryx does not apply an
`artifactType` filter or emit `OCI-Filters-Applied`.

## Discovery

`GET /+api` is peryx's cross-ecosystem discovery document, not a `/v2/` route. It lists every configured index; an OCI
index's entry carries its `/v2/` registry URL, the capabilities peryx serves for it, and a `docker pull` snippet (plus
`docker login`/`docker push` when the index accepts writes) with the host taken from the request. `GET /<route>/+api`
returns the single index's entry. The web UI reads the same data to show a copyable pull command on each tag.

## Authentication

Pull requests (the version check and every `GET`/`HEAD` on manifests, blobs, tags, and referrers) take no
authentication. The `401` + `WWW-Authenticate: Bearer` token handshake belongs to the pull-through path: peryx runs it
as a *client* against an upstream registry that demands it (fetching a bearer token from the challenge realm and caching
it per scope), never as a challenge to its own callers.

Writes (`PUT`/`DELETE` on manifests, `DELETE` on blobs, every blob upload verb, and the upload-status `GET`) require
`Authorization: Basic` where the password is the target hosted index's `upload_token`; the username is ignored. A
virtual index routes the write to its configured upload-target member. Responses:

- `401 UNAUTHORIZED` with `WWW-Authenticate: Basic realm="peryx"`: missing or wrong credentials.
- `403 DENIED`: the resolved index is read-only (proxy, or virtual with no upload target), or its `upload_token` is
  unset (uploads disabled).
- `404 NAME_UNKNOWN`: `<name>` matches no OCI index route.

`docker login` / `podman login` / `crane auth login` against peryx use Basic auth with the token as the password.

## Error responses

Errors use the distribution-spec shape `{"errors": [{"code": "<CODE>", "message": "..."}]}` with
`Content-Type: application/json`, each code paired with its canonical status:

| Code                  | Status | Meaning                                                    |
| --------------------- | ------ | ---------------------------------------------------------- |
| `NAME_UNKNOWN`        | `404`  | `<name>` matches no OCI index route                        |
| `MANIFEST_UNKNOWN`    | `404`  | No member can serve the reference                          |
| `BLOB_UNKNOWN`        | `404`  | The blob is neither stored nor upstream                    |
| `BLOB_UPLOAD_UNKNOWN` | `404`  | No such upload session                                     |
| `DIGEST_INVALID`      | `400`  | Non-`sha256` digest, or bytes that do not match the digest |
| `MANIFEST_INVALID`    | `400`  | An upstream manifest's digest disagreed with the request   |
| `DENIED`              | `403`  | Read-only index, or uploads disabled                       |
| `UNAUTHORIZED`        | `401`  | Missing or wrong upload credentials                        |
| `UNSUPPORTED`         | `405`  | The method is not defined for that route                   |

An upstream that fails or answers unexpectedly during a pull-through returns `502` with code `UNKNOWN`, so the puller
does not mistake a gateway fault for a client error it would not retry.
