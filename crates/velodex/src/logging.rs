//! Logging setup helpers.
//!
//! The pure decisions (validating the sink, building the level filter) live here and are tested.
//! Installing the global subscriber is side-effectful and platform-specific, so it lives in
//! `main.rs`, which coverage excludes.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::ParseError;

use crate::config::{LogConfig, LogSink};

/// An error in the logging configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LogError {
    #[error("log sink 'file' requires a log file path (--log-file or log.file)")]
    MissingFilePath,
}

/// Validate a resolved log config before a subscriber is built.
///
/// # Errors
/// Returns [`LogError::MissingFilePath`] when the sink is [`LogSink::File`] but no path is set.
pub const fn validate(cfg: &LogConfig) -> Result<(), LogError> {
    if matches!(cfg.sink, LogSink::File) && cfg.file.is_none() {
        return Err(LogError::MissingFilePath);
    }
    Ok(())
}

/// Build an [`EnvFilter`] from a level directive such as `info` or `velodex_upstream=debug`.
///
/// # Errors
/// Returns the parse error when `directive` is not a valid filter.
pub fn env_filter(directive: &str) -> Result<EnvFilter, ParseError> {
    EnvFilter::try_new(directive)
}
