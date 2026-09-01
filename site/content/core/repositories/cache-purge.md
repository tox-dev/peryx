+++
title = "Cache purge API"
description = "Remove one cached resource, or preview that removal, without stopping the server."
weight = 12
+++

A cached repository holds a copy of each resource it has served: the upstream page, the resource record, and the file
and metadata rows that page registered. `POST /+cache/purge` removes one resource's copy from a running server.

The [`peryx cache purge`](@/core/operations/cli.md) command does the same removal offline. It cannot be used instead:
every offline `cache` subcommand opens the metadata store, the store admits one holder at a time, and `serve` is that
holder for its whole process lifetime. The dry run is no exception, because a read-only handle is refused for the same
reason a writable one is. Removing a resource through the CLI therefore means downtime; this endpoint is how an operator
removes one without it. Use the [cache inspection API](@/core/repositories/cache-inspection.md) for read-only commands.

## Request

```json
{
  "repository": "pypi",
  "resource": "Flask",
  "apply": true
}
```

`repository` is the route of a configured repository. `resource` is normalized by the ecosystem, so `Flask` and `flask`
name the same PyPI project and the response reports the normalized form.

`apply` defaults to `false`, which counts the records a purge would remove and changes nothing. A preview requires
`administration:read`; confirming requires `administration:write`. A caller who holds neither, or who names a repository
they cannot see, receives `404` so the failure cannot be used to infer which repositories exist.

## Response

```json
{
  "repository": "pypi",
  "resource": "flask",
  "applied": true,
  "removed": {
    "file_url_records": 12,
    "index_pages": 1,
    "metadata_records": 4,
    "project_records": 1
  }
}
```

The `removed` categories are the ecosystem's own record classes. Blob files are not among them: a purge never deletes
content another resource or a hosted upload still references, and unreferenced files are reclaimed separately by
`peryx cache purge orphaned-blobs`.

## Concurrency

The removal runs as the holder of the resource's single-flight gate, the same one every writer of that resource's cached
page joins before it reaches upstream. Two things follow.

A refresh already fetching the resource publishes first, and the purge then removes what it published. The counts in
`removed` are the rows that actually went, not the rows that were there when the request arrived.

A refresh that arrives while the purge holds the gate re-reads the resource afterwards. Finding it gone, it skips rather
than republishing the page it was about to store, so a background sweep cannot undo an operator's removal. Purges of
different resources hold different gates and do not wait on each other.

The purged resource is served again from upstream on the next request that asks for it. A purge removes a copy; it does
not block a resource. Use a [repository policy](@/core/policy-data/_index.md) to stop serving one.
