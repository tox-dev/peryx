+++
title = "Repository quotas"
description = "Reserve repository capacity and recover pending writes with durable accounting."
weight = 9
+++

Repository quotas account for content before a writer publishes package metadata. The storage API reserves capacity,
then the caller commits the reservation with its metadata transaction or releases it after an error. The PyPI and OCI
drivers expose identity constructors for this API. This release ships the accounting substrate: it reserves, commits,
and releases capacity but does not yet read limits from configuration or enforce them in either protocol handler. The
upload paths adopt these APIs in later work.

## Limits

Each reservation admits against a `QuotaLimits` set the caller supplies. Every limit is optional and unset means
unlimited:

| Limit                      | Bounds                                                |
| -------------------------- | ----------------------------------------------------- |
| `max_file_bytes`           | The logical size of one artifact                      |
| `max_accounted_bytes`      | Deduplicated bytes stored in the repository           |
| `max_projects`             | Distinct project identities in the repository         |
| `max_versions_per_project` | Versions within a single project                      |
| `audit`                    | Records a would-reject decision instead of denying it |

`max_file_bytes` bounds one artifact, so keep it within the
[S3 object limit](https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html) of 50,000,000,000,000 bytes; a
reservation an object store cannot hold serves no one. The remaining limits bound repository totals. A repository that
owns no content, such as a virtual index that layers others, holds nothing to account and needs no limits.

## Byte accounting

Each allocation belongs to one accounting class:

| Class       | Content                                                                  |
| ----------- | ------------------------------------------------------------------------ |
| `hosted`    | Content accepted from a package or image publisher                       |
| `cached`    | Content fetched from an upstream package index or registry               |
| `generated` | Content that Peryx derives and stores, such as a metadata representation |
| `trash`     | Soft-deleted content retained for restore or later purge                 |

All four classes consume quota. Moving content to trash does not free capacity; deletion or purge releases its
allocation.

`file_bytes` counts the logical size of each allocation. `accounted_bytes` charges a digest once per repository, so two
files in one repository that reference the same digest add two logical sizes and one accounted size. The same digest in
two repositories consumes capacity in both repositories. Peryx rejects a second size for a digest that the repository
already accounts for.

Project and version counters use reference counts. The first allocation for a project consumes one project slot, and the
first allocation for a version consumes one slot in that project. Peryx frees each slot after the last allocation leaves
it.

## Reservation lifecycle

A reservation starts in `reserved` state. Peryx checks the requested file size and the projected repository counters in
the same serialized metadata transaction that increments reserved counters. Parallel writers near a limit cannot both
claim the last capacity.

The caller then takes one of these actions:

- Commit the reservation with the driver metadata write. Peryx moves each counter from `reserved` to `committed` in the
  same transaction and retains the allocation record for deletion.
- Release the reservation after an interrupted or rejected write. Peryx decrements the matching counters and removes the
  allocation record.

Commit and release operations use a stable reservation UUID. A second commit or release changes no counters. A failed
driver transaction leaves its reservation pending, and a quota finalization failure rolls back the driver rows.

`audit = true` records the limits that would reject a request and admits its reservation. The durable allocation record
stores those violations for inspection. Audit mode still updates reserved and committed counters, which lets operators
observe projected enforcement against real write traffic.

## Restart repair

An interrupted process can leave reserved allocations with no live writer. After restart, call the bounded repair API
with a row limit until it reports no remaining work. Each pass reads at most one more pending entry than the requested
limit, releases at most that limit, and leaves committed allocations intact. Repair runs outside request execution.

Peryx keeps a separate pending-reservation index, so retained committed history does not increase repair scan work. A
repair pass uses memory proportional to its row limit and commits its counter changes once.

## Migration and observability

`MetaStore::open` creates missing quota tables and the pending index in its metadata transaction. Peryx does not scan or
backfill existing blobs during this migration; counters start at zero and grow when callers reserve new allocations.
File-level backups contain the quota tables with the rest of the metadata store.

The stored counters form the quota observability contract. Repository usage reports committed and reserved values for
logical file bytes and accounted bytes, plus project counts. Project usage reports committed and reserved version
counts. The reservation record identifies its class and state. It stores the creation time, the digest and size, and any
audit violations.

Peryx does not export repository or project names as Prometheus labels. Such labels would create an unbounded metric
cardinality and duplicate the durable counters. Management views and retention planning remain outside repository quota
accounting. Billing and per-user limits also remain outside. Peryx does not allocate cost across repositories.
