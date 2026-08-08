//! Sealed browser-session cookies for the read-only web UI.
//!
//! A human who signs in to the dashboard (through `OpenID Connect` today, LDAP next) gets a short-lived
//! session that authenticates only the read/UI surface. Mutating registry requests stay on
//! `Authorization`-header credentials, so a session cookie is never a write credential and carries no
//! CSRF surface to defend.
//!
//! Both the session and the pre-authentication handoff ride *sealed* cookies: the payload is encrypted
//! and authenticated with `ChaCha20-Poly1305` under a key derived from the token-realm signing key, so a
//! client can neither read a cookie's contents nor forge one. Each purpose derives its own key from the
//! same secret through HKDF domain separation, so a session cookie and a pre-auth cookie are not
//! interchangeable even though they share the signing key.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hkdf::{HKDF_SHA256, Salt};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ServerUser;

/// The name of the cookie that carries a signed-in user's sealed session.
pub const SESSION_COOKIE: &str = "peryx_session";
/// The name of the cookie that carries the sealed, single-use login handoff between the authorization
/// redirect and the provider callback.
pub const PRE_AUTH_COOKIE: &str = "peryx_login";

const HKDF_SALT: &[u8] = b"peryx-identity-session-hkdf-salt-v1";
const SESSION_INFO: &[u8] = b"peryx browser session v1";
const PRE_AUTH_INFO: &[u8] = b"peryx browser pre-auth v1";

/// A sealed payload and its absolute expiry, so a cookie stolen after it lapses is inert.
#[derive(Serialize, serde::Deserialize)]
struct Envelope<T> {
    exp: i64,
    data: T,
}

/// Seals and opens the browser-session and pre-authentication cookies.
///
/// Built once from the token-realm signing key and shared across requests; sealing and opening take
/// `&self`. Holds one `ChaCha20-Poly1305` key per cookie purpose, each HKDF-derived from the signing key
/// under a distinct info label.
pub struct SessionSealer {
    session_key: LessSafeKey,
    pre_auth_key: LessSafeKey,
    rng: SystemRandom,
}

impl SessionSealer {
    /// Derive the per-purpose sealing keys from the token-realm `signing_key`.
    #[must_use]
    pub fn new(signing_key: &[u8]) -> Self {
        Self {
            session_key: derive_key(signing_key, SESSION_INFO),
            pre_auth_key: derive_key(signing_key, PRE_AUTH_INFO),
            rng: SystemRandom::new(),
        }
    }

    /// Seal `user` into a session cookie value that stops being accepted at `expires_at` (unix seconds).
    #[must_use]
    pub fn seal_session(&self, user: &ServerUser, expires_at: i64) -> String {
        self.seal(&self.session_key, user, expires_at)
    }

    /// Open a session cookie value, returning the sealed user only when the cookie is authentic and has
    /// not expired at `now` (unix seconds).
    #[must_use]
    pub fn open_session(&self, value: &str, now: i64) -> Option<ServerUser> {
        Self::open(&self.session_key, value, now)
    }

    /// Seal a login `handoff` into a single-use pre-authentication cookie value that lapses at
    /// `expires_at` (unix seconds).
    #[must_use]
    pub fn seal_pre_auth<T: Serialize>(&self, handoff: &T, expires_at: i64) -> String {
        self.seal(&self.pre_auth_key, handoff, expires_at)
    }

    /// Open a pre-authentication cookie value, returning the sealed handoff only when the cookie is
    /// authentic and unexpired at `now` (unix seconds).
    #[must_use]
    pub fn open_pre_auth<T: DeserializeOwned>(&self, value: &str, now: i64) -> Option<T> {
        Self::open(&self.pre_auth_key, value, now)
    }

    fn seal<T: Serialize>(&self, key: &LessSafeKey, data: &T, expires_at: i64) -> String {
        let mut sealed = serde_json::to_vec(&Envelope { exp: expires_at, data }).expect("a session payload serializes");
        let mut nonce = [0_u8; NONCE_LEN];
        self.rng.fill(&mut nonce).expect("the system rng fills a nonce");
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed)
            .expect("sealing a session payload never fails");
        let mut envelope = Vec::with_capacity(NONCE_LEN + sealed.len());
        envelope.extend_from_slice(&nonce);
        envelope.append(&mut sealed);
        URL_SAFE_NO_PAD.encode(&envelope)
    }

    fn open<T: DeserializeOwned>(key: &LessSafeKey, value: &str, now: i64) -> Option<T> {
        let raw = URL_SAFE_NO_PAD.decode(value).ok()?;
        if raw.len() <= NONCE_LEN {
            return None;
        }
        let (nonce, cipher) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut sealed = cipher.to_vec();
        let plain = key.open_in_place(nonce, Aad::empty(), &mut sealed).ok()?;
        let envelope: Envelope<T> = serde_json::from_slice(plain).ok()?;
        (envelope.exp > now).then_some(envelope.data)
    }
}

/// Derive a `ChaCha20-Poly1305` key from `secret` under the HKDF info `label`, so each cookie purpose
/// gets an independent key from the one signing secret.
fn derive_key(secret: &[u8], label: &[u8]) -> LessSafeKey {
    let prk = Salt::new(HKDF_SHA256, HKDF_SALT).extract(secret);
    let mut material = [0_u8; 32];
    prk.expand(&[label], &CHACHA20_POLY1305)
        .and_then(|okm| okm.fill(&mut material))
        .expect("expanding a fixed-length aead key from hkdf never fails");
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &material).expect("a 32-byte chacha20-poly1305 key is valid");
    LessSafeKey::new(unbound)
}

#[cfg(test)]
#[path = "../tests/unit/session_tests.rs"]
mod tests;
