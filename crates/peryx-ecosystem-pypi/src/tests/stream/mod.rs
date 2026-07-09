use std::collections::HashMap;

use peryx_policy::Policy;

use crate::File;
use crate::stream::{PageContext, page_context as build_page_context};

mod context_tests;
mod transformer_tests;
mod types_tests;

pub(super) fn page_context<S: std::hash::BuildHasher>(
    route: &str,
    local_files: Vec<File>,
    local_versions: Vec<String>,
    overrides: &HashMap<String, String, S>,
) -> PageContext {
    build_page_context(route, "demo", Policy::default(), local_files, local_versions, overrides)
}
