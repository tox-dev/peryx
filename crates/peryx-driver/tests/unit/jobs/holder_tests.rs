use super::{new_holder, node_holder};

#[test]
fn test_two_incarnations_with_one_process_id_mint_distinct_holder_tokens() {
    let prefix = format!("node-{}-", std::process::id());

    let (first, second) = (new_holder(), new_holder());

    assert!(first.starts_with(&prefix), "{first} names its process");
    assert!(second.starts_with(&prefix), "{second} names its process");
    assert_ne!(first, second);
}

#[test]
fn test_one_incarnation_keeps_the_holder_token_it_minted() {
    assert_eq!(node_holder(), node_holder());
}
