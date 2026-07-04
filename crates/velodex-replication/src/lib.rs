//! Primary/replica replication for velodex — reserved, not yet implemented.
//!
//! This crate is intentionally empty. It reserves the boundary the distributed story will grow
//! behind so the single-node code never has to move to accommodate it: velodex runs as one writer by
//! default, and replication is opt-in.
//!
//! The design it will implement:
//!
//! - A **primary** owns the append-only [`Journal`] in `velodex-storage`: every mutation (upload,
//!   yank, delete, override, mirror fetch) appends a serial, and that ordered log is the single source
//!   of truth for what changed and when.
//! - **Replicas** follow the primary's changelog by serial, fetch referenced blobs by digest with
//!   hash verification, and serve reads locally while proxying writes back to the primary — the same
//!   single-writer, replay-a-log model devpi uses, and the substrate a single-writer HA / failover
//!   story sits on.
//!
//! Nothing depends on this crate yet; it links no code into the running server. When replication is
//! built, it consumes the storage journal and a coordination seam in the HTTP layer, neither of which
//! the request hot path touches.
//!
//! [`Journal`]: https://docs.rs/velodex-storage
