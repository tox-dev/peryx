use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::{PRE_AUTH_COOKIE, SESSION_COOKIE, SessionSealer};
use crate::{ServerUser, UserId, UserName, UserState};

const KEY: &[u8] = b"a-token-realm-signing-secret-32b!";

fn sealer() -> SessionSealer {
    SessionSealer::new(KEY)
}

fn user() -> ServerUser {
    ServerUser {
        id: UserId::random(),
        name: UserName::new("Ada Lovelace").unwrap(),
        state: UserState::Active,
        revision: 3,
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Handoff {
    state: String,
    nonce: String,
}

fn handoff() -> Handoff {
    Handoff {
        state: "s-123".to_owned(),
        nonce: "n-456".to_owned(),
    }
}

#[test]
fn test_cookie_names_are_stable() {
    assert_eq!(SESSION_COOKIE, "peryx_session");
    assert_eq!(PRE_AUTH_COOKIE, "peryx_login");
}

#[test]
fn test_a_sealed_session_round_trips_before_it_expires() {
    let sealer = sealer();
    let user = user();
    let cookie = sealer.seal_session(&user, 1_000);
    assert_eq!(sealer.open_session(&cookie, 999), Some(user));
}

#[test]
fn test_a_sealed_pre_auth_handoff_round_trips() {
    let sealer = sealer();
    let handoff = handoff();
    let cookie = sealer.seal_pre_auth(&handoff, 1_000);
    assert_eq!(sealer.open_pre_auth::<Handoff>(&cookie, 500), Some(handoff));
}

#[test]
fn test_an_expired_session_is_rejected() {
    let sealer = sealer();
    let cookie = sealer.seal_session(&user(), 1_000);
    assert_eq!(sealer.open_session(&cookie, 1_000), None);
    assert_eq!(sealer.open_session(&cookie, 1_001), None);
}

#[test]
fn test_an_expired_pre_auth_handoff_is_rejected() {
    let sealer = sealer();
    let cookie = sealer.seal_pre_auth(&handoff(), 1_000);
    assert_eq!(sealer.open_pre_auth::<Handoff>(&cookie, 1_000), None);
}

#[test]
fn test_a_session_key_does_not_open_a_pre_auth_cookie() {
    // Domain-separated keys prevent one cookie purpose from authenticating another.
    let sealer = sealer();
    let session = sealer.seal_session(&user(), 1_000);
    assert_eq!(sealer.open_pre_auth::<ServerUser>(&session, 0), None);

    let pre_auth = sealer.seal_pre_auth(&handoff(), 1_000);
    assert!(sealer.open_pre_auth::<Handoff>(&pre_auth, 0).is_some());
    assert_eq!(sealer.open_session(&pre_auth, 0), None);
}

#[test]
fn test_a_cookie_sealed_under_a_different_secret_is_rejected() {
    let cookie = sealer().seal_session(&user(), 1_000);
    let other = SessionSealer::new(b"a-completely-different-secret-key");
    assert_eq!(other.open_session(&cookie, 0), None);
}

#[test]
fn test_a_tampered_cookie_is_rejected() {
    let sealer = sealer();
    let cookie = sealer.seal_session(&user(), 1_000);
    let mut raw = URL_SAFE_NO_PAD.decode(&cookie).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0x01;
    let tampered = URL_SAFE_NO_PAD.encode(&raw);
    assert_eq!(sealer.open_session(&tampered, 0), None);
}

#[test]
fn test_a_non_base64_cookie_is_rejected() {
    assert_eq!(sealer().open_session("not*base64*value", 0), None);
}

#[test]
fn test_a_cookie_shorter_than_a_nonce_is_rejected() {
    let short = URL_SAFE_NO_PAD.encode([0_u8; 4]);
    assert_eq!(sealer().open_session(&short, 0), None);
}
