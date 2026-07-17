//! Trusted publishers: authorizing a CI identity to publish without a long-lived upload secret.
//!
//! A CI job already proves who it is to its platform, and its platform hands it a short-lived OIDC
//! identity token asserting that proof — the workflow, the repository, the environment it ran in. A
//! trusted publisher is an operator's standing decision that one such identity may publish here: match
//! the token's issuer, audience, subject and required claims against the configured rules, and the job
//! earns a scoped upload grant for the moment it needs one, with no rotating secret to leak.
//!
//! This is the neutral half of the exchange, so it lives beside [`authorize`](crate::authorize) rather
//! than in an ecosystem: it turns verified claims into the same [`Grant`] list a token endpoint feeds
//! [`Signer::mint`](crate::Signer::mint), and the minted token then travels the ordinary Bearer upload
//! path. It deliberately stops at the claims: recovering them from a signed token means fetching and
//! trusting the issuer's public keys over the network, a separable layer that verifies the signature
//! before these rules ever see a claim.

use std::collections::{BTreeMap, BTreeSet};

use crate::{Action, Glob, Grant};

/// Authorize a CI identity's claims against the configured publishers at `now` (unix seconds).
///
/// The write grant of the first rule that matches is returned. When none match, the most specific
/// reason wins: a rule that recognized the issuer but rejected a claim is more actionable than one that
/// never applied, so its denial is the one a CI job sees. No publishers means nothing trusts the issuer.
///
/// # Errors
/// Returns the [`PublishDenial`] explaining why no configured publisher accepted the token.
pub fn authorize_publish(
    publishers: &[TrustedPublisher],
    claims: &PublishClaims,
    now: i64,
) -> Result<Vec<Grant>, PublishDenial> {
    authorize_publish_index(publishers, claims, now).map(|(_, grants)| grants)
}

pub fn authorize_publish_index(
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

/// One trusted publisher rule: an OIDC identity an operator has decided may publish, and the projects
/// its minted token may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublisher {
    /// The `iss` a token must carry, matched exactly: the CI platform that vouched for the job.
    pub issuer: String,
    /// The `aud` a token must carry, matched exactly: the identity token was minted for this server, so
    /// one stolen from another audience does not authenticate here.
    pub audience: String,
    /// The pattern the token's `sub` must match, for example `repo:octo/app:ref:refs/heads/main`. A glob
    /// so one rule covers a repository's branches or a workflow's jobs without listing each.
    pub subject: Glob,
    /// Further claims a token must carry verbatim, keyed by claim name (`repository`, `environment`).
    /// Every entry must match; an empty map requires none beyond the subject.
    pub claims: BTreeMap<String, String>,
    /// The project globs a token minted for this publisher may write. The upload path enforces them like
    /// any other grant, so a matched identity still cannot push outside what the rule scoped it to.
    pub projects: Vec<Glob>,
}

impl TrustedPublisher {
    /// The write grant a token matching this rule earns, or why it did not match, checked in the order a
    /// reader reasons about trust: the wrong issuer is not this publisher's concern, an expired token is
    /// spent whoever configured it, and only then do subject and claims decide.
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
            projects: self.projects.clone(),
            actions: BTreeSet::from([Action::Write]),
        }])
    }
}

/// The claims recovered from a CI identity token once its signature has been verified against the
/// issuer. Signature verification is the caller's job; these rules judge only what the claims assert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishClaims {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    /// The token's `exp`, in unix seconds. A rule rejects a token at or past it.
    pub expires_at: i64,
    /// Every other claim the token carried, for a rule's [`claims`](TrustedPublisher::claims) to match.
    pub claims: BTreeMap<String, String>,
}

/// Why no configured publisher accepted a token. Each carries the actionable answer a CI job needs: an
/// unknown issuer means no rule was written for it, a claim mismatch names the claim that disagreed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishDenial {
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
    /// How far a rule got before rejecting the token, so [`authorize_publish`] can surface the reason
    /// from the rule that came closest to trusting it.
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
