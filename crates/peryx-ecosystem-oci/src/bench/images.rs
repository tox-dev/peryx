/// Official images of varied sizes that the pull workload fetches cold, then warm.
pub const PULL_IMAGES: &[&str] = &[
    "library/alpine:3.20",
    "library/busybox:1.36",
    "library/debian:bookworm-slim",
    "library/redis:7.4-alpine",
    "library/nginx:1.27-alpine",
    "library/memcached:1.6-alpine",
];

/// The image whose largest layer prices raw blob throughput.
pub const STRESS_IMAGE: &str = "library/python:3.12-slim";

/// The image a fleet of clients pulls at once, cold then warm.
pub const FLEET_IMAGE: &str = "library/node:22-alpine";

/// A small image outside measured workloads, used to prove upstream pulls work before timing.
pub const READINESS_IMAGE: &str = "library/hello-world:latest";
