//! The access model: who a request speaks as, what an index grants, and the decision between them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{parse_basic, secrets_match};

/// The subject the `upload_token` shorthand authenticates as, and the name a configured token may not
/// take.
pub const UPLOAD_TOKEN_NAME: &str = "upload_token";

/// Whether `principal` may take `action` on `project` in the index `acl` describes.
///
/// `project` is `None` when the caller must decide before it knows the name — a `PyPI` upload is
/// authorized before its multipart body is read — and then asks the weaker question: may this
/// principal take the action on *any* project here? The named check follows once the name is known.
///
/// # Errors
/// Returns [`Denial::Unavailable`] when no token in the index grants the action to anyone (the
/// capability is off, not a credential problem), [`Denial::Unauthenticated`] when the request
/// presented no credential that the action would need, and [`Denial::Forbidden`] when the principal
/// is known but holds no grant covering this project and action.
pub fn authorize(principal: &Principal, acl: &IndexAcl, project: Option<&str>, action: Action) -> Result<(), Denial> {
    if action == Action::Read && acl.anonymous_read {
        return Ok(());
    }
    match principal {
        Principal::Named { subject } => acl
            .token(subject)
            .ok_or(Denial::Forbidden)
            .and_then(|token| authorize_grants(&token.grants, project, action)),
        Principal::Anonymous if acl.grants_to_anyone(action) => Err(Denial::Unauthenticated),
        Principal::Anonymous => Err(Denial::Unavailable),
    }
}

/// Apply the grants recovered from a verified token without resolving its subject through an index ACL.
///
/// # Errors
/// Returns `Denial::Forbidden` when no grant covers the project and action.
pub fn authorize_grants(grants: &[Grant], project: Option<&str>, action: Action) -> Result<(), Denial> {
    grants
        .iter()
        .any(|grant| grant.allows(project, action))
        .then_some(())
        .ok_or(Denial::Forbidden)
}

/// Who a request speaks as, once its credential was checked. A credential that matched nothing leaves
/// the request anonymous, so an invalid token is exactly as privileged as no token at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    Named { subject: String },
}

/// What a request wants to do. The three verbs every ecosystem's protocol maps onto: a pull is a read,
/// a push is a write, and a removal is a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Read,
    Write,
    Delete,
}

/// Why a request was refused. The three cases carry different HTTP answers: an ecosystem tells a client
/// with no credential to authenticate, and one with a valid but insufficient credential not to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// No token grants this action to anyone on this index.
    Unavailable,
    /// The request carried no credential the action accepts.
    Unauthenticated,
    /// The principal is known and lacks a grant for this project and action.
    Forbidden,
}

/// One index's access rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexAcl {
    /// Whether a request with no credential may read. Defaults to `true`, the behavior of every index
    /// peryx served before it had an ACL.
    pub anonymous_read: bool,
    pub tokens: Vec<NamedToken>,
}

impl Default for IndexAcl {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            tokens: Vec::new(),
        }
    }
}

impl IndexAcl {
    /// The ACL of an index configured with nothing but the legacy `upload_token`: one token that writes
    /// and deletes everywhere, and open reads.
    #[must_use]
    pub fn upload_token(secret: impl Into<String>) -> Self {
        Self {
            anonymous_read: true,
            tokens: vec![NamedToken::upload(secret)],
        }
    }

    /// Resolve an `Authorization` header against this ACL at `now` (unix seconds). A header that is
    /// absent, unparsable, or carries a password matching no live token yields [`Principal::Anonymous`].
    #[must_use]
    pub fn identify(&self, header: Option<&str>, now: i64) -> Identity {
        let Some(credentials) = header.and_then(parse_basic) else {
            return Identity {
                principal: Principal::Anonymous,
                user: None,
            };
        };
        let principal = self
            .tokens
            .iter()
            .find(|token| token.live(now) && secrets_match(&credentials.password, &token.secret))
            .map_or(Principal::Anonymous, |token| Principal::Named {
                subject: token.name.clone(),
            });
        Identity {
            principal,
            user: Some(credentials.user),
        }
    }

    /// The grants a principal holds here, which a token endpoint intersects with what a client asked
    /// for. Anonymous requests hold none: an anonymous read is `anonymous_read`, not a grant.
    #[must_use]
    pub fn grants(&self, principal: &Principal) -> &[Grant] {
        match principal {
            Principal::Anonymous => &[],
            Principal::Named { subject } => self.token(subject).map_or(&[], |token| token.grants.as_slice()),
        }
    }

    /// Whether any token here grants `action` to anyone, which is what an index means by "uploads are
    /// enabled": a capability the index offers to some credential, not one this request holds.
    #[must_use]
    pub fn grants_to_anyone(&self, action: Action) -> bool {
        self.tokens
            .iter()
            .any(|token| token.grants.iter().any(|grant| grant.actions.contains(&action)))
    }

    fn token(&self, name: &str) -> Option<&NamedToken> {
        self.tokens.iter().find(|token| token.name == name)
    }
}

/// A request's resolved identity: the principal every access decision runs against, and the username
/// its credential presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub principal: Principal,
    /// The username the Basic credential carried, whether or not it authenticated. Audit context only:
    /// an unverified name proves nothing, so it is never an input to [`authorize`].
    pub user: Option<String>,
}

/// One credential an index accepts, and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedToken {
    /// The subject a request authenticating with this token speaks as.
    pub name: String,
    pub secret: String,
    pub grants: Vec<Grant>,
    /// Unix seconds after which the token stops authenticating; `None` never expires.
    pub expires_at: Option<i64>,
}

impl NamedToken {
    /// The token a legacy `upload_token` stands for: write and delete on every project, forever.
    #[must_use]
    pub fn upload(secret: impl Into<String>) -> Self {
        Self {
            name: UPLOAD_TOKEN_NAME.to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                projects: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }
    }

    fn live(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|expiry| now < expiry)
    }
}

/// A set of actions over a set of project globs. A token carries one grant per scope it was issued
/// for, and a minted JWT carries the grants a token endpoint approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub projects: Vec<Glob>,
    pub actions: BTreeSet<Action>,
}

impl Grant {
    fn allows(&self, project: Option<&str>, action: Action) -> bool {
        self.actions.contains(&action)
            && project.is_none_or(|project| self.projects.iter().any(|glob| glob.matches(project)))
    }
}

/// A project pattern.
///
/// `*` stands for any run of characters, `/` included, so `team/*` covers every repository under
/// `team` however deeply nested, and `*` covers the whole index. Every other character matches itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Glob(String);

impl Glob {
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    /// Whether `project` matches this pattern, by the usual backtracking wildcard walk: on a mismatch,
    /// return to the last `*` and let it swallow one more character.
    #[must_use]
    pub fn matches(&self, project: &str) -> bool {
        let (pattern, project) = (self.0.as_bytes(), project.as_bytes());
        let (mut at, mut cursor) = (0, 0);
        let (mut star, mut resume) = (None, 0);
        while cursor < project.len() {
            if pattern.get(at) == Some(&b'*') {
                star = Some(at);
                resume = cursor;
                at += 1;
            } else if pattern.get(at) == Some(&project[cursor]) {
                at += 1;
                cursor += 1;
            } else if let Some(position) = star {
                at = position + 1;
                resume += 1;
                cursor = resume;
            } else {
                return false;
            }
        }
        pattern[at..].iter().all(|byte| *byte == b'*')
    }
}
