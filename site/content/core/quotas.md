+++
title = "Repository quotas"
description = "Reserve repository capacity and repair pending reservations with durable accounting."
weight = 9
+++

Repository quotas reserve capacity before an ecosystem driver publishes metadata. The driver commits the reservation
with its metadata transaction or releases it after a failed write. Repositories without quotas use the direct write
path.

## Limits

Each reservation checks an optional `QuotaLimits` value:

| Limit                      | Bounds                                      |
| -------------------------- | ------------------------------------------- |
| `max_file_bytes`           | Logical size of one content item            |
| `max_accounted_bytes`      | Deduplicated bytes charged to a repository  |
| `max_projects`             | Distinct driver subjects                    |
| `max_versions_per_project` | Versions associated with one subject        |
| `audit`                    | Records a denial without refusing the write |

An unset limit has no bound. A virtual repository owns no content and does not need storage limits.

## Accounting classes

| Class       | Content                                   |
| ----------- | ----------------------------------------- |
| `hosted`    | Accepted from a publisher                 |
| `cached`    | Fetched from an upstream repository       |
| `generated` | Derived and stored by an ecosystem driver |
| `trash`     | Retained for restore or purge             |

All classes consume quota. Moving content to trash retains its allocation until purge or deletion.

`file_bytes` adds the logical size of each allocation. `accounted_bytes` charges one digest once per repository. Two
logical files in one repository can share one accounted digest. The same digest in two repositories consumes capacity in
both. A repository cannot record two sizes for one digest.

Drivers can track logical bytes by subject. Subject and version counters use reference counts; the first allocation
takes a slot, and releasing the last allocation frees it.

## Reservation lifecycle

A new reservation has state `reserved`. One serialized metadata transaction checks projected counters and increments
reserved capacity. Parallel writers cannot both claim the last available capacity.

The caller then commits or releases the reservation:

- Commit moves counters from `reserved` to `committed` in the driver metadata transaction and retains the allocation
  record for deletion.
- Release decrements reserved counters and removes the allocation record after an interrupted or refused write.

A stable UUID makes commit and release idempotent. A failed driver transaction leaves a pending reservation. A quota
finalization error rolls back the driver metadata rows.

`audit = true` admits the reservation and stores the limits that would have refused it. Reserved and committed counters
still change, so operators can measure a proposed quota against write traffic.

Protocol limits and denial responses belong to the ecosystem drivers:

- [Python package quotas](@/ecosystems/pypi/reference/policy.md#upload-quotas)
- [OCI quotas](@/ecosystems/oci/reference/policy.md#push-quotas)

## Restart repair

An interrupted process can leave reservations without a live writer. The repair API accepts a row limit and releases a
bounded number of pending entries per pass. It leaves committed allocations intact and reports whether more work
remains.

A separate pending index keeps repair work independent of committed history. One repair pass uses memory proportional to
its row limit and commits its counter changes together.

## Status API

`GET /+quota` returns repository summaries for a local administrator. `GET /+quota/repository?repository=<name>` returns
one repository to a principal with read access. The operator role without repository access cannot read either route.

Responses use `private, no-cache` under [RFC 9111](https://www.rfc-editor.org/rfc/rfc9111). Summary pagination follows
the configured repository order and uses an opaque cursor. Responses omit subject and artifact detail.

Each repository reports `limits` and meters for `file_bytes`, `accounted_bytes`, and `projects`. A meter contains
`committed`, `reserved`, `limit`, and `remaining`. An unlimited meter has null `limit` and `remaining`. `file_bytes` has
no repository-level cap, so those two fields are null for that meter.

The CLI reads the same counters:

```console
$ peryx quota list
$ peryx quota inspect --index team-hosted
```

`limits.audit` reports audit mode. Reserved capacity without an active writer indicates pending repair work.

## Migration and observability

Opening the metadata store creates missing quota tables and the pending index. It does not scan existing content, so new
counters start at zero and grow through later reservations. File-level backups include quota state.

Durable usage includes committed and reserved logical bytes, accounted bytes, subject counts, and version counts.
Allocation records contain their class, state, creation time, digest, size, subject-byte flag, and audit violations.

Prometheus metrics omit repository and subject names to keep label cardinality bounded. Billing, per-user limits,
retention planning, and cost allocation remain outside repository quota accounting.
