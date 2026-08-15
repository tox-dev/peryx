//! Dashboard sessions authenticate the read/UI surface. Registry mutations require
//! `Authorization`-header credentials, so session cookies grant no write authority.
//!
//! `ChaCha20-Poly1305` encrypts and authenticates cookie payloads. HKDF derives a separate key for each
//! purpose, preventing substitution between session and pre-authentication cookies.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hkdf::{HKDF_SHA256, Salt};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ServerUser;

pub const SESSION_COOKIE: &str = "peryx_session";
pub const PRE_AUTH_COOKIE: &str = "peryx_login";

const HKDF_SALT: &[u8] = b"peryx-identity-session-hkdf-salt-v1";
const SESSION_INFO: &[u8] = b"peryx browser session v1";
const PRE_AUTH_INFO: &[u8] = b"peryx browser pre-auth v1";

/// Servers reject stolen cookies after their absolute expiry.
#[derive(Serialize, serde::Deserialize)]
struct Envelope<T> {
    exp: i64,
    data: T,
}

pub struct SessionSealer {
    session_key: LessSafeKey,
    pre_auth_key: LessSafeKey,
    rng: SystemRandom,
}

impl SessionSealer {
    #[must_use]
    pub fn new(signing_key: &[u8]) -> Self {
        Self {
            session_key: derive_key(signing_key, SESSION_INFO),
            pre_auth_key: derive_key(signing_key, PRE_AUTH_INFO),
            rng: SystemRandom::new(),
        }
    }

    /// `expires_at` is an absolute Unix timestamp.
    #[must_use]
    pub fn seal_session(&self, user: &ServerUser, expires_at: i64) -> String {
        self.seal(&self.session_key, user, expires_at)
    }

    /// Returns the user for an authentic cookie whose expiry is after `now`, in Unix seconds.
    #[must_use]
    pub fn open_session(&self, value: &str, now: i64) -> Option<ServerUser> {
        Self::open(&self.session_key, value, now)
    }

    /// `expires_at` is an absolute Unix timestamp; callers enforce single use.
    #[must_use]
    pub fn seal_pre_auth<T: Serialize>(&self, handoff: &T, expires_at: i64) -> String {
        self.seal(&self.pre_auth_key, handoff, expires_at)
    }

    /// Returns the handoff for an authentic cookie whose expiry is after `now`, in Unix seconds.
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
