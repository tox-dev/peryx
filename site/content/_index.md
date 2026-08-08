+++
template = "index.html"
+++

- Read-through cache. Proxy an upstream index or registry. A cache miss streams upstream bytes to the client while
  storing them, so the first pull does not wait for a second pass. Later pulls come from the content-addressed store.
- Private artifacts. Publish through the ecosystem's upload API. A virtual index places private artifacts ahead of
  upstream artifacts under one URL, which prevents [dependency confusion](@/core/indexes.md).
- Shared index roles. Cache, host, and merge roles work across the [supported ecosystems](@/ecosystems/_index.md). Each
  ecosystem driver owns its wire protocol and artifact rules.
- Bounded freshness. Upstream `Cache-Control` sets page freshness. A background sweep finds upstream changes. Stale
  pages remain available during outages, and concurrent misses share one upstream fetch.
- Operations. One [TOML](https://toml.io/) file configures peryx. It exposes [Prometheus](https://prometheus.io/)
  metrics, structured logs, usage data, and a web UI. The data directory supports file-level backups. TLS can use
  supplied certificates or [Let's Encrypt](https://letsencrypt.org/).
- Verification. Tests run ecosystem clients against a live server and require 100% line and function coverage. They run
  the [OCI distribution-spec](https://github.com/opencontainers/distribution-spec) conformance suite. The
  [performance results](@/core/performance.md) include reproduction commands.
