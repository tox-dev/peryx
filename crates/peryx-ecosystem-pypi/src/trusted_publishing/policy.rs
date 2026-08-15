use std::collections::{BTreeMap, BTreeSet};

use peryx_identity::{Action, Glob, Grant};

pub(super) fn authorize_publish(
    publishers: &[TrustedPublisher],
    claims: &PublishClaims,
    now: i64,
) -> Result<(usize, Vec<Grant>), PublishDenial> {
    let mut denial = PublishDenial::UnknownIssuer;
    for (position, publisher) in publishers.iter().enumerate() {
        match publisher.authorize(claims, now) {
            Ok(grants) => return Ok((position, grants)),
            Err(reason) if reason.rank() >= denial.rank() => denial = reason,
            Err(_) => {}
        }
    }
    Err(denial)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TrustedPublisher {
    pub issuer: String,
    pub audience: String,
    pub subject: Glob,
    pub claims: BTreeMap<String, String>,
    pub projects: Vec<Glob>,
}

impl TrustedPublisher {
    fn authorize(&self, claims: &PublishClaims, now: i64) -> Result<Vec<Grant>, PublishDenial> {
        if self.issuer != claims.issuer {
            return Err(PublishDenial::UnknownIssuer);
        }
        if self.audience != claims.audience {
            return Err(PublishDenial::WrongAudience);
        }
        if now >= claims.expires_at {
            return Err(PublishDenial::Expired);
        }
        if !self.subject.matches(&claims.subject) {
            return Err(PublishDenial::WrongSubject);
        }
        for (claim, expected) in &self.claims {
            if claims.claims.get(claim).map(String::as_str) != Some(expected) {
                return Err(PublishDenial::ClaimMismatch { claim: claim.clone() });
            }
        }
        Ok(vec![Grant {
            resources: self.projects.clone(),
            actions: BTreeSet::from([Action::Write]),
        }])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PublishClaims {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    pub expires_at: i64,
    pub claims: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum PublishDenial {
    #[error("no trusted publisher is configured for this token's issuer")]
    UnknownIssuer,
    #[error("the token's audience does not match the configured publisher")]
    WrongAudience,
    #[error("the token has expired")]
    Expired,
    #[error("the token's subject matches no configured publisher")]
    WrongSubject,
    #[error("the token is missing the required claim `{claim}` or carries a different value")]
    ClaimMismatch { claim: String },
}

impl PublishDenial {
    const fn rank(&self) -> u8 {
        match self {
            Self::UnknownIssuer => 0,
            Self::WrongAudience => 1,
            Self::Expired => 2,
            Self::WrongSubject => 3,
            Self::ClaimMismatch { .. } => 4,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/trusted_publishing/policy_tests.rs"]
mod tests;
