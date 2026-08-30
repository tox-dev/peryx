use serde::{Deserialize, Serialize};

/// The browser login page's state: the signed-in user's display name, if any, and the OIDC providers a
/// visitor can sign in with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLoginState {
    pub user: Option<String>,
    pub providers: Vec<String>,
}
