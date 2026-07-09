//! The workloads' image sets, pinned to a tag so runs stay comparable.
//!
//! Every reference is a Docker Hub repository path plus tag; each server prepends its own registry
//! base (`peryx` its proxy route, `distribution`/`zot` their root, `direct` Docker Hub itself). The
//! set is kept small and to widely-mirrored official images so a full run stays inside Docker Hub's
//! anonymous pull budget.

/// The images the pull workload fetches through each registry, cold then warm: the OCI analogue of
/// installing the top packages. A spread of sizes, all official.
pub const PULL_IMAGES: &[&str] = &[
    "library/alpine:3.20",
    "library/busybox:1.36",
    "library/debian:bookworm-slim",
    "library/redis:7.4-alpine",
    "library/nginx:1.27-alpine",
    "library/memcached:1.6-alpine",
];

/// The image whose largest layer the throughput workload streams: one big blob to price raw
/// transfer, the OCI analogue of the stress wheel.
pub const STRESS_IMAGE: &str = "library/python:3.12-slim";

/// The image a fleet of clients pulls at once, cold then warm.
pub const FLEET_IMAGE: &str = "library/node:22-alpine";
