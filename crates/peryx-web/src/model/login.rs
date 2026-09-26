use serde::{Deserialize, Serialize};

/// The browser login page's state: the signed-in user's display name, if any, and the OIDC providers a
/// visitor can sign in with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLoginState {
    pub user: Option<String>,
    pub providers: Vec<String>,
}

impl UiLoginState {
    /// Whether the login page has anything to offer: a session to end or a provider to sign in with.
    #[must_use]
    pub const fn offers_sign_in(&self) -> bool {
        self.user.is_some() || !self.providers.is_empty()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/model/login/tests.rs"]
mod tests;
