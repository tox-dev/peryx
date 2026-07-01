//! The `PyPI` ecosystem: project names, and later versions, filenames, and index resolution.
//!
//! This is the only ecosystem velox implements. It sits under its own module so a future ecosystem
//! can be added as a sibling rather than tangled into shared code.

mod name;

pub use name::{PackageName, normalize_name};

#[cfg(test)]
mod tests;
