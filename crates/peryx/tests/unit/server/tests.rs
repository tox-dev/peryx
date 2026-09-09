use peryx_core::Ecosystem;

use super::{Config, WebhookSecret, build_webhooks};
use crate::config::WebhookConfig;

#[test]
fn test_build_webhooks_names_the_index_whose_events_it_cannot_resolve() {
    // The registry answers with the events an ecosystem publishes and fails only for one it does not
    // hold, so an index naming an uninstalled ecosystem is what reaches the failure.
    let mut indexes = Config::default().indexes;
    indexes[0].ecosystem = Ecosystem::new("missing");
    indexes[0].webhooks = vec![WebhookConfig {
        name: "audit".to_owned(),
        url: "https://hooks.example/audit".to_owned(),
        secret: WebhookSecret::Literal("shared".to_owned()),
        events: vec!["artifact.published".to_owned()],
    }];

    // `WebhookRuntime` carries no `Debug`, so the success value goes before the error comes out.
    let error = build_webhooks(&indexes, &crate::compiled_plugins())
        .map(drop)
        .unwrap_err();

    assert_eq!(
        format!("{error:#}"),
        format!(
            "resolve webhook events for {}: ecosystem missing is not installed",
            indexes[0].name
        )
    );
}
