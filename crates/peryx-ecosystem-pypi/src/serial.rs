use crate::changelog::{CHANGELOG_PAGE_SIZE, ChangelogEntry};

/// A serial and the ordered journal domain that assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialStamp {
    pub domain: String,
    pub serial: u64,
}

/// Why an upstream response cannot satisfy a required serial watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamSerialError {
    Missing { required: u64 },
    Regressed { required: u64, received: u64 },
}

impl std::fmt::Display for UpstreamSerialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { required } => write!(
                formatter,
                "upstream response omitted the serial watermark; required at least {required}"
            ),
            Self::Regressed { required, received } => {
                write!(
                    formatter,
                    "upstream serial {received} precedes required serial {required}"
                )
            }
        }
    }
}

impl std::error::Error for UpstreamSerialError {}

/// Require an upstream response to preserve or advance a cached serial watermark.
///
/// An unversioned cache accepts any response. Once the cache records a serial, a missing or lower
/// response serial may come from a stale CDN object and must not replace the cached page.
///
/// # Errors
/// Returns [`UpstreamSerialError`] when a versioned cache receives no serial or a lower serial.
pub const fn validate_upstream_serial(
    required: Option<u64>,
    received: Option<u64>,
) -> Result<Option<u64>, UpstreamSerialError> {
    match (required, received) {
        (Some(required), None) => Err(UpstreamSerialError::Missing { required }),
        (Some(required), Some(received)) if received < required => {
            Err(UpstreamSerialError::Regressed { required, received })
        }
        (None | Some(_), received) => Ok(received),
    }
}

/// Compose serial watermarks for every layer that contributed to one response.
///
/// The function returns the lowest watermark for a shared domain. Clients can treat that value as
/// the newest serial present in every contributing layer. Missing stamps, mixed domains, and an
/// empty response have no safe scalar serial.
#[must_use]
pub fn compose_serial_watermarks(stamps: impl IntoIterator<Item = Option<SerialStamp>>) -> Option<SerialStamp> {
    let mut stamps = stamps.into_iter();
    let mut composed = stamps.next()??;
    for stamp in stamps {
        let stamp = stamp?;
        if stamp.domain != composed.domain {
            return None;
        }
        composed.serial = composed.serial.min(stamp.serial);
    }
    Some(composed)
}

/// A validated snapshot page for `changelog_since_serial`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogPage {
    after: i64,
    current_serial: u64,
    entries: Vec<ChangelogEntry>,
}

/// Why a changelog page cannot represent one ordered snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangelogPageError {
    TooLarge { actual: usize },
    AtOrBeforeCursor { after: i64, serial: u64 },
    NotIncreasing { previous: u64, serial: u64 },
    BeyondSnapshot { current: u64, serial: u64 },
}

impl std::fmt::Display for ChangelogPageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { actual } => write!(
                formatter,
                "changelog page has {actual} entries; limit is {CHANGELOG_PAGE_SIZE}"
            ),
            Self::AtOrBeforeCursor { after, serial } => {
                write!(formatter, "changelog serial {serial} is not after cursor {after}")
            }
            Self::NotIncreasing { previous, serial } => {
                write!(formatter, "changelog serial {serial} does not follow {previous}")
            }
            Self::BeyondSnapshot { current, serial } => {
                write!(formatter, "changelog serial {serial} exceeds snapshot {current}")
            }
        }
    }
}

impl std::error::Error for ChangelogPageError {}

impl ChangelogPage {
    /// Validate entries read after `after` from the snapshot at `current_serial`.
    ///
    /// # Errors
    /// Returns [`ChangelogPageError`] when the page is oversized, regresses its cursor, is not
    /// strictly ordered, or contains an entry newer than the snapshot.
    pub fn new(after: i64, current_serial: u64, entries: Vec<ChangelogEntry>) -> Result<Self, ChangelogPageError> {
        if entries.len() > CHANGELOG_PAGE_SIZE {
            return Err(ChangelogPageError::TooLarge { actual: entries.len() });
        }
        let cursor = u64::try_from(after).ok();
        let mut previous = None;
        for entry in &entries {
            if entry.serial > current_serial {
                return Err(ChangelogPageError::BeyondSnapshot {
                    current: current_serial,
                    serial: entry.serial,
                });
            }
            if cursor.is_some_and(|cursor| entry.serial <= cursor) {
                return Err(ChangelogPageError::AtOrBeforeCursor {
                    after,
                    serial: entry.serial,
                });
            }
            if let Some(previous) = previous
                && entry.serial <= previous
            {
                return Err(ChangelogPageError::NotIncreasing {
                    previous,
                    serial: entry.serial,
                });
            }
            previous = Some(entry.serial);
        }
        Ok(Self {
            after,
            current_serial,
            entries,
        })
    }

    #[must_use]
    pub const fn current_serial(&self) -> u64 {
        self.current_serial
    }

    #[must_use]
    pub fn entries(&self) -> &[ChangelogEntry] {
        &self.entries
    }

    /// The next exclusive cursor: the last returned entry, or the greater of the request cursor and
    /// snapshot serial for an empty page.
    #[must_use]
    pub fn resume_serial(&self) -> u64 {
        self.entries.last().map_or_else(
            || u64::try_from(self.after).map_or(self.current_serial, |after| after.max(self.current_serial)),
            |entry| entry.serial,
        )
    }
}
