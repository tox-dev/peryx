use serde::{Deserialize, Serialize};

/// The browser login page's state: the signed-in user's display name, if any, and the OIDC providers a
/// visitor can sign in with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLoginState {
    pub user: Option<String>,
    pub providers: Vec<String>,
}

impl UiLoginState {
    /// Read the state from the `/_/session` document the hydrated browser fetches.
    #[must_use]
    pub fn from_session(value: &serde_json::Value) -> Self {
        let user = value["user"]["name"].as_str().map(str::to_owned);
        let providers = value["providers"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self { user, providers }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/model/login/tests.rs"]
mod tests;
