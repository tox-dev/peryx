+++
title = "PyPI changelog compatibility"
description = "The Warehouse XML-RPC routes, journal contract, and mirror recovery procedure."
weight = 8
+++

Warehouse's mirroring API accepts XML-RPC `POST` requests at `/pypi`, `/pypi/`, and `/RPC2`. peryx supports the two
methods that mirror clients use to consume its ordered hosted-index changelog:

- `changelog_last_serial()` returns the newest local journal serial.
- `changelog_since_serial(serial)` returns no more than 50,000 rows in `(name, version, timestamp, action, serial)`
  order. Results include records whose serial is greater than the argument and stay sorted by serial.

The protocol codec accepts Warehouse's `int`, `i4`, and `i8` serial forms. It emits `i8` when a peryx serial no longer
fits an XML-RPC `int`, represents an absent version with `nil`, strips XML control characters, and escapes text fields.
Malformed calls return an XML-RPC fault instead of a partial result. peryx limits request bodies to 64 KiB before XML
parsing. Multipart requests on the same paths remain uploads to the configured index, so an index may still use `pypi`
as its route. Changelog calls use the listing rate limit rather than consuming the upload budget.

The feed spans every hosted PyPI index because its serial domain is process-wide. A request must have catalog-wide read
access to each hosted PyPI index: anonymous access is enough for an index with `anonymous_read = true`; a protected
index requires a live credential with a `read` grant for `*`. This prevents one global response from disclosing a
private index's project names.

## Storage contract

The endpoint must read the journal key as the authoritative serial; the placeholder inside the driver payload is not a
serial. Each record also needs its mutation timestamp. The journal write must commit that timestamp; readers must not
reconstruct it.

Local actions use Warehouse's human-readable form while retaining peryx's per-file precision:

| Mutation                   | Changelog action                          |
| -------------------------- | ----------------------------------------- |
| Upload or promotion        | `add file <filename>`                     |
| Soft or permanent deletion | `remove file <filename>`                  |
| Yank or unyank             | `yank <filename>` or `unyank <filename>`  |
| Hide or restore            | `hide <filename>` or `restore <filename>` |

Each promotion record identifies one changed file rather than emitting an ambiguous project event. Each new record
carries the release version and mutation time. Records written before timestamps were introduced remain readable and
report Unix epoch `0`; clients must treat that value as unknown historical time rather than the real event time.

The endpoint exposes hosted-index mutations. Each response layer contributes a serial and its journal domain. Layers in
one domain compose to their lowest serial. Clients can treat that value as the newest serial present in every layer. A
missing serial or mixed domains produce no scalar. A local-plus-upstream overlay omits its serial until peryx projects
upstream events into the local journal.

## Client compatibility

Warehouse limits each changelog call to 50,000 rows, so clients advance from the highest returned serial and request the
next page. The endpoint must read each page from one metadata snapshot; otherwise a concurrent write could move the
reported end past an entry that the page lacks.

The internal page contract enforces the same limit, exclusive cursor, strict serial order, and snapshot upper bound. An
empty page resumes from the greater of the request cursor and snapshot serial, so a client asking beyond the current
head does not move backward.

Once peryx caches an upstream serial, a refresh must return the same or a higher value. A response with a lower serial,
or no serial, may be a stale CDN object and cannot replace the cached page. This matches the consistency checks in
[bandersnatch](https://github.com/pypa/bandersnatch/blob/main/src/bandersnatch/master.py) and
[devpi](https://github.com/devpi/devpi/blob/main/server/devpi_server/mirror.py).

If a mirror loses its cursor or cannot trust its local state, it should read `changelog_last_serial` first, rebuild from
the Simple API, then request changes after the captured serial. The mirror then replays mutations concurrent with the
rebuild. It may replay a mutation already reflected by the crawl once, so updates must remain idempotent. A storage
failure returns Warehouse's opaque `-32403` fault and must not be treated as an empty page.

Current bandersnatch releases discover changes from the [PEP 691](https://peps.python.org/pep-0691/) root project list
and its per-project `_last-serial` extension. The XML-RPC endpoint supports older mirror clients, while current
bandersnatch compatibility also requires those project serials in `/simple/` JSON. The tuple and 50,000-row behavior
follow [Warehouse's implementation](https://github.com/pypi/warehouse/blob/main/warehouse/legacy/api/xmlrpc/views.py).
