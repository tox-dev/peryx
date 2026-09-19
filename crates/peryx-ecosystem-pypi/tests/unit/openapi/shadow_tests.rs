use super::shadow_example;

#[test]
fn test_shadow_example_carries_the_documented_candidate_rows() {
    let example = shadow_example();

    assert_eq!(example["candidates"][0]["member"], "hosted");
    assert_eq!(example["candidates"][1]["decision"]["rule"], "blocked-subject");
}
