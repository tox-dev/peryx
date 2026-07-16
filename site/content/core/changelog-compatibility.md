+++
title = "PyPI changelog compatibility"
description = "The Warehouse XML-RPC contract and the work to expose peryx journal events."
weight = 8
+++

Warehouse's mirroring API accepts XML-RPC `POST` requests at `/pypi`, `/pypi/`, and `/RPC2`. peryx will support the two
methods that mirror clients use to consume an ordered changelog:

- `changelog_last_serial()` returns the newest local journal serial.
- `changelog_since_serial(serial)` returns no more than 50,000 rows in `(name, version, timestamp, action, serial)`
  order. Results include records whose serial is greater than the argument and stay sorted by serial.

The protocol codec accepts Warehouse's `int`, `i4`, and `i8` serial forms. It emits `i8` when a peryx serial no longer
fits an XML-RPC `int`, represents an absent version with `nil`, strips XML control characters, and escapes text fields.
Malformed calls must return an XML-RPC fault instead of a partial result.

## Storage contract

The endpoint must read the journal key as the authoritative serial; the placeholder inside the driver payload is not a
serial. Each record also needs its mutation timestamp. The journal write must commit that timestamp; readers must not
reconstruct it.

Local actions need Warehouse-compatible names. An upload becomes `add file <filename>`; deletion becomes
`remove file <filename>`; yank and unyank retain their action and filename. Promotion records must identify each changed
file rather than emit one ambiguous project event.

The first endpoint version will expose hosted-index mutations. Each response layer contributes a serial and its journal
domain. Layers in one domain compose to their lowest serial. Clients can treat that value as the newest serial present
in every layer. A missing serial or mixed domains produce no scalar. A local-plus-upstream overlay omits its serial
until peryx projects upstream events into the local journal.

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

Current bandersnatch releases discover changes from the PEP 691 root project list and its per-project `_last-serial`
extension. The XML-RPC endpoint supports older mirror clients, while bandersnatch compatibility also requires those
project serials in `/simple/` JSON.
