//! Storage for velodex: the content-addressed blob store, and later the metadata and serial log.

pub mod blob;
pub mod meta;

#[cfg(test)]
mod tests;
