//! Argon2id defaults follow [OWASP Password Storage guidance]. [`PasswordPolicy::new`] supports the
//! higher-memory profiles in [RFC 9106].
//!
//! [OWASP Password Storage guidance]: https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html
//! [RFC 9106]: https://www.rfc-editor.org/rfc/rfc9106

use std::fmt;
use std::hint::black_box;

use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
}

/// Errors omit the password and verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    #[error("argon2 cost parameters are out of range")]
    Params,
    #[error("password hashing failed")]
    Hash,
}

impl PasswordPolicy {
    /// The OWASP-recommended Argon2id baseline: 19 MiB, two passes, a single lane.
    #[must_use]
    pub const fn recommended() -> Self {
        Self {
            memory_kib: 19_456,
            iterations: 2,
            lanes: 1,
        }
    }

    /// `memory_kib` uses kibibytes; `iterations` and `lanes` are counts.
    ///
    /// # Errors
    /// Returns [`PasswordError::Params`] when the costs fall outside Argon2's accepted range, including
    /// `memory_kib` below `8 * lanes`.
    pub fn new(memory_kib: u32, iterations: u32, lanes: u32) -> Result<Self, PasswordError> {
        Params::new(memory_kib, iterations, lanes, None).map_err(|_| PasswordError::Params)?;
        Ok(Self {
            memory_kib,
            iterations,
            lanes,
        })
    }

    /// # Errors
    /// Returns [`PasswordError::Hash`] when salt generation or the Argon2id derivation fails.
    pub fn hash(&self, password: &str) -> Result<PasswordVerifier, PasswordError> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|_| PasswordError::Hash)?;
        let salt = SaltString::encode_b64(&salt).map_err(|_| PasswordError::Hash)?;
        let encoded = self
            .argon2()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| PasswordError::Hash)?
            .to_string();
        Ok(PasswordVerifier(encoded))
    }

    /// Spends one verification for unknown and passwordless accounts to mask account existence.
    ///
    /// # Panics
    /// Panics if Argon2 rejects a 16-byte salt, which its salt encoding contract accepts.
    pub fn spend_decoy(&self, password: &str) {
        let mut salt = [0u8; 16];
        let _ = getrandom::fill(&mut salt);
        let salt = SaltString::encode_b64(&salt).expect("16-byte salts are valid");
        let _ = black_box(self.argon2().hash_password(black_box(password).as_bytes(), &salt));
    }

    fn argon2(&self) -> Argon2<'static> {
        let params =
            Params::new(self.memory_kib, self.iterations, self.lanes, None).expect("policy validated on construction");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }
}

/// Debug output redacts the stored Argon2id verifier.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PasswordVerifier(String);

/// `Accepted { stale: true }` requests re-enrollment under the current policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordCheck {
    Accepted { stale: bool },
    Rejected,
}

impl PasswordVerifier {
    /// Malformed stored verifiers reject like wrong passwords to avoid exposing credential state.
    #[must_use]
    pub fn check(&self, password: &str, policy: &PasswordPolicy) -> PasswordCheck {
        let Ok(parsed) = PasswordHash::new(&self.0) else {
            return PasswordCheck::Rejected;
        };
        if Argon2::default().verify_password(password.as_bytes(), &parsed).is_err() {
            return PasswordCheck::Rejected;
        }
        PasswordCheck::Accepted {
            stale: profile_trails(&parsed, policy),
        }
    }
}

/// Re-enroll when the algorithm profile or any cost differs from the active policy.
fn profile_trails(hash: &PasswordHash<'_>, policy: &PasswordPolicy) -> bool {
    let params = Params::try_from(hash).expect("a verified argon2 hash carries valid parameters");
    hash.algorithm != Algorithm::Argon2id.ident()
        || hash.version != Some(Version::V0x13.into())
        || params.m_cost() != policy.memory_kib
        || params.t_cost() != policy.iterations
        || params.p_cost() != policy.lanes
        || params.output_len() != Some(Params::DEFAULT_OUTPUT_LEN)
}

impl fmt::Debug for PasswordVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PasswordVerifier(<redacted>)")
    }
}
