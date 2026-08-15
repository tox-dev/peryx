//! The unsigned offset is safe to edit because scope filtering precedes pagination. The scope hash
//! prevents replay after visibility changes.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};

use crate::error::PqlError;
use crate::scope::QueryScope;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CursorPayload {
    domain: String,
    scope: String,
    offset: u64,
}

/// # Panics
/// The fixed scalar payload always serializes.
#[must_use]
pub fn encode(domain: &str, scope: &QueryScope, offset: u64) -> String {
    let payload = CursorPayload {
        domain: domain.to_owned(),
        scope: scope_hash(domain, scope),
        offset,
    };
    let json = serde_json::to_vec(&payload).expect("cursor payload serializes");
    URL_SAFE_NO_PAD.encode(json)
}

/// # Errors
/// Returns [`PqlError::InvalidCursor`] when the text is malformed or names a different domain, and
/// [`PqlError::CursorScopeChanged`] when the caller's scope no longer matches the one the cursor was
/// minted under.
pub fn decode(text: &str, domain: &str, scope: &QueryScope) -> Result<u64, PqlError> {
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| PqlError::InvalidCursor)?;
    let payload: CursorPayload = serde_json::from_slice(&bytes).map_err(|_| PqlError::InvalidCursor)?;
    if payload.domain != domain {
        return Err(PqlError::InvalidCursor);
    }
    if payload.scope != scope_hash(domain, scope) {
        return Err(PqlError::CursorScopeChanged);
    }
    Ok(payload.offset)
}

fn scope_hash(domain: &str, scope: &QueryScope) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(scope.fingerprint().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
