use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::{Grant, Principal};

const REALM_SCOPE: TokenScope = TokenScope::new("realm");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenScope(&'static str);

impl TokenScope {
    /// # Panics
    ///
    /// Panics if `value` is empty.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "token scope must not be empty");
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// HS256 tokens carry approved grants and expiry as signed claims. Replicas can verify tokens without
/// sharing a database with the issuer.
#[derive(Clone)]
pub struct Signer {
    audience: String,
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl Signer {
    /// This signer restricts minted tokens to `audience`.
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

    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// `ttl_secs` starts at `issued_at`, in Unix seconds. Anonymous principals use an empty subject and
    /// carry grants available without an identity.
    ///
    /// # Panics
    /// Panics if claim serialization fails.
    #[must_use]
    pub fn mint(&self, principal: &Principal, grants: &[Grant], issued_at: i64, ttl_secs: i64) -> String {
        self.mint_with_id(
            principal,
            grants,
            issued_at,
            ttl_secs,
            &uuid::Uuid::new_v4().to_string(),
            REALM_SCOPE,
        )
    }

    #[must_use]
    pub fn mint_scoped(
        &self,
        scope: TokenScope,
        principal: &Principal,
        grants: &[Grant],
        issued_at: i64,
        ttl_secs: i64,
        token_id: &str,
    ) -> String {
        self.mint_with_id(principal, grants, issued_at, ttl_secs, token_id, scope)
    }

    #[must_use]
    fn mint_with_id(
        &self,
        principal: &Principal,
        grants: &[Grant],
        issued_at: i64,
        ttl_secs: i64,
        token_id: &str,
        scope: TokenScope,
    ) -> String {
        let claims = MintedClaims {
            sub: match principal {
                Principal::Anonymous => "",
                Principal::Named { subject } => subject,
            },
            aud: &self.audience,
            iat: issued_at,
            // Clamp unchecked TTL overflow to the far future; wrapping would expire the token at mint.
            exp: issued_at.checked_add(ttl_secs).unwrap_or(i64::MAX),
            jti: token_id,
            purpose: scope.as_str(),
            grants,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .expect("HS256 signing of serializable claims cannot fail")
    }

    /// Rejects altered or expired tokens, invalid signatures, and wrong audiences.
    ///
    /// # Errors
    /// Returns [`TokenError`] when the token fails signature, structure, audience, or expiry validation.
    pub fn verify(&self, token: &str) -> Result<(Principal, Vec<Grant>), TokenError> {
        let token = self.verify_identified(token)?;
        Ok((token.principal, token.grants))
    }

    fn verify_identified(&self, token: &str) -> Result<VerifiedToken, TokenError> {
        self.verify_for(token, REALM_SCOPE)
    }

    /// # Errors
    /// Returns [`TokenError`] when the token is invalid or belongs to another scope.
    pub fn verify_scoped(&self, token: &str, scope: TokenScope) -> Result<VerifiedToken, TokenError> {
        self.verify_for(token, scope)
    }

    fn verify_for(&self, token: &str, scope: TokenScope) -> Result<VerifiedToken, TokenError> {
        let claims = jsonwebtoken::decode::<VerifiedClaims>(token, &self.decoding, &self.validation)
            .map_err(TokenError)?
            .claims;
        // Pre-scope tokens omitted `purpose` and belong to the realm.
        if claims.purpose != scope.as_str() && !(claims.purpose.is_empty() && scope == REALM_SCOPE) {
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
    purpose: &'a str,
    grants: &'a [Grant],
}

#[derive(Deserialize)]
struct VerifiedClaims {
    sub: String,
    #[serde(default)]
    jti: String,
    #[serde(default)]
    purpose: String,
    grants: Vec<Grant>,
}
