use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::revocation::DigestRevocation;

const SERVER_OP_TAG: &str = "server-op";

/// A change to peryx's own metadata that a replica replays from the shared journal.
///
/// Ecosystem drivers write their own opaque payloads to the same log, so the core vocabulary names
/// itself: an entry carrying the `server-op` tag is core, and anything else belongs to whichever driver
/// wrote it. The tag makes the classification total, which matters because the two failure modes are not
/// symmetric - skipping a foreign payload is correct, while skipping a core payload a replica cannot
/// decode would leave it serving bytes the writer revoked. A tagged payload that will not decode is
/// therefore an error, not an entry to pass over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "server-op", rename_all = "kebab-case")]
pub enum ServerMutation {
    /// The authoritative revocation row for one digest, in whatever state the writer left it.
    DigestRevocation { record: DigestRevocation },
}

impl ServerMutation {
    /// # Panics
    /// Panics if the change does not serialize, which no variant's fields can refuse.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a server mutation always serializes")
    }

    /// Returns `None` when the payload belongs to an ecosystem driver rather than to core.
    ///
    /// # Errors
    /// Returns a decode error when the payload claims a core operation it does not describe.
    pub fn decode(payload: &[u8]) -> Result<Option<Self>, MetaError> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return Ok(None);
        };
        if value.get(SERVER_OP_TAG).is_none() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(value)?))
    }
}
