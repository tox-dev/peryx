use std::collections::HashMap;

use super::page_context;

#[test]
fn test_unknown_override_kind_is_ignored() {
    let overrides: HashMap<String, String> = [("demo-1.0-py3-none-any.whl".to_owned(), "frozen".to_owned())].into();
    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);
    assert!(context.skip.is_empty());
    assert!(context.yanked.is_empty());
}
