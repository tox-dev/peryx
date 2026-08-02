//! The opaque pagination cursor, bound to the scope it was minted under.
//!
//! A cursor carries the domain, the scan position, and a hash of the caller's resolved scope. On
//! decode the hash is recomputed from the caller's current scope and compared: a cursor minted under
//! one grant cannot be replayed under another, because a changed grant changes the hash and the
//! replay is refused rather than silently re-scoped.
//!
//! The encoding is base64 of unsigned JSON, so it is opaque but not integrity-protected: a caller can
//! edit the decoded offset. That is safe because the offset only positions a scan the scope predicate
//! has already narrowed to the caller's own rows, so a tampered offset can page within the caller's
//! own results but never reaches a row outside their scope. The security property the cursor enforces
//! is cross-scope, not offset integrity: the scope-hash check refuses a cursor carried across a grant
//! change or forged for a different scope.

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

/// Mint a cursor for the next page.
///
/// # Panics
/// Never in practice: the payload is a fixed shape of a string, a string, and an integer, which
/// always serializes.
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

/// Decode a cursor and return its scan offset, refusing a cursor that does not belong to this domain
/// and scope.
///
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
