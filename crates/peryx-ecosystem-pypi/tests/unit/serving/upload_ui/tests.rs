use super::escape;

#[test]
fn escape_encodes_html_metacharacters() {
    assert_eq!(escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
}
