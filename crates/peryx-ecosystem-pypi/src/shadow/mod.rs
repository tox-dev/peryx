mod http;
mod inspect;
mod model;
mod query;

pub use http::ShadowRoutes;
pub use model::{ShadowCandidate, ShadowReason, ShadowSource};
pub use query::{ShadowQuery, ShadowQueryError};

#[cfg(test)]
#[path = "../../tests/unit/shadow/coverage_tests.rs"]
mod coverage_tests;
