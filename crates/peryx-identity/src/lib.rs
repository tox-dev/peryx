//! Protocols reduce credentials and resources to [`Principal`], [`Action`], and ACL values before
//! calling [`authorize`], keeping access rules independent of wire protocols.
//!
//! [`Signer`] owns the token realm's signing key. Token endpoints supply approved grants without
//! handling the key.
//!
//! Persistent users use the separate, deny-by-default [`grants_permit`] model. Fixed [`Role`] values
//! bind a [`UserId`] to a [`GrantScope`]; per-index named tokens continue to use [`authorize`].

mod acl;
mod external;
mod ldap;
mod oidc;
mod oidc_http;
mod oidc_login;
mod password;
mod revocation;
mod roles;
mod scoped_token;
mod session;
mod token;
mod user;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub use acl::{
    Action, Denial, Glob, Grant, Identity, IndexAcl, NamedToken, Principal, ResourceMatch, authorize, authorize_all,
    authorize_grants,
};
pub use external::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityError, ExternalIdentityLinker,
    ExternalIdentityResolution, ExternalIdentityStore, ExternalLinkRequest, ExternalLogin, ExternalSubject,
    MAX_EXTERNAL_GROUPS, ManagedRoleGrant, ProviderId,
};
pub use ldap::{
    LdapBindMode, LdapLoginError, LdapLoginService, LdapProvider, LdapProviderBuildError, LdapProviderError,
    LdapProviderSettings,
};
pub use oidc::{OidcTokenVerifier, OidcVerificationError, OidcVerifier, VerifiedOidcIdentity};
pub use oidc_http::OidcHttpTransport;
pub use oidc_login::{
    Authorization, CallbackResponse, OidcLoginError, OidcLoginProvider, OidcLoginService, OidcProviderBuildError,
    OidcProviderError, OidcProviderSettings, PendingLogin,
};
pub use password::{PasswordCheck, PasswordError, PasswordPolicy, PasswordVerifier};
pub use revocation::{ArtifactDigest, ArtifactDigestError, DigestDecision, RevocationReason, RevocationReasonError};
pub use roles::{GrantScope, Resource, Role, RoleGrant, Scope, can_manage_grants, grants_permit};
pub use scoped_token::{TokenId, TokenName, TokenNameError, TokenSecret, TokenVerifier};
pub use session::{PRE_AUTH_COOKIE, SESSION_COOKIE, SessionSealer};
pub use token::{Signer, TokenError, TokenScope, VerifiedToken};
pub use user::{ServerUser, UserId, UserLifecycleChange, UserLifecycleEvent, UserName, UserNameError, UserState};

pub const TOKEN_AUDIENCE: &str = "peryx";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicCredentials {
    pub user: String,
    pub password: String,
}

/// Returns `None` unless the value contains a Basic scheme, valid base64 UTF-8, and a
/// `user:password` separator.
#[must_use]
pub fn parse_basic(header_value: &str) -> Option<BasicCredentials> {
    let encoded = strip_auth_scheme(header_value, "Basic")?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (user, password) = credentials.split_once(':')?;
    Some(BasicCredentials {
        user: user.to_owned(),
        password: password.to_owned(),
    })
}

/// HTTP authentication schemes compare case-insensitively while their credentials remain case-sensitive.
#[must_use]
pub fn strip_auth_scheme<'a>(header_value: &'a str, scheme: &str) -> Option<&'a str> {
    let (presented, credential) = header_value.split_at_checked(scheme.len())?;
    let credential = credential.strip_prefix(' ')?;
    presented.eq_ignore_ascii_case(scheme).then_some(credential)
}

/// Compares equal-length secrets without exposing the matching-prefix length through response time.
/// Secret length is public; `black_box` prevents the optimizer from restoring an early exit.
fn secrets_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (presented, expected) in presented.iter().zip(expected) {
        difference |= presented ^ expected;
    }
    std::hint::black_box(difference) == 0
}

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
