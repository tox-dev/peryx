+++
title = "High availability"
description = "Run one writer with read replicas and promote a replica during a planned failover."
weight = 7
+++

peryx supports one writer with multiple read replicas. Send mutation traffic to the writer. Replicas serve data copied
from the writer and reject mutation requests with `503 Service Unavailable`.

Give every writer a distinct, stable identity:

```toml
writer_identity = "writer-a"
```

At startup, the writer claims that identity in the metadata store. A writer configured with another identity cannot open
the store until an operator promotes it. This prevents a restored copy from starting as a second writer.

Enable replica mode in TOML:

```toml
read_only = true
```

A replica must retain the writer's identity in its configuration, and the copied metadata store must contain the same
claim. peryx stops startup unless that claim matches a nonblank configuration value.

```shell
PERYX_READ_ONLY=true peryx serve --config peryx.toml
peryx serve --config peryx.toml --read-only
```

The environment variable and command-line flag provide the same setting. Replica mode does not claim the configured
writer identity, so a restored configuration may retain the source writer's identity. It disables upstream cache fills,
webhook delivery, and background maintenance. A configured replication replica enforces the same writer-identity check.

Populate each replica's data directory from a verified backup or an external replication system before routing traffic
to it. Copy the metadata store and referenced blobs from the same point in time. peryx does not copy data between nodes
or coordinate a shared blob store.

## Load-balancer probes

`GET /+health` checks the local metadata and blob stores. `GET /+ready` returns `200` when the process can serve reads.
`GET /+ready?writes=true` returns `200` on a healthy writer and `503` on each replica. Configure the read pool with
`/+ready` and the write pool with `/+ready?writes=true`.

`GET /+status` reports `role` as `writer` or `replica`. Its `health` object shows whether the node can serve reads or
accept writes, plus the state of both local stores. It also reports the last observed reachability of each configured
upstream.

## Manual promotion

1. Stop or fence the old writer so it cannot accept another mutation.

1. Finish copying its metadata and blobs to the selected replica and verify the copy.

1. With the replica stopped and still configured with the old identity, replace the store's writer claim:

   ```shell
   peryx writer promote writer-b --config peryx.toml
   ```

   The command compares the configured identity with the store's current claim and refuses a stale or missing value.

1. Set `writer_identity = "writer-b"`, remove replica mode, and start the selected replica.

1. Wait for `GET /+ready?writes=true` to return `200`, then move write traffic to it.

1. Rebuild former writer nodes as replicas before returning them to service.

Promotion changes the store's claim; it does not copy data or stop the old process. peryx does not provide leader
election or online promotion. Do not promote until you fence the old writer, and do not start two writers against copies
that can diverge.
