use rstest::rstest;

use super::{install_command, shell_quote};

#[rstest]
#[case::plain("flask", "flask")]
#[case::pinned("flask==1.2", "flask==1.2")]
#[case::extras("flask[async]", "'flask[async]'")]
#[case::whitespace("bad name", "'bad name'")]
#[case::embedded_quote("o'hara", r"'o'\''hara'")]
fn test_shell_quote(#[case] value: &str, #[case] expected: &str) {
    assert_eq!(shell_quote(value), expected);
}

#[rstest]
#[case::unpinned("flask", None, "uv pip install --index-url <origin>/root/packages/simple/ flask")]
#[case::pinned(
    "flask",
    Some("1.2.3"),
    "uv pip install --index-url <origin>/root/packages/simple/ flask==1.2.3"
)]
#[case::quoted(
    "flask[async]",
    Some("1.2.3"),
    "uv pip install --index-url <origin>/root/packages/simple/ 'flask[async]==1.2.3'"
)]
fn test_install_command(#[case] project: &str, #[case] version: Option<&str>, #[case] expected: &str) {
    assert_eq!(install_command("root/packages", project, version), expected);
}
