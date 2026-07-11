//! The token realm's signing key: minting a JWT for an approved set of grants, and verifying one back.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::{Grant, Principal};

/// The key peryx signs its own tokens with, and the only thing that verifies them.
///
/// The tokens are JWTs (HS256): a client's credential is a self-contained, expiring assertion of the
/// grants a token endpoint approved, so verifying one is a signature check with no lookup — the
/// property a replica needs, since it can verify a token the primary minted without sharing a
/// database. Signing and verifying come from `jsonwebtoken`, the maintained Rust implementation of
/// RFC 7519, rather than from a hand-rolled MAC.
#[derive(Clone)]
pub struct Signer {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl Signer {
    #[must_use]
    pub fn new(key: &[u8]) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        Self {
            encoding: EncodingKey::from_secret(key),
            decoding: DecodingKey::from_secret(key),
            validation,
        }
    }

    /// Mint a token for `principal` carrying `grants`, valid for `ttl_secs` from `issued_at` (unix
    /// seconds). An anonymous principal gets the empty subject, as the distribution spec's token
    /// server does: a token with no identity still carries whatever the index grants anonymously.
    ///
    /// # Panics
    /// Never in practice: HS256 signing fails only if the claims cannot be serialized, and they are a
    /// fixed struct of strings and integers.
    #[must_use]
    pub fn mint(&self, principal: &Principal, grants: &[Grant], issued_at: i64, ttl_secs: i64) -> String {
        let claims = Claims {
            sub: match principal {
                Principal::Anonymous => String::new(),
                Principal::Named { subject } => subject.clone(),
            },
            iat: issued_at,
            exp: issued_at + ttl_secs,
            grants: grants.to_vec(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .expect("HS256 signing of serializable claims cannot fail")
    }

    /// Recover the principal and grants a token asserts, rejecting one this key did not sign, one whose
    /// claims were altered, and one past its expiry.
    ///
    /// # Errors
    /// Returns [`TokenError`] when the token fails signature, structure, or expiry validation.
    pub fn verify(&self, token: &str) -> Result<(Principal, Vec<Grant>), TokenError> {
        let claims = jsonwebtoken::decode::<Claims>(token, &self.decoding, &self.validation)
            .map_err(TokenError)?
            .claims;
        let principal = if claims.sub.is_empty() {
            Principal::Anonymous
        } else {
            Principal::Named { subject: claims.sub }
        };
        Ok((principal, claims.grants))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid token: {0}")]
pub struct TokenError(jsonwebtoken::errors::Error);

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    iat: i64,
    exp: i64,
    grants: Vec<Grant>,
}
