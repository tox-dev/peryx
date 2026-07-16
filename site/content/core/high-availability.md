+++
title = "High availability"
description = "Run one writer with read replicas and promote a replica during a planned failover."
weight = 7
+++

peryx supports one writer with multiple read replicas. Send mutation traffic to the writer. Replicas serve data copied
from the writer and reject mutation requests with `503 Service Unavailable`.

Enable replica mode in TOML:

```toml
read_only = true
```

```shell
PERYX_READ_ONLY=true peryx serve --config peryx.toml
peryx serve --config peryx.toml --read-only
```

The environment variable and command-line flag above provide the same setting. Replica mode disables upstream cache
fills. It also stops webhook delivery and background maintenance. Populate the replica's data directory from a verified
backup or an external replication system before routing traffic to it. peryx does not copy data between nodes or
coordinate a shared blob store.

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
1. Remove `read_only = true` from the selected replica and restart it.
1. Wait for `GET /+ready?writes=true` to return `200`, then move write traffic to it.
1. Rebuild former writer nodes as replicas before returning them to service.

peryx does not provide leader election or online promotion. Do not start two writers against copies that can diverge.
