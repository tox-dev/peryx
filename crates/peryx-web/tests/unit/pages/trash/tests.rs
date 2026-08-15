use leptos::prelude::*;

use crate::model::UiTrashPage;

use super::{Trash, trash_page, trash_results};

#[test]
fn trash_page_renders_query_controls() {
    let owner = Owner::new();
    owner.set();
    let html = view! { <Trash /> }.to_html();
    for expected in ["Trash", r#"id="trash-state""#, "Enter credentials and search"] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[test]
fn trash_page_renders_result_states() {
    let page: UiTrashPage = serde_json::from_value(serde_json::json!({
        "trash": [{
            "ecosystem": "example", "repository": "root/hosted", "resource": "artifact",
            "artifact": "artifact.bin", "digest": null, "reason": null, "actor": null,
            "deleted_at_unix": 0, "deadline_unix": 86400, "state": "restorable", "restorable": true
        }],
        "next_cursor": "next"
    }))
    .expect("trash page is valid");
    let html = trash_page(page).to_html();
    for expected in [
        "Loaded 1 trash records.",
        r#"class="badge trash-restorable">Restorable</span>"#,
        "<td>artifact.bin</td>",
        "<td>-</td>",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }

    for (loading, result, expected) in [
        (true, None, "Loading trash..."),
        (false, None, "Enter credentials and search to load trash records."),
        (false, Some(Err("denied".to_owned())), "denied"),
        (
            false,
            Some(Ok(UiTrashPage {
                trash: Vec::new(),
                next_cursor: None,
            })),
            "No trash records matched",
        ),
    ] {
        let html = trash_results(loading, result).to_html();
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}
