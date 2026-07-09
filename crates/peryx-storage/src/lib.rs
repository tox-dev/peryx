//! Storage for peryx: the content-addressed blob store and the metadata store, which holds the
//! cached index, uploads, overrides, and the append-only journal (the serial changelog).

pub mod archive;
pub mod blob;
pub mod meta;

#[cfg(test)]
mod tests;
