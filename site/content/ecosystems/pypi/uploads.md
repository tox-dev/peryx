+++
title = "Upload compatibility"
description = "Wheel names, declared digests, equivalent versions, mutation paths, provenance, and durability rules."
weight = 5
aliases = [ "/ecosystems/pypi/dist-info/", "/ecosystems/pypi/upload-digests/", "/ecosystems/pypi/version-match/", "/ecosystems/pypi/reserved-names/"]
+++

An upload accepted by [Warehouse](https://pypi.org/) should also work through peryx. Warehouse and pip resolve a project
and release by normalized identity instead of requiring a canonical spelling for each field:
[PEP 503](https://peps.python.org/pep-0503/) on the name and [PEP 440](https://peps.python.org/pep-0440/) on the
version. peryx applies the same rule to wheel names, declared digests, version-scoped mutations, and verb-named
projects. It still rejects inputs that identify a different project, release, or artifact.

## Durability acknowledgement

A successful upload response follows the evidence the current handler collects. Under `availability.mode = "none"`,
peryx answers `200` after the local backend commits the artifact and metadata. Under `dc`, metadata commits on the
writer while `[availability.write_ack]` controls how many same-datacenter member receipts must report the blob. Under
`ha`, the resolver also waits for the metadata operation to apply in the share of the remote datacenters the same policy
names: one under `local`, a strict majority of them under `majority`, and all of them under `everywhere`. The current
release has no supported network layout for all HA peer routes; see the
[release-status inventory](@/core/availability/_index.md#release-status).

The response follows the evidence. A write that reaches its quorum answers `200 upload accepted`. A write still short of
its quorum when the client's `write_ack.deadline-secs` window elapses answers `202 Accepted` carrying a stable operation
id, not a failure: the durable completion may land after the client stops waiting, so reporting a definite failure would
be wrong and a blind retry could publish the artifact twice. The client resends the same upload, and peryx resolves the
original operation by its id, replaying the recorded result rather than mutating a second time. An identical resend of
an already-published file is the same idempotent success it has always been.

Every `dc` and `ha` node serves `GET /+replication/v1/receipts/sha256/{digest}` on its public server. Service assembly
builds an HTTP receipt client for each other member in the local datacenter and polls those clients during the upload
deadline. The route requires the replication bearer token; `200` names the serving node, the digest, and the stored byte
length, `404` reports no local blob, and malformed digests return `400`. This peer route is internal and does not appear
in the public OpenAPI document.

Each client counts an answer only when the node, digest, and size are the ones it asked for, so two configured addresses
that reach one process contribute one copy instead of two. The resolver also uses this node-receipt model for object
stores instead of carrying backend-specific commit evidence, so a `dc` filesystem roster still wants each member address
on its own storage failure domain. Cross-datacenter artifact replication runs after publication; the upload does not
wait for those bytes.

Filesystem persistence also ignores a parent-directory sync failure before a receipt can be served. On a filesystem
where that sync is needed for crash durability, a receipt can overstate what survives a crash.

The scheduled crash-recovery finalizer follows a different evidence path. When it finds a pending intent whose local
file rows and placement exist, it can record `published` without calling the distributed acknowledgement resolver. A
later retry can replay `200 upload accepted` without the DC receipts the synchronous request path waits for.

## Project size under concurrent uploads

`max_project_size_bytes` applies to the sum of counted distribution files for one normalized project. peryx reserves an
upload's bytes before durable blob storage, then commits those bytes in the transaction that publishes the filename.
Concurrent uploads therefore compete for the same remaining capacity; only complete files that fit become visible.

This differs from reading every file size and adding the incoming body before publication. That check grows with the
project's release history, and two requests can both observe the same free space. The reservation counter stays constant
cost as releases accumulate and includes capacity held by uploads that have not published yet.

A failed or cancelled request releases its reservation. A same-content re-upload of an existing filename stays an
idempotent success and does not consume more project bytes. `quota_audit = true` accepts a would-reject upload while
recording the violation, which lets an operator observe the limit before enforcing it. See the
[quota settings](@/ecosystems/pypi/reference/policy.md#upload-quotas) and the
[exact response contract](@/ecosystems/pypi/reference/uploads.md#project-size-quota).

## Un-normalized wheels

peryx accepts a wheel whose internal `.dist-info` directory is not spelled the modern, normalized way, as long as it
names the same project and version as the filename. The check compares normalized identity rather than exact bytes,
which accepts historical artifacts without weakening identity checks.

### Wheel normalization compatibility

A wheel's layout is `{name}-{version}.dist-info/`, and the filename is `{name}-{version}-{tags}.whl`. For years the two
`{name}` fields were written however the build tool spelled the project: `Flask-0.12-py2.py3-none-any.whl` shipped a
`Flask-0.12.dist-info` directory, mixed case and all. Only later did the ecosystem settle on
[PEP 503](https://peps.python.org/pep-0503/) normalization, which lowercases the name and folds every run of `-`, `_`,
and `.` to a single `-`, and current build backends write the directory that way. The wheels built before that
convention did not vanish; they are still on PyPI, and installers still install them.

pip and [Warehouse](https://pypi.org/) (pypi.org) never demanded a byte-exact directory. They compare the directory's
project name and version to the filename's after normalizing both, so `Flask-0.12.dist-info` satisfies a `flask-0.12`
filename. peryx now does the same: PEP 503 on the name, [PEP 440](https://peps.python.org/pep-0440/) parsing on the
version. The [reference](@/ecosystems/pypi/reference/uploads.md#wheel-dist-info-matching) states the exact comparison.

### Rejected wheel mismatches

peryx used to build the expected directory name from the filename and require the archive to contain that exact string.
An older wheel whose directory read `Flask-0.12.dist-info` was measured against the computed `flask-0.12.dist-info` and
rejected on upload with `.dist-info directory ... does not match expected ...`, even though the two name the same
release.

That made peryx stricter than the index it stands in front of, and the gap bit where peryx is meant to disappear:

- **Mirroring.** A cached index that pulls a historical wheel from pypi.org, or a migration that re-uploads an
  organization's back catalogue into a hosted index, carries whatever `.dist-info` spelling the original build wrote. A
  file pip installs from pypi.org could not be served through peryx.
- **Re-uploading.** A team moving a private index onto peryx, or restoring from a backup of older builds, hit the same
  wall for artifacts they had shipped for years.

Refusing a wheel that pypi.org accepts breaks the drop-in promise. The index in front of PyPI should take every file
PyPI would. Matching by normalized identity closes that gap while keeping the guarantee that matters. The metadata
inside the wheel belongs to the project and version on the label.

### Strict wheel checks

Normalization does not loosen the comparison. A directory whose normalized name or parsed version differs from the
filename is still rejected, as is an archive with no `.dist-info` directory or more than one. peryx accepts a different
spelling of the same identity, matching pip and Warehouse, not leniency past them.

## MD5 on upload

peryx accepts an upload that declares only a legacy `md5_digest`, verifies it, and then never mentions MD5 again. An
index built around SHA-256 still takes an MD5-only upload, does not bother computing MD5 when a stronger digest is
already declared, and serves back a file that carries no MD5 at all.

### MD5 compatibility

[Warehouse](https://pypi.org/) accepts `md5_digest` on its upload API. Clients and CI have declared MD5 to PyPI for
years: some older tooling sends only `md5_digest`, and hand-rolled upload scripts often compute the one hash that ships
in the Python standard library without a thought about which. An index that rejects those uploads is stricter than the
one it emulates, and the gap shows up exactly where peryx is meant to disappear: a `twine upload` or a mirrored publish
that succeeds against pypi.org fails against peryx.

peryx used to reject an MD5-only upload outright, even with a correct digest, because it never computed MD5 and so had
nothing to check the declared value against. It now computes MD5 over the staged content when that is the only digest
the client declared, verifies it, and stores the file. A correct `md5_digest` is accepted; a wrong one is rejected with
`md5_digest mismatch`, the same way a wrong SHA-256 is. The behavior matches Warehouse, so an upload that works against
pypi.org works against peryx.

### Stronger-digest precedence

peryx already hashes every upload with SHA-256, which is how it content-addresses the file, and with BLAKE2b-256, both
computed in one pass as it reads the stream. Verifying a declared `sha256_digest` or `blake2_256_digest` is then a
comparison against a hash it already holds.

Computing MD5 is different: peryx does not need MD5 for anything else, so producing it means reading the staged content
a second time. When the client declared a `sha256_digest` or `blake2_256_digest`, verifying that digest already proves
the bytes are the ones the client sent. A matching MD5 on top would add no assurance, and MD5 is the weaker hash of the
set, so re-reading the file to check it would be work spent to confirm something already confirmed. peryx verifies MD5
only when it is the sole digest on offer, which is the one case where skipping it would leave the upload unchecked.

### Served digests

Accepting MD5 on upload does not make peryx an MD5 index. The simple-index entry for a stored file carries a `sha256`
hash and nothing else, and that is the hash every installer uses to verify what it downloaded. MD5 has been broken
against collision attacks for years; re-publishing it as a content hash would advertise a guarantee peryx will not stand
behind. SHA-256 supersedes it for that job, peryx computes SHA-256 for every file regardless of what the uploader
declared, and that is the digest it serves.

So MD5 lives entirely at the upload boundary. peryx accepts it because Warehouse does, verifies it when nothing stronger
was declared, and drops it the moment the file is stored.

## Equivalent version spellings

A release has more than one spelling. `1.0`, `1.0.0`, and `1.0.0.0` are one version under
[PEP 440](https://peps.python.org/pep-0440/), and every resolver, pip, uv, and pypi.org itself, treats them as one.
peryx serves them as one: a project page filtered to `1.0.0` shows a file whose form version was `1.0`. The
version-scoped admin operations, yank, delete, and promote, have to reach the same file, or they act on a release that
looks different from the one the page shows. They match by PEP 440 equality, and that is why.

### Two ways to compare a version

An upload records the version spelling supplied by the build tool. `1.0` and `1.0.0` identify the same release, and
their files appear together on the project page. When an operator addresses `yank 1.0.0`, two checks decide whether a
file belongs to that request, and they can disagree.

- The **served page** filters by PEP 440 equality. Ask for `1.0.0` and it returns every file of that release, `1.0` and
  `1.0.0.0` included, because that is what a release means to an installer.
- A **byte-exact** match compares the two strings. `1.0.0` does not equal `1.0`, so a file uploaded as `1.0` falls
  outside a request addressed to `1.0.0`.

While the served page used one rule and the mutations used the other, the operator saw one release and the operation
acted on another.

### Rejected digest mismatches

peryx used to compare an upload's version to the requested version byte for byte inside yank, delete, and promote. A
release published as `1.0` was invisible to any request that spelled it another way:

- **Yank did nothing.** `PUT /root/pypi/mypkg/1.0.0/yank` on a file uploaded as `1.0` matched no file, reported zero
  files changed, and left the release live. The operator, reading `1.0.0` off the project page, had every reason to
  think the yank landed, and no sign that it had not.
- **Delete left the file up.** The same mismatch on a delete answered "nothing matched" while the file kept serving.
  Worse, delete falls back to matching on the stored record exactly when the served-page filter finds nothing, so the
  two version notions had to agree or the fallback missed too, in the one place it exists to catch the file.
- **Promote skipped the release.** A promote from a staging route to a release route stepped over a file whose spelling
  did not match, and shipped an incomplete release without saying so.

Each of these fails without a sign. The request succeeds, the count comes back zero, and the file stays as it was. An
operator learns the yank did not take only when a resolver installs the version they thought they had pulled.

### Version equality

The operations route their version comparison through the same PEP 440 equality the served page uses, with a fall back
to byte comparison when a version does not parse. Addressing any spelling of a release now reaches every file of that
release, and the operation acts on the set of files the page shows for that version. The two sides of peryx, the page a
client reads and the mutation an operator runs, share one definition of what a release is.

### Strict version checks

Equality is one release, not a loose match. A request for `1.0` reaches `1.0.0` but never `1.0.1` or `1.1`; those are
different releases and stay untouched. The [local segment](https://peps.python.org/pep-0440/#local-version-identifiers)
counts: `1.0+build` and `1.0` are distinct versions and do not match. And a version that is not valid PEP 440 is
compared by its exact spelling, so a non-standard tag matches only itself. peryx reaches every spelling of the right
release; it does not reach the wrong one.

## Verb-named projects

peryx serves three mutations, yank and restore and promote, and names each one in the URL that performs it. A project
whose name is one of those words is a legal package, and peryx addresses it like any other. The name and the verb must
not share fate, or a delete becomes impossible.

### peryx does not reserve names

peryx is a private index and a mirror. It hosts whatever [PEP 503](https://peps.python.org/pep-0503/)-legal name you
push and whatever a cached upstream carries, and it keeps no list of prohibited or reserved project names. Blocking
names is a public-registry concern: pypi.org withholds some names to keep an open, shared namespace legible and to blunt
squatting. Inside your own index the namespace is yours, and `yank` is as valid a project as `requests`.

What the old router did have was an *accidental* reservation. The mutation URLs reuse the verbs as path segments, and
the routing peeled a trailing `yank`, `restore`, or `promote` off the path before it read the project name. When the
verb was the entire path, nothing was left to name the project, so the three verbs went missing as project-addressable
names, a side effect of the grammar rather than a rule anyone wrote.

### Rejected mutation paths

`DELETE /root/pypi/yank/` deletes the project `yank`. The old router read the trailing `yank` as the un-yank action,
looked for a project name in front of it, found none, and rejected the request with `400 Bad Request`. A project named
`yank` could be uploaded and installed but never deleted at the project level: its name shadowed the delete.

`yank` and `restore` are real projects on pypi.org, so the collision was reachable. It bit where peryx is meant to
disappear:

- **Mirroring.** A cached index pulls a project named `yank` from pypi.org into a virtual index, and the operator cannot
  later remove it from the hosted layer.
- **Migrating.** A team moving a back catalogue onto peryx re-uploads a package named `restore`, then finds it stuck: no
  project-level delete to undo a mistaken import.

An index that cannot delete a project it accepted is not a drop-in front for one that can.

### Accepted project names

peryx separates the two namespaces by position, not by forbidding the name. A trailing verb is an action only when a
project segment precedes it; a path that is nothing but the verb names the project. `DELETE /root/pypi/yank/` deletes
`yank`, `PUT /root/pypi/yank/yank` yanks it, and the versioned and normal project-level forms are unchanged. The
[reference](@/ecosystems/pypi/reference/uploads.md#mutation-paths-for-verb-named-projects) lists every path.

This does not loosen anything. A real yank still needs its `.../yank` suffix behind a project, a real delete still needs
the token and a volatile hosted layer. peryx stopped treating a lone verb as an action; it did not stop treating a
suffixed verb as one.

## Hosted attestations

peryx accepts [PEP 740](https://peps.python.org/pep-0740/) attestations attached to a hosted upload, binds each one to
the distribution it rode in with, stores the bundle, and serves it back as a provenance object on the Simple API. A
publisher that attaches attestations to a `twine upload` against pypi.org attaches them the same way against peryx.

### Provenance binding

peryx does not verify Sigstore signatures, certificate identities, or transparency-log inclusion; those are the
consumer's to check, and a build-service identity policy is out of scope. What peryx enforces is the binding a consumer
relies on before it ever looks at a signature: every attestation must name *this* distribution. Each attestation's
in-toto subject has to carry the uploaded file's SHA-256 digest, and if it names a filename, that filename has to be the
one being uploaded. An attestation whose subject digest is for some other file, or whose subject names a different
wheel, is a bundle issued for a different artifact, and peryx rejects it.

Storing the attestation next to the file lets a client confirm that the provenance describes the downloaded bytes. peryx
keeps the certificate chain, log proofs, and predicate intact for the verifier.

### Publish is all-or-nothing

The attestation and the distribution publish in one transaction, so a bad bundle takes both down with it. A subject
mismatch, a malformed envelope, a statement that is not valid base64 or not a valid in-toto statement, an unsupported
version, a bundle nested past the JSON parser's depth limit, or a field over its size limit. Any of these fails the
upload with a `400`, and neither the file nor its provenance becomes visible. There is no half-published state where the
wheel is installable but its provenance is missing, or the reverse.

### Visibility and provenance

The provenance follows the distribution's visibility, because it is only ever reachable through the file's advertised
provenance URL. Yank a release and its files stay on the page (marked yanked) with their provenance URLs intact. Trash a
file and it drops off every served page with its provenance link. Restore it and both return. The provenance blob is
kept through the trash, keyed by the artifact's own digest, so a restore never has to re-derive it. The association a
client sees always matches what the index is willing to serve.

### Requiring an attestation

`required_attestations` on an index's `[index.policy]` table lists the [in-toto](https://slsa.dev/spec/v1.0/provenance)
predicate types an upload must carry a PEP 740 attestation for, such as `https://docs.pypi.org/attestations/publish/v1`
from the
[PyPA index-hosted attestation specification](https://packaging.python.org/en/latest/specifications/index-hosted-attestations/).
peryx evaluates the requirement at the upload boundary, after the same structural and subject-binding validation above
and before the file and its provenance publish. An upload missing a required predicate type publishes neither object,
and the `403` names the missing types without echoing bundle content. Matching predicates permit the same upload. The
requirement reads the predicate types declared by the bound attestations. A later verifier checks signatures,
certificates, transparency logs, and identity claims.

`attestation_mode` chooses what an unmet requirement does. `enforce`, the default, rejects the upload. `audit` records
the same policy decision but publishes the upload anyway, so an operator can measure how much of a project's traffic
already ships attestations before turning enforcement on. Both modes persist the decision, so the audit trail shows what
enforcement would have rejected.

### Threat model

Untrusted input arrives in the `attestations` field: attacker-controlled JSON, an attacker-chosen certificate, an
attacker-written predicate. peryx bounds it before it parses it (aggregate field size, per-attestation size, statement
size, parser depth) so a hostile bundle cannot exhaust memory or stack. It never interprets the predicate or the
verification material; it stores them as opaque bytes and serves them only inside the JSON provenance body, where
metacharacters stay inert string data. The provenance URL is peryx's own digest-addressed route, so nothing an uploader
controls reaches the HTML page except through that fixed, escaped attribute. peryx holds no signing material at any
point; it stores and serves what a publisher signed elsewhere. `required_attestations` reads only the predicate type a
bound attestation declares, so it raises no new trust in the bundle: a wrong-subject or malformed bundle is already
rejected before the requirement is judged, and a matching upload still gets no signature or identity guarantee it did
not earn.

## Controls route through the project's authority

A yank, unyank, delete, restore, or promote changes what a project serves, so peryx routes each one through the
project's home authority the way a first publish routes there. The normalized project name is the authority key, so
every PEP 503 spelling of a name fences on one epoch, and a control accepted at one ingress is the same control at any
other.

A control leases the authority's committed epoch when it starts, resolves the files it will touch, then re-admits that
epoch before it writes. A control whose authority did not move commits under the epoch it leased. One whose home
transferred while it ran leased a superseded epoch, so peryx rejects it with `409 Conflict` and writes nothing: a former
home cannot change a serial or a file's visibility after the project has moved on. The rejection carries a retry hint
without topology details, so a protocol client learns to retry without learning the cluster's shape.

### Retrying a fenced control

Reissue the same request. The retry leases the current epoch and, once the transfer has settled, commits against the new
home. A second yank of an already-yanked file and a second delete of an already-trashed file are idempotent. A retry
after an ambiguous failure therefore converges on one serial ordering and one visible result.

### Mode `none` and ungrouped deployments

A process that runs no ownership group holds no epoch, and a project a group has not homed yet reads the unassigned
sentinel. Both admit every control unfenced. Mode `none` therefore pays no authority-fencing cost; a distributed group
starts fencing after it commits an authority home.

## Operational checks

- The exact accept and reject rules, tables, and error strings: [upload rules](@/ecosystems/pypi/reference/uploads.md)
- The full set of upload checks: [publish packages](@/ecosystems/pypi/guides/publish.md)
- Publish a wheel built by older tooling, or with a single legacy digest, and target a release by any equivalent
  spelling: [publish packages](@/ecosystems/pypi/guides/publish.md) and
  [yank and delete packages](@/ecosystems/pypi/guides/remove.md)
- Walk an upload of a historical wheel, an MD5-only client, an equivalent-version yank, and a verb-named project end to
  end: [publish and manage a release](@/ecosystems/pypi/tutorials/publish-and-manage.md)
