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
    audience: String,
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl Signer {
    /// Build a signer whose tokens are valid only for `audience`.
    #[must_use]
    pub fn new(key: &[u8], audience: impl Into<String>) -> Self {
        let audience = audience.into();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.required_spec_claims.insert("aud".to_owned());
        validation.set_audience(&[&audience]);
        Self {
            audience,
            encoding: EncodingKey::from_secret(key),
            decoding: DecodingKey::from_secret(key),
            validation,
        }
    }

    /// The service this signer mints tokens for and accepts tokens on behalf of.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
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
        self.mint_with_id(
            principal,
            grants,
            issued_at,
            ttl_secs,
            &uuid::Uuid::new_v4().to_string(),
            TokenPurpose::Realm,
        )
    }

    /// Mint a token that only the trusted-publishing upload path accepts.
    #[must_use]
    pub fn mint_trusted(
        &self,
        principal: &Principal,
        grants: &[Grant],
        issued_at: i64,
        ttl_secs: i64,
        token_id: &str,
    ) -> String {
        self.mint_with_id(
            principal,
            grants,
            issued_at,
            ttl_secs,
            token_id,
            TokenPurpose::TrustedPublishing,
        )
    }

    #[must_use]
    fn mint_with_id(
        &self,
        principal: &Principal,
        grants: &[Grant],
        issued_at: i64,
        ttl_secs: i64,
        token_id: &str,
        purpose: TokenPurpose,
    ) -> String {
        let claims = MintedClaims {
            sub: match principal {
                Principal::Anonymous => "",
                Principal::Named { subject } => subject,
            },
            aud: &self.audience,
            iat: issued_at,
            exp: issued_at + ttl_secs,
            jti: token_id,
            purpose,
            grants,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .expect("HS256 signing of serializable claims cannot fail")
    }

    /// Recover the principal and grants a token asserts, rejecting one this key did not sign, one whose
    /// claims were altered, one intended for another audience, and one past its expiry.
    ///
    /// # Errors
    /// Returns [`TokenError`] when the token fails signature, structure, audience, or expiry validation.
    pub fn verify(&self, token: &str) -> Result<(Principal, Vec<Grant>), TokenError> {
        let token = self.verify_identified(token)?;
        Ok((token.principal, token.grants))
    }

    fn verify_identified(&self, token: &str) -> Result<VerifiedToken, TokenError> {
        self.verify_for(token, TokenPurpose::Realm)
    }

    /// Verify a trusted-publishing token and return its audit ID.
    ///
    /// # Errors
    /// Returns [`TokenError`] for an invalid trusted-publishing token or a token minted for another purpose.
    pub fn verify_trusted(&self, token: &str) -> Result<VerifiedToken, TokenError> {
        self.verify_for(token, TokenPurpose::TrustedPublishing)
    }

    fn verify_for(&self, token: &str, purpose: TokenPurpose) -> Result<VerifiedToken, TokenError> {
        let claims = jsonwebtoken::decode::<VerifiedClaims>(token, &self.decoding, &self.validation)
            .map_err(TokenError)?
            .claims;
        if claims.purpose != purpose {
            return Err(TokenError(jsonwebtoken::errors::ErrorKind::InvalidToken.into()));
        }
        let principal = if claims.sub.is_empty() {
            Principal::Anonymous
        } else {
            Principal::Named { subject: claims.sub }
        };
        Ok(VerifiedToken {
            principal,
            grants: claims.grants,
            id: claims.jti,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    pub principal: Principal,
    pub grants: Vec<Grant>,
    pub id: String,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid token: {0}")]
pub struct TokenError(jsonwebtoken::errors::Error);

#[derive(Serialize)]
struct MintedClaims<'a> {
    sub: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
    jti: &'a str,
    purpose: TokenPurpose,
    grants: &'a [Grant],
}

#[derive(Deserialize)]
struct VerifiedClaims {
    sub: String,
    #[serde(default)]
    jti: String,
    #[serde(default)]
    purpose: TokenPurpose,
    grants: Vec<Grant>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TokenPurpose {
    #[default]
    Realm,
    TrustedPublishing,
}
