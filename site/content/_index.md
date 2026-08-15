+++
template = "index.html"
+++

- Read-through cache. A miss streams verified bytes from an upstream while committing them to the content-addressed
  store. Concurrent misses share one transfer.
- Private artifacts. Hosted indexes accept writes through their ecosystem owner. Virtual indexes resolve an ordered list
  of cached and hosted members.
- One binary. Configuration selects each index owner and the availability mode at startup. The executable has no
  architecture-changing runtime flags.
- Configured composition. Each index names a registered ecosystem owner. The owner validates its settings and installs
  its supported capabilities.
- Bounded freshness. Upstream cache directives set freshness within operator limits. Stored data can remain available
  for a configured stale window during an outage.
- Availability choice. `availability.mode = "none"` starts no distributed tasks, listeners, timers, or watchers.
  Distributed modes add replication, placement, and lifecycle services through neutral contracts.
- Verification. CI requires exact line and function coverage per crate and per declared system-test source root.
