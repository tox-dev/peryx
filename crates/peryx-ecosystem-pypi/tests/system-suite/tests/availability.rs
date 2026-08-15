use peryx_test_support as harness;

#[path = "cases/dc_group_readiness.rs"]
mod dc_group_readiness;
#[path = "cases/failover.rs"]
mod failover;
#[path = "cases/finalize_home_dc.rs"]
mod finalize_home_dc;
#[path = "cases/jobs_availability.rs"]
mod jobs_availability;
#[path = "support/pypi.rs"]
mod pypi_support;
#[path = "cases/replica_policy.rs"]
mod replica_policy;
