+++
title = "Availability test harness"
description = "How the multi-process availability tests spawn real peryx binaries, inject network faults through Toxiproxy, and observe a datacenter group over its public HTTP surface."
weight = 40
+++

The availability harness stands up a group of real `peryx serve` processes, faults the network between them, and asserts
what an operator would see over HTTP. It lives in `crates/peryx/tests/harness/` and the self-tests that exercise it are
in `crates/peryx/tests/availability.rs`. Both sit behind the `availability-e2e` feature, so the default `cargo test` and
the coverage gate skip them; CI runs them in a dedicated job that installs `toxiproxy-server`.

Run them with the binary on your `PATH`:

```console
$ brew install toxiproxy     # or download toxiproxy-server from the releases
$ just availability
```

## Test API

A test describes a group with a `Topology` and spawns it into a `Cluster`. `Topology::single()` is one stand-alone node;
`Topology::dc(group, members)` and `Topology::ha(group, members)` build a datacenter roster from `MemberSpec`s,
generating each node's config, ports, and the shared `[[availability.member]]` roster so every node agrees on it. Each
spawned `Node` owns a temp data directory, a captured log, and its ports; the handle drives it (`await_ready`, `kill`,
`restart`) and observes it (`status`, `readiness`, `topology`, `is_running`). Every process runs in its own group and is
killed when the `Cluster` drops, so a panicking test leaks nothing.

`Toxiproxy` wraps a managed `toxiproxy-server`: `proxy(upstream)` puts a controllable listener in front of a node's
socket, and the returned `Proxy` cuts (`partition`), restores (`heal`), or slows (`pause`) the link. This is how a test
partitions two nodes without touching their processes.

On an assertion failure, `Cluster::failure_report()` renders each node's topology snapshot, status body, and log tail.

## Current limits

The embedded ownership Raft node runs, but a multi-node consensus group cannot form yet. Peryx does not mount the
inbound peer-RPC router, so bootstrap cannot reach quorum, and no HTTP endpoint accepts an ownership write or returns
the current authority. `Topology::validate_config()` proves through `peryx config check` that peryx accepts a generated
`ha` or `dc` roster without starting a server. The `OwnershipControl` methods (`submit_ownership_write`, `leader`,
`await_authority_transfer`) return `HarnessError::Unsupported`. The failover test tier will implement them after the
write and authority endpoints land ([#540](https://github.com/tox-dev/peryx/issues/540)). Mounting the peer-RPC router
will let `ha` topologies spawn a live cluster instead of validating configuration alone.

## Extending it

The downstream availability tests ([#558](https://github.com/tox-dev/peryx/issues/558),
[#559](https://github.com/tox-dev/peryx/issues/559), and the cross-mode suites) add `tests/*.rs` files that declare
`mod harness;` and drive clusters through the same API. Put new observations on `Node` and new faults on `Proxy`. Add
ownership controls to the `OwnershipControl` trait when the endpoint exists.
