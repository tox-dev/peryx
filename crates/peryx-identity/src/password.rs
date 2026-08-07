//! Local password authentication over the server-user store.
//!
//! A password is never stored: [`PasswordPolicy::hash`] derives a memory-hard Argon2id verifier and the
//! caller keeps only that. [`PasswordVerifier::check`] answers whether a presented password derives the
//! stored verifier, and whether the verifier's parameters have fallen behind the current policy so the
//! caller can re-enroll it under the same identity. The verifier is a secret: its [`Debug`] is redacted
//! and it never surfaces in a serialized account view.
//!
//! Defaults follow the [OWASP Password Storage guidance] for Argon2id - 19 MiB of memory, two
//! iterations, one lane - over the algorithm [RFC 9106] standardizes. A deployment that wants the
//! RFC's higher-memory profile raises them through [`PasswordPolicy::new`].
//!
//! [OWASP Password Storage guidance]: https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html
//! [RFC 9106]: https://www.rfc-editor.org/rfc/rfc9106

use std::fmt;
use std::hint::black_box;

use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

/// The Argon2id cost parameters a deployment enrolls and verifies passwords under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
}

/// A rejected password operation. Neither variant carries the password or the verifier.
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

    /// Build a policy from explicit Argon2id costs, in kibibytes of memory, passes, and lanes.
    ///
    /// # Errors
    /// Returns [`PasswordError::Params`] when the costs fall outside Argon2's accepted range (notably
    /// `memory_kib` below `8 * lanes`).
    pub fn new(memory_kib: u32, iterations: u32, lanes: u32) -> Result<Self, PasswordError> {
        Params::new(memory_kib, iterations, lanes, None).map_err(|_| PasswordError::Params)?;
        Ok(Self {
            memory_kib,
            iterations,
            lanes,
        })
    }

    /// Derive a memory-hard verifier for `password` under a fresh random salt.
    ///
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

    /// Spend one verification's worth of work without a stored verifier, so an unknown or passwordless
    /// account fails in the same time a real mismatch does and reveals nothing by how long it took.
    pub fn spend_decoy(&self, password: &str) {
        let mut salt = [0u8; 16];
        let _ = getrandom::fill(&mut salt);
        if let Ok(salt) = SaltString::encode_b64(&salt) {
            let _ = black_box(self.argon2().hash_password(black_box(password).as_bytes(), &salt));
        }
    }

    fn argon2(&self) -> Argon2<'static> {
        let params =
            Params::new(self.memory_kib, self.iterations, self.lanes, None).expect("policy validated on construction");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }
}

/// A stored Argon2id verifier.
///
/// It holds the PHC-encoded salt, parameters, and tag that a presented password must reproduce. It is a
/// credential secret, so its [`Debug`] redacts and it is never placed in an account view a client can
/// read.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PasswordVerifier(String);

/// The outcome of checking a password against a stored verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordCheck {
    /// The password reproduced the verifier. `stale` is set when the verifier's parameters no longer
    /// match the policy, so the caller should re-enroll it under the same identity.
    Accepted { stale: bool },
    /// The password did not reproduce the verifier.
    Rejected,
}

impl PasswordVerifier {
    /// Whether `password` reproduces this verifier, and whether its parameters trail `policy`.
    ///
    /// A malformed stored verifier rejects rather than erroring: an account whose credential cannot be
    /// parsed cannot authenticate, and that is the same public outcome as a wrong password.
    #[must_use]
    pub fn check(&self, password: &str, policy: &PasswordPolicy) -> PasswordCheck {
        let Ok(parsed) = PasswordHash::new(&self.0) else {
            return PasswordCheck::Rejected;
        };
        if Argon2::default().verify_password(password.as_bytes(), &parsed).is_err() {
            return PasswordCheck::Rejected;
        }
        PasswordCheck::Accepted {
            stale: params_trail(&parsed, policy),
        }
    }
}

/// A verified verifier trails the policy when any Argon2id cost no longer matches, so a tightened policy
/// re-enrolls the credential and a loosened one is normalized back up on the next successful login. A
/// verifier only reaches here after [`argon2::Argon2::verify_password`] accepted it, so its parameters
/// parse.
fn params_trail(hash: &PasswordHash<'_>, policy: &PasswordPolicy) -> bool {
    let params = Params::try_from(hash).expect("a verified argon2 hash carries valid parameters");
    params.m_cost() != policy.memory_kib || params.t_cost() != policy.iterations || params.p_cost() != policy.lanes
}

impl fmt::Debug for PasswordVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PasswordVerifier(<redacted>)")
    }
}
