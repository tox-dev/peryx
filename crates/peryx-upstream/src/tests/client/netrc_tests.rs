use std::path::Path;

use crate::{Auth, Netrc};

fn write_netrc(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn load(contents: &str) -> (tempfile::TempDir, Netrc) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.netrc");
    write_netrc(&path, contents);
    let netrc = Netrc::from_path(&path).unwrap();
    (dir, netrc)
}

fn basic(username: &str, password: &str) -> Auth {
    Auth::Basic {
        username: username.to_owned(),
        password: password.to_owned(),
    }
}

#[test]
fn test_netrc_resolves_origin_before_authority_and_host() {
    let (_dir, netrc) = load(
        "machine example.com login host password host-pass\n\
         machine example.com:443 login authority password authority-pass\n\
         machine https://example.com:443 login origin password origin-pass\n",
    );

    assert_eq!(
        netrc.auth_for_str("https://example.com/simple/").unwrap(),
        basic("origin", "origin-pass")
    );
    assert_eq!(
        netrc.auth_for_str("http://example.com:443/simple/").unwrap(),
        basic("authority", "authority-pass")
    );
    assert_eq!(
        netrc.auth_for_str("https://example.com:8443/simple/").unwrap(),
        Auth::None
    );
}

#[test]
fn test_netrc_bare_host_matches_a_default_port() {
    let (_dir, netrc) = load("machine plain.example login reader password secret\n");
    assert_eq!(
        netrc.auth_for_str("https://plain.example/simple/").unwrap(),
        basic("reader", "secret")
    );
}

#[test]
fn test_netrc_resolves_default_and_ignores_empty_default() {
    let (_dir, netrc) = load("default login fallback password fallback-pass\n");
    assert_eq!(
        netrc.auth_for_str("https://missing.example/simple/").unwrap(),
        basic("fallback", "fallback-pass")
    );
    let (_dir, netrc) = load("default\n");
    assert_eq!(
        netrc.auth_for_str("https://missing.example/simple/").unwrap(),
        Auth::None
    );
}

#[test]
fn test_netrc_normalizes_ipv6_machine_names() {
    let (_dir, bracketed) = load("machine [2001:db8::1] login ipv6 password bracketed\n");
    assert_eq!(
        bracketed.auth_for_str("https://[2001:db8::1]/simple/").unwrap(),
        basic("ipv6", "bracketed")
    );
    let (_dir, unbracketed) = load("machine 2001:db8::1 login ipv6 password plain\n");
    assert_eq!(
        unbracketed.auth_for_str("https://[2001:db8::1]/simple/").unwrap(),
        basic("ipv6", "plain")
    );
    let (_dir, origin) = load("machine https://[2001:db8::2]:8443 login ipv6 password origin\n");
    assert_eq!(
        origin.auth_for_str("https://[2001:db8::2]:8443/simple/").unwrap(),
        basic("ipv6", "origin")
    );
}

#[test]
fn test_netrc_matches_ipv4_but_not_schemes_without_known_ports() {
    let (_dir, netrc) = load(
        "machine 192.0.2.1 login ipv4 password address\n\
         machine custom://example.com login custom password scheme\n",
    );
    assert_eq!(
        netrc.auth_for_str("https://192.0.2.1/simple/").unwrap(),
        basic("ipv4", "address")
    );
    assert_eq!(netrc.auth_for_str("custom://example.com/simple/").unwrap(), Auth::None);
}

#[test]
fn test_netrc_returns_anonymous_for_urls_without_hosts() {
    let (_dir, netrc) = load("default login fallback password fallback-pass\n");
    assert_eq!(netrc.auth_for_str("file:///tmp/pkg.whl").unwrap(), Auth::None);
    assert!(netrc.auth_for_str("not a URL").is_err());
}

#[test]
fn test_netrc_debug_redacts_credentials() {
    let (_dir, netrc) = load("machine example.com login reader password swordfish\n");
    let debug = format!("{netrc:?}");
    assert!(debug.contains("machine_count: 1"));
    assert!(!debug.contains("reader"));
    assert!(!debug.contains("swordfish"));
    assert_eq!(format!("{:?}", Auth::None), "None");
    assert_eq!(format!("{:?}", basic("reader", "swordfish")), "Basic(..)");
    assert_eq!(format!("{:?}", Auth::Bearer("swordfish".to_owned())), "Bearer(..)");
}

#[test]
fn test_netrc_reports_missing_and_non_regular_paths() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.netrc");
    let error = Netrc::from_path(&missing).unwrap_err();
    assert!(error.to_string().contains("cannot read netrc file"));
    assert!(
        Netrc::from_path(dir.path())
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
}

#[test]
fn test_netrc_parser_error_redacts_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.netrc");
    write_netrc(
        &path,
        "machine example.com login reader password swordfish invalid-token\n",
    );
    let message = Netrc::from_path(&path).unwrap_err().to_string();
    assert!(message.contains("has invalid syntax"));
    assert!(!message.contains("reader"));
    assert!(!message.contains("swordfish"));
    assert!(!message.contains("invalid-token"));
}

#[test]
fn test_netrc_invalid_utf8_is_a_redacted_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid-utf8.netrc");
    std::fs::write(&path, [0xff]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let message = Netrc::from_path(&path).unwrap_err().to_string();
    assert!(message.contains("cannot read netrc file"));
    assert!(!message.contains("�"));
}

#[cfg(unix)]
#[test]
fn test_netrc_rejects_group_or_other_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("public.netrc");
    std::fs::write(&path, "machine example.com login reader password secret\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let message = Netrc::from_path(&path).unwrap_err().to_string();
    assert!(message.contains("must not grant group or other permissions"));
    assert!(!message.contains("secret"));
}
