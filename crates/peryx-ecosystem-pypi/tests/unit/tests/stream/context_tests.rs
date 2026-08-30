use std::collections::BTreeMap;

use super::page_context;
use crate::Yanked;
use crate::store::FileOverride;

#[test]
fn test_an_override_that_imposes_nothing_neither_hides_nor_yanks() {
    let overrides = BTreeMap::from([("demo-1.0-py3-none-any.whl".to_owned(), FileOverride::default())]);

    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);

    assert!(context.skip.is_empty());
    assert!(context.hidden.is_empty());
    assert!(context.yanked.is_empty());
}

#[test]
fn test_a_hidden_and_yanked_file_is_both_skipped_and_yanked() {
    let overrides = BTreeMap::from([(
        "demo-1.0-py3-none-any.whl".to_owned(),
        FileOverride {
            hidden: true,
            yanked: Yanked::Reason("CVE-2026-1234".to_owned()),
        },
    )]);

    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);

    assert!(context.skip.contains("demo-1.0-py3-none-any.whl"));
    assert!(context.hidden.contains("demo-1.0-py3-none-any.whl"));
    assert_eq!(
        context.yanked.get("demo-1.0-py3-none-any.whl"),
        Some(&Yanked::Reason("CVE-2026-1234".to_owned()))
    );
}
