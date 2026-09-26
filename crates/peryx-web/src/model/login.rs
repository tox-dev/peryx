use serde::{Deserialize, Serialize};

/// The browser login page's state: the signed-in user's display name, if any, the OIDC providers a
/// visitor can sign in with, and whether the local name and password form works.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLoginState {
    pub user: Option<String>,
    pub providers: Vec<String>,
    pub local: bool,
}

impl UiLoginState {
    /// Whether the login page has anything to offer: a session to end or a way to sign in.
    #[must_use]
    pub const fn offers_sign_in(&self) -> bool {
        self.user.is_some() || self.local || !self.providers.is_empty()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/model/login/tests.rs"]
mod tests;
