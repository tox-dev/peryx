//! The bearer tokens a pull reuses, bounded so a caller cannot grow them without limit.
//!
//! A proxy request names the repository, and the scope built from that name is part of the cache key,
//! so whoever is pulling chooses the keys. The cache therefore carries an entry budget, a
//! retained-byte budget, and an expiry on every entry, and it drops what no longer fits whenever an
//! exchange adds to it.

use std::collections::HashMap;

use peryx_upstream::{CredentialIdentity, CredentialProviderId};

/// The most tuples the cache retains. A proxy fronting a few registries keeps far fewer repositories
/// hot than this; the bound is here for the caller who is not pulling in good faith.
const MAX_ENTRIES: usize = 512;

/// The most key and token bytes the cache retains. A token response may itself reach a megabyte, so
/// the byte budget is what stops a handful of outsized tokens from costing more than many ordinary
/// ones.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The lifetime to assume when a realm sends no `expires_in`, which is what the distribution token
/// specification tells a client to do.
const DEFAULT_LIFETIME_SECS: i64 = 60;

/// The longest lifetime the cache honours, however long the realm claims. A token peryx holds past
/// this is a credential kept for no reason it can still justify.
const MAX_LIFETIME_SECS: i64 = 3600;

/// Retirement runs this far ahead of the realm's own deadline, so a clock a few seconds fast on
/// either side does not hand a request a token the registry has already stopped taking.
const EXPIRY_SKEW_SECS: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenCacheKey {
    pub base: String,
    pub scope: String,
    pub provider: CredentialProviderId,
}

impl TokenCacheKey {
    const fn weight(&self) -> usize {
        self.base.len() + self.scope.len()
    }
}

#[derive(Debug)]
struct Entry {
    credentials: CredentialIdentity,
    value: String,
    /// Unix seconds at which the entry stops being served.
    expires_at: i64,
    /// The cache's use counter as of this entry's last hit, which is what orders eviction.
    used_at: u64,
}

#[derive(Debug, Default)]
pub struct TokenCache {
    entries: HashMap<TokenCacheKey, Entry>,
    bytes: usize,
    uses: u64,
}

impl TokenCache {
    /// The live token for `key` issued to `identity`.
    ///
    /// A hit renews the entry's place in the eviction order, which is what keeps a repository under
    /// active pull ahead of a flood of single-use scopes: the entry a burst keeps hitting is the last
    /// one eviction reaches, so the burst still authenticates once.
    pub fn get(&mut self, key: &TokenCacheKey, identity: CredentialIdentity, now: i64) -> Option<String> {
        if self.entries.get(key).is_some_and(|entry| entry.expires_at <= now) {
            self.remove(key);
            return None;
        }
        self.uses += 1;
        let uses = self.uses;
        let entry = self.entries.get_mut(key)?;
        if entry.credentials != identity {
            return None;
        }
        entry.used_at = uses;
        Some(entry.value.clone())
    }

    /// Retain `value` under `key` until `expires_at`.
    ///
    /// Expired tuples leave here rather than waiting for a lookup that names them, so a scope pulled
    /// once and never again stops costing anything at the next exchange.
    pub fn insert(
        &mut self,
        key: TokenCacheKey,
        credentials: CredentialIdentity,
        value: String,
        expires_at: i64,
        now: i64,
    ) {
        self.sweep_expired(now);
        self.uses += 1;
        let weight = key.weight();
        self.bytes += weight + value.len();
        let entry = Entry {
            credentials,
            value,
            expires_at,
            used_at: self.uses,
        };
        if let Some(replaced) = self.entries.insert(key, entry) {
            self.bytes -= weight + replaced.value.len();
        }
        self.evict_to_budget();
    }

    /// The retained token values, for a test that asserts a burst produced one of them.
    #[cfg(test)]
    pub fn values(&self) -> Vec<String> {
        self.entries.values().map(|entry| entry.value.clone()).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    fn remove(&mut self, key: &TokenCacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= key.weight() + entry.value.len();
        }
    }

    fn sweep_expired(&mut self, now: i64) {
        let expired: Vec<TokenCacheKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.remove(&key);
        }
    }

    /// Drop the least recently served entries until both budgets hold again.
    fn evict_to_budget(&mut self) {
        if self.within_budget() {
            return;
        }
        let mut order: Vec<(u64, TokenCacheKey)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.used_at, key.clone()))
            .collect();
        order.sort_unstable_by_key(|(used_at, _)| *used_at);
        for (_, key) in order {
            if self.within_budget() {
                return;
            }
            self.remove(&key);
        }
    }

    fn within_budget(&self) -> bool {
        self.entries.len() <= MAX_ENTRIES && self.bytes <= MAX_BYTES
    }
}

/// What a token response said about its own lifetime, before peryx applies any policy to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredLifetime {
    /// The realm sent no `expires_in`.
    Absent,
    /// The realm sent a lifetime peryx could read, in seconds.
    Seconds(i64),
    /// The realm sent an `expires_in` that is not a number of seconds.
    Malformed,
}

/// When a token issued now stops being served, or `None` when it must not be retained at all.
///
/// The token is opaque and stays that way: the answer comes from the response's own fields and never
/// from decoding the value. A malformed or non-positive lifetime is not one peryx can honour, so that
/// token serves the request it was fetched for and is not cached. `issued_at` says how much of the
/// lifetime the realm has already spent; without it the clock starts at the exchange.
pub fn expires_at(lifetime: DeclaredLifetime, issued_at: Option<i64>, now: i64) -> Option<i64> {
    let declared = match lifetime {
        DeclaredLifetime::Absent => DEFAULT_LIFETIME_SECS,
        DeclaredLifetime::Malformed => return None,
        DeclaredLifetime::Seconds(seconds) if seconds <= 0 => return None,
        DeclaredLifetime::Seconds(seconds) => seconds,
    };
    let spent = issued_at.map_or(0, |issued_at| now.saturating_sub(issued_at).max(0));
    let remaining = declared.min(MAX_LIFETIME_SECS) - spent - EXPIRY_SKEW_SECS;
    (remaining > 0).then(|| now + remaining)
}

#[cfg(test)]
#[path = "../tests/unit/token_cache/tests.rs"]
mod tests;
