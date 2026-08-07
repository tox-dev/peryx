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

## Import a directory

`peryx import-dir` loads local wheels and gzip-compressed source distributions into a hosted index:

```shell
peryx import-dir root/pypi ./dist --data-dir /var/lib/peryx
peryx import-dir hosted ./dist --config peryx.toml
```

The index argument accepts a hosted name, a hosted route, or a virtual route with a hosted upload target. The command
walks the directory tree and applies the upload checks on this page to `.whl` and `.tar.gz` files. It skips unsupported
files and rejects invalid distributions.

Output is tab-separated:

```text
status  filename  project  version  reason
```

Each row has an `imported`, `skipped`, or `rejected` status and a reason. A summary row reports the three counts.

## Durable ingress admission

A PyPI client uploads to whichever datacenter it reaches. Before peryx stores an upload, the datacenter it arrived at
durably admits it: it streams the bytes to a staged blob and records a durable write intent bound to the tenant (the
upload route), the ecosystem authority key (the normalized project), the artifact digest, the byte size, the ingress
datacenter, and an operation id. Admission succeeds only once both the bytes and the intent are durable in that
datacenter, so an accepted upload survives a restart near the client. Publication, home-datacenter assignment, and
cross-datacenter replication run after admission and are not part of it.

| Situation                                           | Status | Result                                                                                          |
| --------------------------------------------------- | ------ | ----------------------------------------------------------------------------------------------- |
| A new upload is streamed and staged                 | `200`  | One intent is recorded, bound to the identity above                                             |
| The same filename and content is resent             | `200`  | The resend resolves the same intent; no bytes are staged twice                                  |
| The same filename is resent with different content  | `400`  | `File already exists: "<name>" has different content; use a different filename`                 |
| The declared digest does not match the bytes        | `400`  | `<field> mismatch`, before an intent is recorded                                                |
| The backend cannot prove same-datacenter durability | `503`  | `same-datacenter durability unavailable: <guarantee>`                                           |
| The authority is at its retention record ceiling    | `503`  | `ingress admission retention is full: authority is at its record ceiling`, with a `Retry-After` |
| The next intent would cross the byte ceiling        | `503`  | `ingress admission retention is full: authority is at its byte ceiling`, with a `Retry-After`   |

**Idempotency.** The intent is keyed by the upload's file identity within its tenant and authority, so a retried upload
resolves one intent rather than staging its bytes a second time. A client that loses the response and resends the same
distribution is admitted again and publishes the same file idempotently; a resend of the same filename carrying
different bytes is refused with the same error publication returns, so the client-visible upload contract is unchanged.

**Checksums.** The `sha256_digest`, `blake2_256_digest`, and `md5_digest` a client declares are verified against the
streamed bytes before admission records anything, so a checksum mismatch is rejected with its existing error and leaves
no staged intent. See [the digest fields peryx verifies](#upload-digest-fields) below.

**Same-datacenter durability.** Admission requires the configured blob backend to prove race-safe, integrity-checked
writes. Create-if-absent prevents a concurrent writer from clobbering a staged artifact, and checksum validation rejects
corrupted writes. A local filesystem proves both. An object store proves them only when its endpoint honors conditional
writes and checksum validation; one that cannot is refused at admission rather than acknowledging an upload it cannot
make durable.

**Retention limits and backpressure.** The retention buffer is bounded per authority, not globally: each authority
stages up to a record ceiling and a byte ceiling, so one busy project cannot starve the ledger of every other while a
home outage holds writes un-finalized. A new intent is refused once its authority holds its record ceiling, or once
admitting it would cross its byte ceiling. Admission trips backpressure at 80% of either ceiling, one shed signal before
the hard bound. The node admits an upload that crosses the soft threshold and logs the authority's retained records and
bytes so an operator sees capacity pressure building before writes are refused. When an authority reaches a hard
ceiling, a further upload is shed with `503` and a
[`Retry-After`](https://www.rfc-editor.org/rfc/rfc9110.html#field.retry-after) so the client backs off and retries
rather than losing the write; the buffer drains as the home finalizes the retained intents.

**Order, retention, and expiry.** Every admission takes the next value of a durable, never-reused sequence, and the
pending set is keyed by it. After a restart, the node resumes the drain in admission order across interleaved
authorities instead of key order. The node retains a pending intent because its write may still finalize, so a stalled
home sheds new load through admission rather than by dropping work in flight. A finalized intent, or one that expired
without finalizing, is pruned once the retention window has passed since its last transition, which returns its slot to
the authority's ceiling.

**Partition behavior.** Admission may retain bytes and intent state during a home outage, but it never publishes
metadata or changes home authority: those run downstream once the home is reachable. A datacenter in a control-plane
minority therefore keeps admitting and retaining eligible uploads durably near the client, and the retained intents
drain at the new home once [authority transfers](@/core/availability-authority-transfer.md) to a survivor.

**Finalization.** Admission stages the upload; the authority's home datacenter turns it into a release. That step
validates the intent against current state and commits the metadata and its outbox entry in one transaction. See
[finalizing admitted uploads](@/core/availability-finalization.md) for the fence, the validation checks, and how a retry
replays one result.

## Project size quota

An index policy can set `max_project_size_bytes` for hosted PyPI uploads. The limit counts each distribution file's
logical size under its normalized project. Files that existed before quota accounting was enabled are not backfilled;
the counter starts at zero and grows through metered uploads.

| Situation                                      | Status | Result                                                      |
| ---------------------------------------------- | ------ | ----------------------------------------------------------- |
| Projected counted bytes are within the limit   | `200`  | File and reservation commit together                        |
| Projected counted bytes exceed the limit       | `403`  | Rule `max-project-size`; no file metadata publishes         |
| The policy enables `quota_audit`               | `200`  | File publishes and the durable reservation records the hit  |
| Validation, storage, metadata, or status fails | varies | No file publishes and reserved project bytes return to zero |
| The request is cancelled or disconnected       | varies | Pending project bytes are released                          |
| The filename already holds the same content    | `200`  | No new allocation                                           |

The denial reason remains `project size <total> would exceed limit <limit>`. It is returned as the existing JSON policy
denial with action `upload`, field `project_size`, and rule `max-project-size`. Lowering a limit below current counted
use rejects later counted uploads; project pages and file downloads remain available.

A virtual upload route and its hosted target can both configure the limit. The lower value applies. When both set a
limit, audit behavior requires `quota_audit = true` on both; either enforcing layer makes the combined decision enforce.
Quota decisions increment `peryx_pypi_quota_admitted_total` or `peryx_pypi_quota_rejected_total` under the hosted role.
The metric series contain no project names.

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

### Requiring predicate types

An index whose `[index.policy]` sets `required_attestations` makes an upload carry a PEP 740 attestation for every
listed in-toto predicate type. peryx evaluates this after the structural, digest, and attestation-binding checks above
and after the neutral project, size, and tag rules, so a file rejected on one of those reports that denial first. The
requirement matches predicate types verbatim against the `predicateType` each bound attestation declares; it verifies no
signature, certificate, or transparency-log entry.

| Situation                                             | Status | Rule                         |
| ----------------------------------------------------- | ------ | ---------------------------- |
| Every required predicate type is present              | passes | none                         |
| A required predicate type is missing (`enforce` mode) | `403`  | `required-attestation`       |
| A required predicate type is missing (`audit` mode)   | passes | `required-attestation-audit` |

In `enforce` mode the `403` body is a policy denial whose `reason` names the missing types, for example
`upload is missing a required attestation predicate type: https://docs.pypi.org/attestations/publish/v1`, without
echoing bundle content. In `audit` mode the upload publishes and peryx records the `required-attestation-audit` decision
instead of rejecting. Either way the decision is persisted to the policy-decision log. An upload with no attestations
satisfies no requirement.

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
