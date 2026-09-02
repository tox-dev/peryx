+++
title = "Cache inspection API"
description = "Inspect cache contents without stopping the server."
weight = 11
+++

`peryx serve` keeps the metadata store open for its process lifetime. redb refuses a second open while that writer
exists, so the offline [`peryx cache`](@/core/operations/cli.md) commands need the server to stop first. The cache
inspection API reads through the server's existing handle.

Each endpoint requires `administration:read` and a local administrator credential. They return
`text/plain; charset=utf-8` with the same tab-separated rows as the matching offline command.

`GET /+cache` matches `peryx cache list`. `GET /+cache/size` matches `peryx cache size`, and `GET /+cache/fsck` matches
`peryx cache fsck`. There is no endpoint for `peryx cache repair`: these endpoints read, and a rebuild writes.

`GET /+cache` accepts the offline list filters as query parameters: `index`, `resource`, `digest`, `stale`,
`min_age_secs`, and `min_size_bytes`.

```shell
curl --user administrator:password 'https://packages.example/+cache?index=pypi&stale=true'
curl --user administrator:password https://packages.example/+cache/size
curl --user administrator:password https://packages.example/+cache/fsck
```

The handlers run filesystem scans outside the async request executor. Clients can write cache entries during an
inspection, so one report can span two committed states.

Operators can use the offline commands when a server cannot start. Those commands do not build a serving state and
require exclusive access to the metadata store.
