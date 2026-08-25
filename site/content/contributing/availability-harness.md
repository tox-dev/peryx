+++
title = "Availability test harness"
description = "Exercise availability through real processes and controlled network faults."
weight = 40
+++

The availability harness starts real `peryx serve` processes and observes public HTTP behavior under service and network
faults. `peryx-test-support` owns reusable process and proxy support. Neutral scenarios live in
`crates/peryx/tests/{availability,cluster,observability}.rs`. Protocol requests and assertions stay in their ecosystem
system packages.

The neutral scenarios use the `availability-e2e` test feature. The native coverage job runs them with the rest of the
all-features workspace suite.

Install Toxiproxy for the host lane:

```shell
brew install toxiproxy
just availability
```

Use the Linux system profile when the host cannot run the dependencies:

```shell
just availability
```

## Lifecycle ownership

`Topology::single()` describes one process with availability disabled. `Topology::dc(group, members)` and
`Topology::ha(group, members)` build distributed rosters from `MemberSpec` values. The harness reserves listeners,
writes node configuration and data directories, starts each process, and waits for readiness before returning a
`Cluster`.

Each `Node` owns its child process, listeners, data directory, and captured log. `kill` and `restart` cause explicit
failures. Dropping a node terminates and reaps its process. A failed partial startup drops the nodes that started.

Use `ProcessHarness` for a specific binary or shutdown endpoint. Its process limit bounds concurrent children; it does
not replace readiness checks.

## Observe state

Use public operator surfaces:

- `status`, `readiness`, `topology`, `metrics`, and `placements` read named endpoints.
- `http_get`, `http_get_as`, and `request` cover other public routes.
- `is_running`, `kill`, and `restart` control the process lifecycle.
- `diagnostics` and `log_tail` retain failure evidence.

`Cluster::failure_report()` captures endpoint snapshots and a log tail from each node. Include it in failures that
depend on cluster state.

Wait for observable conditions with `await_topology_signal`, `await_leader`, `await_leader_change`,
`await_authority_transfer`, or `await_log_signal`. Pass a deadline to bound failure. Do not sleep before sampling;
sleeping makes success depend on runner speed and discards the last observation.

`Cluster` implements `OwnershipControl`. Leadership methods read live consensus state. Transfer authority by stopping a
process or changing its network path. The harness has no direct ownership-write shortcut.

## Inject network faults

`Toxiproxy::start()` owns a `toxiproxy-server` process. `Topology::start_proxied` places managed proxies between nodes
and returns a `Proxied` cluster.

- `partition` cuts a link; `heal` restores it.
- `pause` adds latency; `resume` removes it.
- `endpoint` returns the listener used by clients.

Drop `Toxiproxy` after the cluster so proxy listeners remain available during node shutdown. Both owners reap their
children on normal return and failure.

## Add a scenario

Put neutral process and availability behavior in the `peryx` package. Put protocol fixtures, requests, and assertions in
the owning ecosystem system package. Reuse `peryx-test-support`; do not copy its process harness.

Add node observations to `Node`, topology-wide waits to `Cluster`, and link faults to `Proxy`. Keep assertions on public
surfaces. Use `Topology::validate_config()` when a case needs to prove that generated `dc` or `ha` configuration passes
`peryx config check`.

Run the focused suites before the full distributed report:

```shell
just availability
just simulation
just coverage-native
```
