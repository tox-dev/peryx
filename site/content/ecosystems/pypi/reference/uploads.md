+++
title = "Upload rules"
description = "The exact upload checks peryx runs: wheel .dist-info matching, the digest fields it verifies, PEP 440 version matching for admin operations, and the mutation paths for verb-named projects, with every accept/reject table and error string."
weight = 3
aliases = [ "/ecosystems/pypi/reference/dist-info/", "/ecosystems/pypi/reference/upload-digests/", "/ecosystems/pypi/reference/version-match/", "/ecosystems/pypi/reference/reserved-names/"]
+++

peryx validates an upload by normalized identity, verifies whatever digest a client declares, and matches a
version-scoped mutation to a release the way the served page does. This page states each rule exactly: what passes, what
is rejected, and the error string a rejection returns. For why peryx works this way, see
[what peryx accepts on upload](@/ecosystems/pypi/uploads.md); for the routes these apply to, see
[HTTP endpoints](@/ecosystems/pypi/reference/endpoints.md).

## Wheel .dist-info matching

Every wheel carries one `*.dist-info` directory holding its `METADATA`, `WHEEL`, and `RECORD`.
[PEP 427](https://packaging.python.org/en/latest/specifications/binary-distribution-format/) names it
`{distribution}-{version}.dist-info`. peryx checks that this directory names the same project and version as the wheel
filename before it reads those files, so a wheel cannot claim to be `requests-2.32.5` while shipping another project's
metadata.

### What peryx compares

peryx derives the project name and version from the filename, then reads the project name and version from the
`.dist-info` directory, and compares the two by value:

- **Project name.** [PEP 503](https://peps.python.org/pep-0503/) normalization on both sides: lowercase, and collapse
  every run of `-`, `_`, or `.` into a single `-`. `Flask`, `flask`, and `FLASK` are one name; `Foo.Bar`, `foo_bar`, and
  `foo--bar` are one name.
- **Version.** [PEP 440](https://peps.python.org/pep-0440/) parsing and equality, not string equality. `1.0` and `1.0.0`
  are the same version, as are `1.0rc1` and `1.0RC1`.

The directory's stem (everything before `.dist-info`) is split into name and version at its **last** hyphen, matching
how the filename splits. peryx does **not** require the directory bytes to equal the normalized filename bytes. An
archive whose directory is spelled the un-normalized way older build tools wrote it is accepted, which is what pip and
[Warehouse](https://pypi.org/) (pypi.org) do. For why, see
[un-normalized wheels](@/ecosystems/pypi/uploads.md#un-normalized-wheels).

### Accepted

Each of these wheels is accepted; the filename is on the left, the directory the archive actually contains on the right.

| Wheel filename                    | `.dist-info` directory  | Why it matches                                 |
| --------------------------------- | ----------------------- | ---------------------------------------------- |
| `Flask-0.12-py2.py3-none-any.whl` | `Flask-0.12.dist-info`  | `Flask` and `flask` normalize the same         |
| `foo_bar-1.0-py3-none-any.whl`    | `Foo.Bar-1.0.dist-info` | `Foo.Bar` and `foo_bar` normalize to `foo-bar` |
| `pkg-1.0-py3-none-any.whl`        | `pkg-1.0.0.dist-info`   | `1.0` and `1.0.0` are equal under PEP 440      |

### Rejected

peryx still rejects a directory whose identity genuinely disagrees with the filename, and any archive without exactly
one `.dist-info`. For a wheel filed `Flask-1.0-py3-none-any.whl`, expected `flask-1.0.dist-info`:

| `.dist-info` directory | Error                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------- |
| `other-1.0.dist-info`  | `.dist-info directory other-1.0.dist-info does not match expected flask-1.0.dist-info` |
| `flask-2.0.dist-info`  | `.dist-info directory flask-2.0.dist-info does not match expected flask-1.0.dist-info` |
| `flask.dist-info`      | `.dist-info directory flask.dist-info does not match expected flask-1.0.dist-info`     |
| none                   | `missing .dist-info directory`                                                         |
| two or more            | `multiple .dist-info directories found: ...`                                           |

A directory with no hyphen in its stem, such as `flask.dist-info`, has no version segment to parse and so cannot match.
A version that does not parse as PEP 440 fails the same way. Every failure is an `invalid wheel:` message and a `400` on
upload.

### The required files

peryx reads `METADATA`, `WHEEL`, and `RECORD` from the directory the archive contains, spelled the way the archive
spells it, not from the normalized name it computed. A missing one of these is a distinct
`missing required <dir>/METADATA` (or `WHEEL`, or `RECORD`) failure.

## Upload digest fields

The legacy upload API lets a client declare a content digest of the file it sends. peryx accepts three digest fields and
verifies whichever the client declared against the bytes it staged. A correct digest passes; a wrong one is rejected.

### Accepted fields

An upload's multipart form may carry any of these fields alongside the `content` part:

| Field               | Algorithm   | Hex length |
| ------------------- | ----------- | ---------- |
| `sha256_digest`     | SHA-256     | 64         |
| `blake2_256_digest` | BLAKE2b-256 | 64         |
| `md5_digest`        | MD5         | 32         |

Any one of them suffices, and none is required. peryx always computes the SHA-256 it content-addresses the file by,
independent of what the client declares, so an upload that declares no digest at all is still stored. twine and
`uv publish` normally send all three; older tooling and minimal CI scripts sometimes send `md5_digest` alone.

### What peryx verifies

peryx hashes the staged bytes with SHA-256 and BLAKE2b-256 as it reads the upload stream, so verifying a declared
`sha256_digest` or `blake2_256_digest` costs nothing beyond a comparison. It verifies each field the client declared:

- **`sha256_digest`** against the content SHA-256 it computed.
- **`blake2_256_digest`** against the content BLAKE2b-256 it computed.
- **`md5_digest`** only when it is the sole declared digest, meaning neither `sha256_digest` nor `blake2_256_digest` is
  present. peryx does not compute MD5 while staging, so this is the one case that reads the staged content a second
  time. When a stronger digest is declared, that verification already covers the bytes, and peryx leaves the declared
  MD5 unverified rather than re-reading the file.

The check is the same regardless of field: the declared value must be lowercase hex of the field's length and must equal
the digest peryx computed.

### Rejections

A declared digest that does not match the content is a `400`:

| Condition                                      | Status | Message                                                                 |
| ---------------------------------------------- | ------ | ----------------------------------------------------------------------- |
| `md5_digest` disagrees with the content        | `400`  | `md5_digest mismatch`                                                   |
| `sha256_digest` disagrees with the content     | `400`  | `sha256_digest mismatch`                                                |
| `blake2_256_digest` disagrees with the content | `400`  | `blake2_256_digest mismatch`                                            |
| a digest is not lowercase hex of its length    | `400`  | `<field> value "<value>" is not lowercase hex with the expected length` |

The mismatch message is always `<field> mismatch`, naming the field that disagreed. A wrong `md5_digest` is only reached
when MD5 is the sole declared digest; when a stronger digest is present peryx verifies that one and never inspects the
MD5.

### What peryx does not do with digests

peryx does not advertise MD5 downstream. The simple-index entry for a stored file carries a `sha256` hash and no `md5`,
so clients read and verify the artifact by SHA-256 regardless of which digest the uploader declared. MD5 is a weak hash;
peryx accepts it on upload for parity with the index it fronts, not as a content guarantee it re-serves.

## Attestations

The upload multipart form may carry an `attestations` field: a JSON array of
[PEP 740](https://peps.python.org/pep-0740/) attestation objects. When present and valid, peryx stores the bundle and
publishes a `provenance` URL on the file's Simple API entry; when absent, the file publishes with no provenance.

### What peryx validates

Validation binds every attestation to the uploaded distribution and bounds the untrusted input. peryx does not verify
signatures, certificates, or transparency-log inclusion.

| Check                | Rule                                                                             |
| -------------------- | -------------------------------------------------------------------------------- |
| Field shape          | A JSON array of at least one object; at most 32 attestations                     |
| Version              | Each attestation's `version` is `1`                                              |
| Envelope             | `envelope.statement` is base64 that decodes to a valid in-toto statement         |
| Subject digest       | Some subject's `digest.sha256` equals the uploaded file's SHA-256                |
| Subject name         | If that subject names a file, the name equals the upload filename                |
| Per-attestation size | Each attestation is at most 256 KiB                                              |
| Aggregate field size | The whole `attestations` field is at most 1 MiB                                  |
| Statement size       | A decoded statement is at most 64 KiB                                            |
| Parser depth         | The field must parse within the JSON recursion limit; deeper nesting is rejected |

### Publication is atomic

A valid bundle and its distribution publish in one transaction. Any validation failure returns `400` and publishes
neither the file nor its provenance. The `400` body names the offending attestation by index and the reason, for example
`attestation 0 subject digest does not match the uploaded distribution` or
`attestations field carries 40 attestations; at most 32 are accepted`.

### What peryx stores and serves

peryx wraps the accepted attestations into a provenance object
`{"version": 1, "attestation_bundles": [{"publisher": null, "attestations": [...]}]}`, stores it content-addressed in
the blob store keyed by the artifact's digest, and serves it at `.../files/{sha256}/{filename}.provenance` with media
type `application/vnd.pypi.integrity.v1+json`. The `publisher` is `null` because peryx does not resolve a Trusted
Publisher identity. See [Simple API serving](@/ecosystems/pypi/reference/simple-api.md#provenance-and-attestations) for
the served shape.

## Version matching for admin operations

The version-scoped admin operations address a release by version: yank, un-yank, delete, and promote. Each reads the
version recorded on every upload of the project and acts on the files whose version matches the one in the request. The
match is [PEP 440](https://peps.python.org/pep-0440/) equality of the release, not a byte-exact comparison of the two
strings, so a request addressed to `1.0.0` reaches a file uploaded with form version `1.0`.

### The rule

Two versions match when either holds:

- their strings are byte-identical, or
- both parse as PEP 440 versions and those parsed versions are equal.

When either string fails to parse as a PEP 440 version, only the byte-identical case remains: the comparison falls back
to exact string equality. This is the same equality the served project page applies when it decides which files a
version filter shows, so an operation and the page it acts on agree on what one release is.

### What counts as equal

PEP 440 equality normalizes the release segment, so trailing-zero spellings of the same release are equal, while a
different release, or a version carrying a distinct
[local segment](https://peps.python.org/pep-0440/#local-version-identifiers), is not.

| Requested   | Recorded on upload | Match | Why                                             |
| ----------- | ------------------ | ----- | ----------------------------------------------- |
| `1.0.0`     | `1.0`              | yes   | same release, `1.0` == `1.0.0`                  |
| `1.0.0.0`   | `1.0`              | yes   | same release, trailing zeros normalize          |
| `1.0.0.0`   | `1.0.0`            | yes   | same release                                    |
| `1.0.0`     | `1.0.1`            | no    | different release                               |
| `1.0+build` | `1.0.0+build`      | yes   | same release and same local segment             |
| `1.0+build` | `1.0`              | no    | local segment present on one side only          |
| `1.0.0`     | `nightly`          | no    | `nightly` does not parse; byte comparison fails |
| `nightly`   | `nightly`          | yes   | neither parses; byte-identical                  |

### The record fallback

Matching reads the version stored on each upload record, the form value captured when the file was published, not a
value re-derived from the filename. When that stored string is not a parseable PEP 440 version, or the requested version
is not, the comparison is byte-exact: an unparseable recorded version matches only a request that spells it the same
way. Delete relies on this. When the served-page filter matches nothing, delete falls back to matching on the stored
record, and the two notions of equality have to agree or the fallback misses the file it should remove.

### Scope

The rule governs every version-scoped form of these endpoints:

- `PUT /{route}/{project}/{version}/yank` and its `DELETE` un-yank
- `DELETE /{route}/{project}/{version}/`
- `PUT /{route}/{project}/{version}/promote?from=...`

The project-wide forms that carry no version, such as `PUT /{route}/{project}/yank`, act on every file of the project
and never compare versions.

### What version matching does not do

The match is equality of one release, not a range or a prefix. A request for `1.0` does not reach `1.0.1` or `1.1`. It
does not ignore the local segment: `1.0+build` and `1.0` are distinct releases. And it never rewrites a stored version;
the record keeps the spelling it was uploaded with, and matching is decided per request.

## Mutation paths for verb-named projects

peryx names its mutation actions in the URL. A `PUT` yanks, restores, or promotes; a `DELETE` deletes or un-yanks. The
action is the last path segment: `PUT /{route}/{project}/yank`, `DELETE /{route}/{project}/yank` (un-yank),
`PUT /{route}/{project}/{version}/restore`. `yank`, `restore`, and `promote` are also legal
[PEP 503](https://peps.python.org/pep-0503/) project names, so a project can be named after the verb that acts on it.

### The grammar

peryx peels a trailing action segment only when a project segment precedes it: the text left after removing the verb
must end in `/`, so the request names a project before it names an action. A path that is nothing but the verb is not an
action, it is the project. Names are compared after PEP 503 normalization, so `Yank`, `YANK`, and `yank` are the same
project and collide the same way.

The table uses route `root/pypi` and a project whose normalized name is `yank`.

| Request                          | Meaning                        |
| -------------------------------- | ------------------------------ |
| `DELETE /root/pypi/yank/`        | delete the project `yank`      |
| `DELETE /root/pypi/yank/1.0/`    | delete version `1.0` of `yank` |
| `PUT /root/pypi/yank/yank`       | yank every file of `yank`      |
| `PUT /root/pypi/yank/1.0/yank`   | yank version `1.0` of `yank`   |
| `DELETE /root/pypi/yank/yank`    | un-yank the project `yank`     |
| `PUT /root/pypi/restore/restore` | restore the project `restore`  |

`promote` is always versioned and takes `from={source route}`, so its verb-named form is
`PUT /root/pypi/promote/1.0/promote?from=staging` to promote version `1.0` of the project `promote`. A promote without a
version answers `400` with `promotion requires a version`, verb-named or not.

### What changed

peryx used to strip the verb even when it was the whole path, reading the request as the action on an empty project.
`DELETE /root/pypi/yank/`, a delete of the project `yank`, parsed as an un-yank of a project with no name and failed
validation with `400 Bad Request`. The project named `yank` had no working project-level delete: its own name shadowed
the action. The versioned delete `DELETE /root/pypi/yank/1.0/` and the project-level yank `PUT /root/pypi/yank/yank`
already worked, because each puts a project segment before the trailing token.

The scope was narrow. `DELETE` peels only `yank`, so `yank` was the one project name whose project-level delete broke;
`restore` and `promote` never collided on `DELETE`. The fix drops the whole-path case from the grammar for every verb,
so a project named after any mutation verb stays addressable on both methods.

### Not affected

Uploading a project named `yank`, `restore`, or `promote` was never blocked; the collision lived only in the mutation
router, and the upload path parses the name straight. Every request above takes the same upload token as any other
mutation, and a `200` carries the number of files affected, a `404` means nothing matched.

## In practice

- The standards these implement: [standards](@/ecosystems/pypi/reference/standards.md)
- The full set of upload checks in one place: [publish packages](@/ecosystems/pypi/guides/publish.md)
- Target a release by any equivalent spelling, or host a verb-named project:
  [yank and delete packages](@/ecosystems/pypi/guides/remove.md)
- Walk a legacy wheel, an MD5-only client, an equivalent-version yank, and a verb-named delete end to end:
  [publish and manage a release](@/ecosystems/pypi/tutorials/publish-and-manage.md)
- Why peryx accepts these inputs: [what peryx accepts on upload](@/ecosystems/pypi/uploads.md) </content>
