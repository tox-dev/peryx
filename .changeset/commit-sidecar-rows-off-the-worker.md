---
default: patch
---

# Commit sidecar rows off the async worker

A burst of cold wheel metadata sidecars no longer holds the request workers while the metadata store syncs to disk, so
`/+status` keeps answering during the burst.
