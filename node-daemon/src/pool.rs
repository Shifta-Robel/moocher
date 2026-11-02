//! Key pools and what the node knows of the cluster's allowances.
//!
//! A provider's `keys:` give keyproxy several credentials to choose from. The
//! hub sends the cluster's view (`NodeCommand::Registry`): allowances others
//! used up, how busy each is, and which ones lower-urgency sessions must keep
//! off for `right_now` work. Choosing stays here, so calls keep working while
//! the hub is unreachable; the view can only lag.
//!
//! Borrowing: a lendable key goes to another node encrypted for a one-time
//! X25519 key of the borrower, so the hub passes on only what it cannot read.
//! A borrowed key stays in memory and expires.

use crate::cfgstore::Snapshot;
use crate::link::LinkHandle;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use moocher_protocol::config::{PoolKey, ProviderDef};
use moocher_protocol::link::{AgentCall, NodeEvent};
use moocher_protocol::model::*;
use ring::{aead, agreement, hkdf, rand as ring_rand};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// A refused key is tried again after this.
const BAD_KEY_MS: i64 = 600_000;
const BORROW_TTL_MS: i64 = 3_600_000;
/// After a borrow found nothing.
const BORROW_BACKOFF_MS: i64 = 60_000;
/// A session that made no call for this long no longer counts as using its key.
const USE_TTL_MS: i64 = 600_000;
const REPORT_EVERY_MS: i64 = 60_000;
/// Keys are looked up this often for reports (`pass:` can be slow, or ask for a PIN).
const LOOKUP_EVERY_MS: i64 = 60_000;

/// One credential a call can go out with.
#[derive(Clone, Default)]
pub struct KeyChoice {
    pub secret: Option<String>,
    pub account: Option<String>,
    pub key_id: String,
    pub max_concurrent: Option<u32>,
    pub lendable: bool,
    pub borrowed: bool,
}

pub fn key_id(secret: &str) -> String {
    format!("key:{}", &hex::encode(Sha256::digest(secret.as_bytes()))[..12])
}

/// The pool of `p` as far as it resolves on this node.
pub fn own_keys(snap: &Snapshot, p: &ProviderDef) -> Vec<KeyChoice> {
    p.keys
        .iter()
        .filter_map(|k| {
            let (secret, account) = snap.secret(&k.secret_ref()).map_err(|e| tracing::warn!(error = %e, "a pool key does not resolve")).ok()?;
            Some(KeyChoice { key_id: key_id(&secret), secret: Some(secret), account, max_concurrent: k.max_concurrent, lendable: k.lend, borrowed: false })
        })
        .collect()
}

#[derive(Clone)]
struct Borrowed {
    key: KeyChoice,
    expires_at: Ts,
}

#[derive(Default)]
struct Inner {
    buckets: HashMap<(String, String), RegistryBucket>,
    yields: Vec<KeyYield>,
    uses: HashMap<String, (KeyUse, Ts)>,
    /// Calls under way, by (provider, scope id) and by key.
    flights: HashMap<(String, String), u32>,
    key_flights: HashMap<String, u32>,
    bad: HashMap<String, Ts>,
    levels: BTreeMap<String, u8>,
    borrowed: HashMap<String, Vec<Borrowed>>,
    borrow_after: HashMap<String, Ts>,
    last_report: Option<(Vec<PoolKeyInfo>, Vec<KeyUse>, Ts)>,
    own: Option<OwnKeys>,
}

/// This node's keys as last looked up, and the config they came from.
struct OwnKeys {
    pools: BTreeMap<String, Vec<PoolKey>>,
    keys: Vec<(String, KeyChoice)>,
    at: Ts,
}

pub struct Pool {
    inner: Arc<Mutex<Inner>>,
    /// One borrow per provider at a time.
    borrowing: tokio::sync::Mutex<()>,
}

/// Counts a call as under way until dropped.
pub struct Flight {
    inner: Arc<Mutex<Inner>>,
    scope: (String, String),
    key: String,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut m = self.inner.lock().unwrap();
        if let Some(n) = m.flights.get_mut(&self.scope) {
            *n = n.saturating_sub(1);
        }
        if let Some(n) = m.key_flights.get_mut(&self.key) {
            *n = n.saturating_sub(1);
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Pool { inner: Default::default(), borrowing: Default::default() }
    }
}

impl Pool {
    /// A session's urgency as the hub last said (`level()`); normal by default.
    pub fn level(&self, session: &str) -> u8 {
        self.inner.lock().unwrap().levels.get(session).copied().unwrap_or(TaskLabel::Normal.level())
    }

    pub fn apply(&self, buckets: Vec<RegistryBucket>, yields: Vec<KeyYield>, levels: BTreeMap<String, u8>) {
        self.inner.lock().unwrap().levels = levels;
        let mut m = self.inner.lock().unwrap();
        m.buckets = buckets.into_iter().map(|b| ((b.provider.clone(), b.scope_id.clone()), b)).collect();
        m.yields = yields;
    }

    /// Used up by the cluster's count, which this node's own may not show yet.
    pub fn exhausted(&self, provider: &str, scope_id: &str, now: Ts) -> Option<Ts> {
        self.inner.lock().unwrap().buckets.get(&(provider.to_string(), scope_id.to_string())).and_then(|b| b.exhausted_until).filter(|u| *u > now)
    }

    /// Must this session keep off this allowance? Then: who for.
    pub fn yielded(&self, provider: &str, scope_id: &str, session: &str, now: Ts) -> Option<String> {
        self.inner.lock().unwrap().yields.iter().find(|y| y.provider == provider && y.scope_id == scope_id && y.expires_at > now && !y.exempt.iter().any(|e| e == session)).map(|y| y.by.clone())
    }

    /// How busy an allowance is, without `session`: lower is better.
    pub fn load(&self, provider: &str, scope_id: &str, session: &str) -> (u32, u64) {
        let m = self.inner.lock().unwrap();
        let key = (provider.to_string(), scope_id.to_string());
        let (remote, used) = m.buckets.get(&key).map_or((0, 0), |b| (b.in_flight + b.sessions, b.used));
        let local = m.uses.values().filter(|(u, _)| u.provider == provider && u.scope_id == scope_id && u.session != session).count() as u32;
        (remote + local + m.flights.get(&key).copied().unwrap_or(0), used)
    }

    /// Sessions on a route, without `session`.
    pub fn route_load(&self, provider: &str, route: &str, session: &str) -> usize {
        self.inner.lock().unwrap().uses.values().filter(|(u, _)| u.provider == provider && u.route == route && u.session != session).count()
    }

    /// Is the key at its `max_concurrent`, counting other nodes?
    pub fn at_capacity(&self, provider: &str, scope_id: &str, k: &KeyChoice) -> bool {
        let Some(max) = k.max_concurrent else { return false };
        let m = self.inner.lock().unwrap();
        let remote = m.buckets.get(&(provider.to_string(), scope_id.to_string())).map_or(0, |b| b.in_flight);
        m.key_flights.get(&k.key_id).copied().unwrap_or(0) + remote >= max
    }

    pub fn begin(&self, provider: &str, scope_id: &str, key_id: &str) -> Flight {
        let scope = (provider.to_string(), scope_id.to_string());
        let mut m = self.inner.lock().unwrap();
        *m.flights.entry(scope.clone()).or_default() += 1;
        *m.key_flights.entry(key_id.to_string()).or_default() += 1;
        Flight { inner: self.inner.clone(), scope, key: key_id.to_string() }
    }

    pub fn note_use(&self, u: KeyUse) {
        self.inner.lock().unwrap().uses.insert(u.session.clone(), (u, now_ms()));
    }

    pub fn is_bad(&self, key_id: &str, now: Ts) -> bool {
        self.inner.lock().unwrap().bad.get(key_id).is_some_and(|t| *t > now)
    }

    /// The provider refused this key.
    pub fn mark_bad(&self, provider: &str, key_id: &str) {
        let mut m = self.inner.lock().unwrap();
        m.bad.insert(key_id.to_string(), now_ms() + BAD_KEY_MS);
        if let Some(v) = m.borrowed.get_mut(provider) {
            v.retain(|b| b.key.key_id != key_id);
        }
    }

    pub fn borrowed(&self, provider: &str, now: Ts) -> Vec<KeyChoice> {
        let mut m = self.inner.lock().unwrap();
        let Some(v) = m.borrowed.get_mut(provider) else { return vec![] };
        v.retain(|b| b.expires_at > now);
        v.iter().map(|b| b.key.clone()).collect()
    }

    /// Ask the hub for a key from another node. Returns whether one came.
    pub async fn borrow(&self, link: &LinkHandle, session: &str, provider: &str, have: Vec<String>) -> bool {
        let _one = self.borrowing.lock().await;
        let now = now_ms();
        {
            let m = self.inner.lock().unwrap();
            if m.borrow_after.get(provider).is_some_and(|t| *t > now) {
                return false;
            }
            // Someone waiting on the lock may have brought one already.
            if m.borrowed.get(provider).is_some_and(|v| v.iter().any(|b| !have.contains(&b.key.key_id) && b.expires_at > now)) {
                return true;
            }
        }
        match self.try_borrow(link, session, provider, have).await {
            Ok(b) => {
                tracing::info!(provider, key = %b.key.key_id, "borrowed a key");
                self.inner.lock().unwrap().borrowed.entry(provider.to_string()).or_default().push(b);
                true
            }
            Err(e) => {
                tracing::info!(provider, error = %format!("{e:#}"), "no key to borrow");
                self.inner.lock().unwrap().borrow_after.insert(provider.to_string(), now + BORROW_BACKOFF_MS);
                false
            }
        }
    }

    async fn try_borrow(&self, link: &LinkHandle, session: &str, provider: &str, have: Vec<String>) -> Result<Borrowed> {
        let rng = ring_rand::SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).map_err(|_| anyhow!("no randomness"))?;
        let public = private.compute_public_key().map_err(|_| anyhow!("no public key"))?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let v = link.request(session, AgentCall::BorrowKey { provider: provider.to_string(), public_key: b64.encode(public.as_ref()), have }).await?;
        let secret = open(private, &v, provider)?;
        let key = KeyChoice {
            key_id: key_id(&secret),
            secret: Some(secret),
            account: v["account"].as_str().map(String::from),
            max_concurrent: v["max_concurrent"].as_u64().map(|n| n as u32),
            lendable: false,
            borrowed: true,
        };
        Ok(Borrowed { key, expires_at: now_ms() + BORROW_TTL_MS })
    }

    /// Tell the hub about this node's pools, on change.
    pub fn report(&self, link: &LinkHandle, snap: &Snapshot) {
        let now = now_ms();
        let pools: BTreeMap<String, Vec<PoolKey>> = snap.providers.iter().filter(|(_, p)| !p.keys.is_empty()).map(|(n, p)| (n.clone(), p.keys.clone())).collect();
        // A config change shows at once; only the lookups wait.
        let stale = self.inner.lock().unwrap().own.as_ref().is_none_or(|o| o.pools != pools || now - o.at >= LOOKUP_EVERY_MS);
        if stale {
            let keys = pools.keys().filter_map(|n| snap.providers.get_key_value(n)).flat_map(|(name, p)| own_keys(snap, p).into_iter().map(move |k| (name.clone(), k))).collect();
            self.inner.lock().unwrap().own = Some(OwnKeys { pools, keys, at: now });
        }
        let mut m = self.inner.lock().unwrap();
        m.uses.retain(|_, (_, at)| now - *at < USE_TTL_MS);
        let mut keys: Vec<PoolKeyInfo> = vec![];
        for (name, k) in m.own.as_ref().map(|o| o.keys.as_slice()).unwrap_or_default() {
            let Some(p) = snap.providers.get(name) else { continue };
            keys.push(info(name, p, snap, k, &m, now));
        }
        for (name, v) in &m.borrowed {
            let Some(p) = snap.providers.get(name) else { continue };
            for b in v.iter().filter(|b| b.expires_at > now) {
                keys.push(info(name, p, snap, &b.key, &m, now));
            }
        }
        let mut in_use: Vec<KeyUse> = m.uses.values().map(|(u, _)| u.clone()).collect();
        in_use.sort_by(|a, b| a.session.cmp(&b.session));
        if let Some((k, u, at)) = &m.last_report {
            if *k == keys && *u == in_use && now - at < REPORT_EVERY_MS {
                return;
            }
        }
        m.last_report = Some((keys.clone(), in_use.clone(), now));
        drop(m);
        link.emit(NodeEvent::Pool { keys, in_use });
    }
}

fn info(name: &str, p: &ProviderDef, snap: &Snapshot, k: &KeyChoice, m: &Inner, now: Ts) -> PoolKeyInfo {
    let scope = crate::quota::default_scope(p, true);
    let scope_id = crate::quota::scope_id(scope, &snap.node.name, "direct", None, k.secret.as_deref(), k.account.as_deref());
    PoolKeyInfo {
        provider: name.to_string(),
        key_id: k.key_id.clone(),
        scope_id,
        account: k.account.clone(),
        lendable: k.lendable,
        borrowed: k.borrowed,
        bad_until: m.bad.get(&k.key_id).copied().filter(|t| *t > now),
        max_concurrent: k.max_concurrent,
        in_flight: m.key_flights.get(&k.key_id).copied().unwrap_or(0),
    }
}

/// `NodeCommand::LendKey`: encrypt a key this node agreed to lend.
pub fn lend(snap: &Snapshot, provider: &str, wanted: &str, to: &str, public_key: &str) -> Result<Value> {
    let p = snap.providers.get(provider).ok_or_else(|| anyhow!("provider {provider} is not configured here"))?;
    let k = own_keys(snap, p).into_iter().find(|k| k.key_id == wanted).ok_or_else(|| anyhow!("no such key here"))?;
    if !k.lendable {
        bail!("this key is not marked lend: true");
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let peer = b64.decode(public_key).context("bad public key")?;
    let (public, sealed) = seal(&peer, k.secret.as_deref().unwrap_or_default(), provider)?;
    tracing::info!(provider, key = wanted, to, "lending a key");
    Ok(json!({"public_key": b64.encode(public), "sealed": b64.encode(sealed), "account": k.account, "max_concurrent": k.max_concurrent}))
}

const INFO: &[u8] = b"moocher key loan v1";

struct KeyLen;
impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        32
    }
}

fn aead_key(shared: &[u8]) -> Result<aead::LessSafeKey> {
    let mut k = [0u8; 32];
    hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(shared).expand(&[INFO], KeyLen).map_err(|_| anyhow!("hkdf"))?.fill(&mut k).map_err(|_| anyhow!("hkdf"))?;
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &k).map_err(|_| anyhow!("aead key"))?;
    Ok(aead::LessSafeKey::new(unbound))
}

/// Encrypt for `peer`. The key is new each time, so a fixed nonce is safe.
fn seal(peer: &[u8], secret: &str, provider: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let rng = ring_rand::SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).map_err(|_| anyhow!("no randomness"))?;
    let public = private.compute_public_key().map_err(|_| anyhow!("no public key"))?.as_ref().to_vec();
    let key = agreement::agree_ephemeral(private, &agreement::UnparsedPublicKey::new(&agreement::X25519, peer), |s| aead_key(s)).map_err(|_| anyhow!("key agreement failed"))??;
    let mut data = secret.as_bytes().to_vec();
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key([0; 12]), aead::Aad::from(provider.as_bytes()), &mut data).map_err(|_| anyhow!("encrypting failed"))?;
    Ok((public, data))
}

fn open(private: agreement::EphemeralPrivateKey, v: &Value, provider: &str) -> Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let peer = b64.decode(v["public_key"].as_str().unwrap_or_default()).context("bad reply")?;
    let mut data = b64.decode(v["sealed"].as_str().unwrap_or_default()).context("bad reply")?;
    let key = agreement::agree_ephemeral(private, &agreement::UnparsedPublicKey::new(&agreement::X25519, &peer), |s| aead_key(s)).map_err(|_| anyhow!("key agreement failed"))??;
    let plain = key.open_in_place(aead::Nonce::assume_unique_for_key([0; 12]), aead::Aad::from(provider.as_bytes()), &mut data).map_err(|_| anyhow!("the key did not decrypt"))?;
    Ok(String::from_utf8(plain.to_vec())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (agreement::EphemeralPrivateKey, Vec<u8>) {
        let p = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &ring_rand::SystemRandom::new()).unwrap();
        let public = p.compute_public_key().unwrap().as_ref().to_vec();
        (p, public)
    }

    fn reply(public: Vec<u8>, sealed: Vec<u8>) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD;
        json!({"public_key": b64.encode(public), "sealed": b64.encode(sealed)})
    }

    #[test]
    fn a_loan_opens_only_for_the_borrower_and_provider() {
        let (private, public) = pair();
        let (p, s) = seal(&public, "sk-secret", "groq").unwrap();
        assert!(!String::from_utf8_lossy(&s).contains("sk-secret"));
        assert_eq!(open(private, &reply(p.clone(), s.clone()), "groq").unwrap(), "sk-secret");
        let (other, _) = pair();
        assert!(open(other, &reply(p.clone(), s.clone()), "groq").is_err());
        let (private, public) = pair();
        let (p, s) = seal(&public, "sk-secret", "groq").unwrap();
        assert!(open(private, &reply(p, s), "openai").is_err());
    }

    #[test]
    fn yields_spare_the_exempt_and_lapse() {
        let pool = Pool::default();
        let y = KeyYield { provider: "p".into(), scope_id: "key:a".into(), exempt: vec!["urgent".into()], expires_at: 100, by: "task #1".into() };
        pool.apply(vec![], vec![y], Default::default());
        assert_eq!(pool.yielded("p", "key:a", "other", 50).as_deref(), Some("task #1"));
        assert_eq!(pool.yielded("p", "key:a", "urgent", 50), None);
        assert_eq!(pool.yielded("p", "key:b", "other", 50), None);
        assert_eq!(pool.yielded("p", "key:a", "other", 150), None);
    }

    #[test]
    fn flights_count_until_dropped() {
        let pool = Pool::default();
        let k = KeyChoice { key_id: "key:a".into(), max_concurrent: Some(1), ..Default::default() };
        assert!(!pool.at_capacity("p", "key:a", &k));
        let f = pool.begin("p", "key:a", "key:a");
        assert!(pool.at_capacity("p", "key:a", &k));
        drop(f);
        assert!(!pool.at_capacity("p", "key:a", &k));
    }
}
