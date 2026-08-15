use std::collections::BTreeSet;

use peryx::config::{SecretSource, TokenConfig};
use peryx_identity::Action;

pub fn writer_token(secret: SecretSource) -> TokenConfig {
    TokenConfig {
        name: "uploader".to_owned(),
        secret,
        resources: vec!["*".to_owned()],
        actions: BTreeSet::from([Action::Write, Action::Delete]),
        expires_at: None,
    }
}
