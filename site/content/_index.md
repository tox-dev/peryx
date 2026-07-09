+++
template = "index.html"
+++

- **Read-through cache** Proxies pypi.org, Docker Hub, or any upstream index or registry. A cache miss streams upstream
  bytes to the client while writing them aside, so the first pull pays no penalty; every later one comes from disk. Each
  artifact (a wheel or an image layer) verifies against its sha256 and is stored once, content-addressed, however many
  projects pull it.
- **Private packages that shadow upstream** Publish over each ecosystem's own upload API into virtual indexes, where
  your `utils` beats the `utils` someone registered upstream and your image beats the one on the public registry. One
  URL, no second index to configure, no [dependency confusion](@/core/indexes.md).
- **One model, every ecosystem** The same cache/host/merge roles serve Python packages over the
  [PyPI Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) (PEP 691 JSON with PEP
  503 HTML fallback, PEP 700 fields, the PEP 658/714 `.metadata` fast path) and container images over the
  [OCI `/v2/` distribution API](https://github.com/opencontainers/distribution-spec). Adding a format is a driver, not a
  rewrite. See [ecosystems](@/ecosystems/_index.md).
- **Honest freshness** Upstream `Cache-Control` decides how long a page serves from cache; a background sweep catches
  upstream changes even for things nobody is requesting; outages degrade to stale-but-working. Concurrent misses for one
  page, wheel, or layer share a single upstream fetch.
- **Built to operate** One TOML file, Prometheus metrics, per-file usage drill-down, structured logs, a live web UI, and
  a data directory you can back up with `cp`. Optional TLS or automatic Let's Encrypt certificates. No JVM, no database
  server, idle RAM in the tens of MB.
- **Proven with real clients** The test suite drives each ecosystem's real clients against a live velodex, holds 100%
  line and function coverage, passes the OCI distribution-spec conformance suite, and the
  [performance numbers](@/core/performance.md) come with the exact commands that produced them.
