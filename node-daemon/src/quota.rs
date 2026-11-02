//! Provider allowances per IP, key or account (PHASE1_5 §3b).
//!
//! A bucket is (provider, scope id, kind). Keyproxy records every request
//! (and tokens, once OpenCode reports them) and feeds every response back.
//! What a bucket knows, best source first:
//! 1. provider signals: rate-limit headers, `retry-after`, 429s;
//! 2. operator overrides (`overrides.yaml`, set from the CLI/UI);
//! 3. configured limits (`providers.yaml` `quota.limits`), counted here.
//!
//! Buckets are persisted in node.db so a restart does not forget that an
//! allowance is used up, and reported to the hub as a full snapshot.

use crate::store::Store;
use moocher_protocol::config::*;
use moocher_protocol::model::{now_ms, QuotaBucket, Ts};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Without a reset time, a 429 parks a bucket this long.
const DEFAULT_BACKOFF_MS: i64 = 60_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Bucket {
    scope: String,
    // From the provider.
    learned_limit: Option<u64>,
    learned_remaining: Option<u64>,
    learned_reset: Option<Ts>,
    learned_at: Ts,
    learned_source: String,
    exhausted_until: Option<Ts>,
    /// Running total (shown when no window is configured).
    used: u64,
    /// One counter per configured limit (see `limit_key`).
    counters: BTreeMap<String, Counter>,
    routes: BTreeSet<String>,
    sessions: BTreeMap<String, Ts>,
    updated_at: Ts,
}

type Key = (String, String, QuotaKind); // provider, scope id, kind

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Counter {
    /// Fixed windows: start of the current window and usage in it.
    start: Ts,
    used: u64,
    /// Rolling windows: (time, amount) still inside the window.
    events: VecDeque<(Ts, u64)>,
}

fn limit_key(l: &QuotaLimit) -> String {
    format!("{}|{}|{}|{}", l.window, l.at.as_deref().unwrap_or(""), l.tz.as_deref().unwrap_or(""), if l.rolling { "r" } else { "f" })
}

pub struct Quotas {
    node: String,
    store: Arc<Store>,
    buckets: Mutex<HashMap<Key, Bucket>>,
    dirty: AtomicBool,
}

/// What keyproxy needs to know to account a request.
#[derive(Debug, Clone)]
pub struct Target {
    pub provider: String,
    pub scope: QuotaScope,
    pub scope_id: String,
}

impl Target {
    /// The same allowance as seen by one model (`quota.models`): its own buckets.
    pub fn for_model(&self, model: &str) -> Target {
        Target { scope_id: format!("{}|{model}", self.scope_id), ..self.clone() }
    }
}

/// Configured limits by provider, and by `provider|model` for model buckets.
pub fn limits_by_bucket(providers: &BTreeMap<String, ProviderDef>) -> BTreeMap<String, Vec<QuotaLimit>> {
    let mut out = BTreeMap::new();
    for (name, p) in providers {
        let spec = spec_for(p);
        for (m, q) in &spec.models {
            out.insert(format!("{name}|{m}"), q.limits.clone());
        }
        out.insert(name.clone(), spec.limits);
    }
    out
}

/// Effective quota settings for a provider.
pub fn spec_for(p: &ProviderDef) -> QuotaSpec {
    p.quota.clone().unwrap_or(QuotaSpec { scope: None, limits: vec![], learn: vec![QuotaLearn::Headers, QuotaLearn::Http429], units: Default::default(), rate: None, models: Default::default() })
}

pub fn default_scope(p: &ProviderDef, has_key: bool) -> QuotaScope {
    match spec_for(p).scope {
        Some(QuotaScope::Key | QuotaScope::Account) if !has_key => QuotaScope::Ip,
        Some(s) => s,
        None if p.auth == AuthStyle::None || !has_key => QuotaScope::Ip,
        None => QuotaScope::Key,
    }
}

/// Scope id: the egress IP (or a stand-in until it is known), or a key/account id.
pub fn scope_id(scope: QuotaScope, node: &str, route: &str, ip: Option<&str>, secret: Option<&str>, account: Option<&str>) -> String {
    let key_id = || secret.map(|s| format!("key:{}", &hex::encode(Sha256::digest(s.as_bytes()))[..12]));
    match scope {
        QuotaScope::Ip => ip.map(String::from).unwrap_or_else(|| format!("route:{node}/{route}")),
        QuotaScope::Key => key_id().unwrap_or_else(|| format!("route:{node}/{route}")),
        QuotaScope::Account => account.map(|a| format!("account:{a}")).or_else(key_id).unwrap_or_else(|| format!("route:{node}/{route}")),
    }
}

/// Start of the fixed window containing `now`, and its length.
pub(crate) fn fixed_window(l: &QuotaLimit, now: Ts) -> Option<(Ts, Ts)> {
    let len = parse_duration_ms(&l.window)?.max(1000);
    let anchor = match &l.at {
        Some(at) => {
            let (h, m) = at.split_once(':')?;
            let (h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
            (h * 3600 + m * 60) * 1000 - tz_offset_ms(l.tz.as_deref())?
        }
        None => 0,
    };
    let k = (now - anchor).div_euclid(len);
    Some((anchor + k * len, len))
}

/// `UTC`, `+02:00`, `-0700` -> offset east of UTC in ms.
fn tz_offset_ms(tz: Option<&str>) -> Option<i64> {
    let tz = tz.unwrap_or("UTC").trim();
    if tz.eq_ignore_ascii_case("utc") || tz.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1, &tz[1..]),
        b'-' => (-1, &tz[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i64>().ok()?, 0),
        4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
        _ => return None,
    };
    Some(sign * (h * 3600 + m * 60) * 1000)
}

/// One rate-limit observation from response headers.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub kind: QuotaKind,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_at: Option<Ts>,
}

/// A reset value: a duration (`6m0s`, `1.5s`, `17`), epoch seconds/millis, or RFC 3339.
fn parse_reset(v: &str, now: Ts) -> Option<Ts> {
    let v = v.trim();
    if let Ok(n) = v.parse::<f64>() {
        let n = n as i64;
        return Some(if n >= 1_000_000_000_000 {
            n
        } else if n >= 1_000_000_000 {
            n * 1000
        } else {
            now + n * 1000
        });
    }
    if let Some(d) = parse_duration_ms(v) {
        return Some(now + d);
    }
    let t = time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339)
        .or_else(|_| time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc2822))
        .ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as Ts)
}

pub fn parse_headers(h: &reqwest::header::HeaderMap, now: Ts) -> Vec<Observed> {
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let num = |n: &str| get(n).and_then(|v| v.trim().parse::<f64>().ok()).map(|f| f.max(0.0) as u64);
    let mut out = vec![];
    for (kind, k) in [(QuotaKind::Requests, "requests"), (QuotaKind::Tokens, "tokens")] {
        // OpenAI / Groq / most OpenAI-compatible gateways.
        let o = Observed {
            kind,
            limit: num(&format!("x-ratelimit-limit-{k}")),
            remaining: num(&format!("x-ratelimit-remaining-{k}")),
            reset_at: get(&format!("x-ratelimit-reset-{k}")).and_then(|v| parse_reset(&v, now)),
        };
        if o.limit.is_some() || o.remaining.is_some() {
            out.push(o);
            continue;
        }
        // Anthropic.
        let o = Observed {
            kind,
            limit: num(&format!("anthropic-ratelimit-{k}-limit")),
            remaining: num(&format!("anthropic-ratelimit-{k}-remaining")),
            reset_at: get(&format!("anthropic-ratelimit-{k}-reset")).and_then(|v| parse_reset(&v, now)),
        };
        if o.limit.is_some() || o.remaining.is_some() {
            out.push(o);
        }
    }
    // OpenRouter and others: unsuffixed headers count requests.
    if !out.iter().any(|o| o.kind == QuotaKind::Requests) {
        let o = Observed {
            kind: QuotaKind::Requests,
            limit: num("x-ratelimit-limit"),
            remaining: num("x-ratelimit-remaining"),
            reset_at: get("x-ratelimit-reset").and_then(|v| parse_reset(&v, now)),
        };
        if o.limit.is_some() || o.remaining.is_some() {
            out.push(o);
        }
    }
    out
}

/// When a 429 says to come back: `retry-after`, else a `retryDelay` in the body (Gemini).
pub fn retry_after(h: &reqwest::header::HeaderMap, body: &[u8], now: Ts) -> Option<Ts> {
    if let Some(v) = h.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Some(t) = parse_reset(v, now) {
            return Some(t);
        }
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let details = v["error"]["details"].as_array()?;
    details.iter().filter_map(|d| d["retryDelay"].as_str()).find_map(|d| parse_duration_ms(d).map(|ms| now + ms))
}

impl Quotas {
    pub fn new(node: &str, store: Arc<Store>) -> Quotas {
        let mut buckets = HashMap::new();
        for (provider, scope_id, kind, json) in store.quota_rows().unwrap_or_default() {
            let kind = if kind == "tokens" { QuotaKind::Tokens } else { QuotaKind::Requests };
            if let Ok(b) = serde_json::from_str::<Bucket>(&json) {
                buckets.insert((provider, scope_id, kind), b);
            }
        }
        Quotas { node: node.into(), store, buckets: Mutex::new(buckets), dirty: AtomicBool::new(true) }
    }

    /// Something outside the buckets changed (config, overrides): re-report.
    pub fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Make sure configured buckets exist so the UI shows them before first use.
    pub fn ensure(&self, t: &Target, route: &str, limits: &[QuotaLimit]) {
        let mut m = self.buckets.lock().unwrap();
        for kind in [QuotaKind::Requests, QuotaKind::Tokens] {
            if !limits.iter().any(|l| l.kind == kind) {
                continue;
            }
            let b = m.entry((t.provider.clone(), t.scope_id.clone(), kind)).or_insert_with(|| Bucket { scope: t.scope.as_str().into(), ..Default::default() });
            if b.routes.insert(route.to_string()) {
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    /// If the bucket is used up now, until when.
    pub fn exhausted(&self, t: &Target, limits: &[QuotaLimit], overrides: &[QuotaOverride], now: Ts) -> Option<Ts> {
        let mut until: Option<Ts> = None;
        let mut later = |t: Ts| until = Some(until.map_or(t, |u: Ts| u.max(t)));
        for o in overrides.iter().filter(|o| o.provider == t.provider && o.scope_id.as_deref().is_none_or(|s| s == t.scope_id)) {
            if let (Some(0), Some(r)) = (o.remaining, o.resets_at) {
                if r > now {
                    later(r);
                }
            }
        }
        let m = self.buckets.lock().unwrap();
        for kind in [QuotaKind::Requests, QuotaKind::Tokens] {
            let Some(b) = m.get(&(t.provider.clone(), t.scope_id.clone(), kind)) else { continue };
            if let Some(u) = b.exhausted_until.filter(|u| *u > now) {
                later(u);
            }
            if b.learned_remaining == Some(0) {
                if let Some(r) = b.learned_reset.filter(|r| *r > now) {
                    later(r);
                }
            }
            for l in limits.iter().filter(|l| l.kind == kind) {
                let (used, reset) = counted(b, l, now);
                if used >= l.max {
                    if let Some(r) = reset {
                        later(r);
                    }
                }
            }
        }
        until
    }

    /// The provider said the account is out of credits: treat the allowance
    /// as used up until `until`.
    pub fn exhaust_until(&self, t: &Target, until: Ts) {
        let now = now_ms();
        let mut m = self.buckets.lock().unwrap();
        let b = m.entry((t.provider.clone(), t.scope_id.clone(), QuotaKind::Requests)).or_insert_with(|| Bucket { scope: t.scope.as_str().into(), ..Default::default() });
        b.exhausted_until = Some(until);
        b.learned_remaining = Some(0);
        b.learned_reset = Some(until);
        b.learned_at = now;
        b.learned_source = "credits".into();
        b.updated_at = now;
        drop(m);
        self.touch();
    }

    pub fn record_request(&self, t: &Target, route: &str, session: &str, limits: &[QuotaLimit]) {
        self.add(t, route, session, limits, QuotaKind::Requests, 1);
    }

    /// Tokens are known only after a request finished (OpenCode's counters),
    /// so keyproxy attributes them to the bucket that served that request.
    pub fn record_tokens(&self, t: &Target, route: &str, session: &str, limits: &[QuotaLimit], tokens: u64) {
        if tokens > 0 {
            self.add(t, route, session, limits, QuotaKind::Tokens, tokens);
        }
    }

    fn add(&self, t: &Target, route: &str, session: &str, limits: &[QuotaLimit], kind: QuotaKind, amount: u64) {
        let now = now_ms();
        let mut m = self.buckets.lock().unwrap();
        {
            let b = m.entry((t.provider.clone(), t.scope_id.clone(), kind)).or_insert_with(|| Bucket { scope: t.scope.as_str().into(), ..Default::default() });
            b.routes.insert(route.to_string());
            b.sessions.insert(session.to_string(), now);
            b.sessions.retain(|_, ts| now - *ts < 600_000);
            for l in limits.iter().filter(|l| l.kind == kind) {
                let c = b.counters.entry(limit_key(l)).or_default();
                if l.rolling {
                    c.events.push_back((now, amount));
                    let len = parse_duration_ms(&l.window).unwrap_or(0);
                    while c.events.front().is_some_and(|(ts, _)| now - ts >= len) {
                        c.events.pop_front();
                    }
                } else if let Some((start, _)) = fixed_window(l, now) {
                    if c.start != start {
                        c.start = start;
                        c.used = 0;
                    }
                    c.used += amount;
                }
            }
            b.used += amount;
            // A learned "remaining" goes down as we spend it, until the provider tells us again.
            if let Some(r) = b.learned_remaining.as_mut() {
                *r = r.saturating_sub(amount);
            }
            b.updated_at = now;
        }
        drop(m);
        self.touch();
    }

    /// Learn from a provider response. Returns the time the bucket is exhausted until, if it is.
    pub fn observe(&self, t: &Target, status: u16, headers: &reqwest::header::HeaderMap, body: &[u8], learn: &[QuotaLearn]) -> Option<Ts> {
        let now = now_ms();
        let mut m = self.buckets.lock().unwrap();
        let mut exhausted = None;
        if learn.contains(&QuotaLearn::Headers) {
            for o in parse_headers(headers, now) {
                let b = m.entry((t.provider.clone(), t.scope_id.clone(), o.kind)).or_insert_with(|| Bucket { scope: t.scope.as_str().into(), ..Default::default() });
                b.learned_limit = o.limit.or(b.learned_limit);
                b.learned_remaining = o.remaining;
                b.learned_reset = o.reset_at;
                b.learned_at = now;
                b.learned_source = "header".into();
                b.updated_at = now;
                if o.remaining == Some(0) {
                    if let Some(r) = o.reset_at {
                        b.exhausted_until = Some(r);
                        exhausted = Some(r);
                    }
                }
            }
        }
        if status == 429 && learn.contains(&QuotaLearn::Http429) {
            let until = retry_after(headers, body, now)
                .or_else(|| m.get(&(t.provider.clone(), t.scope_id.clone(), QuotaKind::Requests)).and_then(|b| b.learned_reset).filter(|r| *r > now))
                .unwrap_or(now + DEFAULT_BACKOFF_MS);
            let b = m.entry((t.provider.clone(), t.scope_id.clone(), QuotaKind::Requests)).or_insert_with(|| Bucket { scope: t.scope.as_str().into(), ..Default::default() });
            b.exhausted_until = Some(until);
            b.learned_remaining = Some(0);
            b.learned_reset = Some(until);
            b.learned_at = now;
            b.learned_source = "429".into();
            b.updated_at = now;
            exhausted = Some(until);
        }
        drop(m);
        self.touch();
        exhausted
    }

    /// Full view for the hub. `limits`/`overrides` come from the current config.
    pub fn snapshot(&self, provider_limits: &BTreeMap<String, Vec<QuotaLimit>>, overrides: &[QuotaOverride]) -> Vec<QuotaBucket> {
        let now = now_ms();
        let m = self.buckets.lock().unwrap();
        let mut out = vec![];
        for ((provider, scope_id, kind), b) in m.iter() {
            let limits_of = match scope_id.split_once('|') {
                Some((_, model)) => provider_limits.get(&format!("{provider}|{model}")),
                None => provider_limits.get(provider),
            };
            let limits: Vec<&QuotaLimit> = limits_of.map(|v| v.iter().filter(|l| l.kind == *kind).collect()).unwrap_or_default();
            let mut q = QuotaBucket {
                provider: provider.clone(),
                scope: b.scope.clone(),
                scope_id: scope_id.clone(),
                kind: kind.as_str().into(),
                limit: None,
                used: b.used, // replaced by the window's usage when a limit is configured
                remaining: None,
                resets_at: None,
                exhausted: false,
                source: "counted".into(),
                window: None,
                node: self.node.clone(),
                routes: b.routes.iter().cloned().collect(),
                sessions: b.sessions.iter().filter(|(_, ts)| now - **ts < 600_000).map(|(s, _)| s.clone()).collect(),
                updated_at: b.updated_at,
                unit: None,
            };
            // Configured limit (the tightest one).
            for l in &limits {
                let (used, reset) = counted(b, l, now);
                let rem = l.max.saturating_sub(used);
                if q.remaining.is_none_or(|r| rem < r) {
                    q.used = used;
                    q.limit = Some(l.max);
                    q.remaining = Some(rem);
                    q.resets_at = reset;
                    q.window = Some(format!("{}{}", l.window, if l.rolling { " rolling" } else { "" }));
                    q.source = "config".into();
                }
            }
            // Provider signals win while they are current.
            let learned_current = b.learned_reset.is_none_or(|r| r > now) && b.learned_at > 0;
            if learned_current && (b.learned_remaining.is_some() || b.learned_limit.is_some()) {
                q.limit = b.learned_limit.or(q.limit);
                q.remaining = b.learned_remaining.or(q.remaining);
                q.resets_at = b.learned_reset.or(q.resets_at);
                q.source = b.learned_source.clone();
            }
            if let Some(u) = b.exhausted_until.filter(|u| *u > now) {
                q.exhausted = true;
                q.remaining = Some(0);
                q.resets_at = Some(q.resets_at.map_or(u, |r| r.max(u)));
            }
            if *kind == QuotaKind::Requests {
                for o in overrides.iter().filter(|o| &o.provider == provider && o.scope_id.as_deref().is_none_or(|s| s == scope_id)) {
                    if o.resets_at.is_none_or(|r| r > now) {
                        if o.remaining.is_some() {
                            q.remaining = o.remaining;
                        }
                        if o.resets_at.is_some() {
                            q.resets_at = o.resets_at;
                        }
                        q.source = "override".into();
                        if o.remaining == Some(0) {
                            q.exhausted = true;
                        }
                    }
                }
            }
            if q.remaining == Some(0) && q.resets_at.is_some_and(|r| r > now) {
                q.exhausted = true;
            }
            out.push(q);
        }
        out.sort_by(|a, b| (&a.provider, &a.scope_id, &a.kind).cmp(&(&b.provider, &b.scope_id, &b.kind)));
        out
    }

    /// Persist and say whether anything changed since the last call.
    pub fn flush(&self) -> bool {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return false;
        }
        let rows: Vec<(String, String, String, String)> = self
            .buckets
            .lock()
            .unwrap()
            .iter()
            .filter_map(|((p, s, k), b)| Some((p.clone(), s.clone(), k.as_str().to_string(), serde_json::to_string(b).ok()?)))
            .collect();
        if let Err(e) = self.store.put_quota_rows(&rows) {
            tracing::warn!(error = %e, "persisting quota buckets failed");
        }
        true
    }
}

/// (used in the current window, when it frees up) for a configured limit.
fn counted(b: &Bucket, l: &QuotaLimit, now: Ts) -> (u64, Option<Ts>) {
    let c = b.counters.get(&limit_key(l)).cloned().unwrap_or_default();
    if l.rolling {
        let len = parse_duration_ms(&l.window).unwrap_or(0);
        let live: Vec<&(Ts, u64)> = c.events.iter().filter(|(ts, _)| now - ts < len).collect();
        (live.iter().map(|(_, n)| n).sum(), live.first().map(|(ts, _)| ts + len))
    } else {
        match fixed_window(l, now) {
            Some((start, len)) => (if c.start == start { c.used } else { 0 }, Some(start + len)),
            None => (0, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn header_formats() {
        let now = 1_000_000_000_000;
        let o = parse_headers(&hm(&[("x-ratelimit-limit-requests", "14400"), ("x-ratelimit-remaining-requests", "0"), ("x-ratelimit-reset-requests", "2m59.56s")]), now);
        assert_eq!(o[0], Observed { kind: QuotaKind::Requests, limit: Some(14400), remaining: Some(0), reset_at: Some(now + 179_560) });
        let o = parse_headers(&hm(&[("anthropic-ratelimit-tokens-remaining", "500"), ("anthropic-ratelimit-tokens-reset", "2001-09-09T01:46:50Z")]), now);
        assert_eq!(o[0].kind, QuotaKind::Tokens);
        assert_eq!(o[0].reset_at, Some(1_000_000_010_000));
        let o = parse_headers(&hm(&[("x-ratelimit-limit", "20"), ("x-ratelimit-remaining", "3"), ("x-ratelimit-reset", "1000000060000")]), now);
        assert_eq!(o[0].reset_at, Some(1_000_000_060_000));
        assert_eq!(retry_after(&hm(&[("retry-after", "30")]), b"", now), Some(now + 30_000));
        let gemini = br#"{"error":{"code":429,"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"38s"}]}}"#;
        assert_eq!(retry_after(&HeaderMap::new(), gemini, now), Some(now + 38_000));
    }

    #[test]
    fn fixed_daily_window_at_wall_clock() {
        let l = QuotaLimit { kind: QuotaKind::Requests, max: 3, window: "1d".into(), at: Some("03:15".into()), tz: Some("+02:00".into()), rolling: false };
        // 2026-10-23 12:00 UTC; resets at 01:15 UTC (03:15 +02:00) the next day.
        let now = 1_792_756_800_000;
        let (start, len) = fixed_window(&l, now).unwrap();
        assert_eq!(start + len, 1_792_804_500_000);
        assert!(start <= now && now < start + len);
    }

    #[test]
    fn buckets_exhaust_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("n.db")).unwrap());
        let q = Quotas::new("n1", store.clone());
        let t = Target { provider: "kilo-free".into(), scope: QuotaScope::Ip, scope_id: "203.0.113.7".into() };
        let limits = vec![QuotaLimit { kind: QuotaKind::Requests, max: 2, window: "1h".into(), at: None, tz: None, rolling: false }];
        assert_eq!(q.exhausted(&t, &limits, &[], now_ms()), None);
        q.record_request(&t, "direct", "s-1", &limits);
        q.record_request(&t, "direct", "s-1", &limits);
        let until = q.exhausted(&t, &limits, &[], now_ms()).expect("2/2 used");
        assert!(until > now_ms());
        // Another IP is a separate bucket.
        let t2 = Target { scope_id: "198.51.100.9".into(), ..t.clone() };
        assert_eq!(q.exhausted(&t2, &limits, &[], now_ms()), None);
        // A 429 with retry-after exhausts a bucket without configured limits.
        let t3 = Target { provider: "groq".into(), scope: QuotaScope::Key, scope_id: "key:abc".into() };
        let u = q.observe(&t3, 429, &hm(&[("retry-after", "120")]), b"", &[QuotaLearn::Headers, QuotaLearn::Http429]).unwrap();
        assert!(q.exhausted(&t3, &[], &[], now_ms()).is_some_and(|x| x == u));
        // Overrides: "0 left until T" exhausts; snapshot shows the override.
        let ov = QuotaOverride { provider: "kilo-free".into(), scope_id: Some("198.51.100.9".into()), resets_at: Some(now_ms() + 3_600_000), remaining: Some(0), unit: None, used: None, model: None, note: None, set_by: None };
        assert!(q.exhausted(&t2, &limits, std::slice::from_ref(&ov), now_ms()).is_some());
        q.record_request(&t2, "via-vps", "s-2", &limits);
        let mut pl = BTreeMap::new();
        pl.insert("kilo-free".to_string(), limits.clone());
        let snap = q.snapshot(&pl, &[ov]);
        let b = snap.iter().find(|b| b.scope_id == "198.51.100.9").unwrap();
        assert_eq!((b.source.as_str(), b.exhausted, b.routes.clone()), ("override", true, vec!["via-vps".to_string()]));
        let a = snap.iter().find(|b| b.scope_id == "203.0.113.7").unwrap();
        assert_eq!((a.used, a.limit, a.remaining, a.exhausted), (2, Some(2), Some(0), true));
        // Persisted across restarts.
        assert!(q.flush());
        let q2 = Quotas::new("n1", store);
        assert!(q2.exhausted(&t, &limits, &[], now_ms()).is_some());
    }

    #[test]
    fn separate_counters_per_limit() {
        let dir = tempfile::tempdir().unwrap();
        let q = Quotas::new("n1", Arc::new(Store::open(&dir.path().join("n.db")).unwrap()));
        let t = Target { provider: "p".into(), scope: QuotaScope::Ip, scope_id: "1.2.3.4".into() };
        let limits = vec![
            QuotaLimit { kind: QuotaKind::Requests, max: 2, window: "1m".into(), at: None, tz: None, rolling: true },
            QuotaLimit { kind: QuotaKind::Requests, max: 100, window: "1d".into(), at: Some("00:00".into()), tz: None, rolling: false },
        ];
        q.record_request(&t, "direct", "s", &limits);
        assert_eq!(q.exhausted(&t, &limits, &[], now_ms()), None);
        q.record_request(&t, "direct", "s", &limits);
        // The per-minute limit is hit; it frees up within a minute, not at midnight.
        let until = q.exhausted(&t, &limits, &[], now_ms()).unwrap();
        assert!(until - now_ms() <= 60_000);
        let snap = q.snapshot(&BTreeMap::from([("p".to_string(), limits)]), &[]);
        assert_eq!((snap[0].limit, snap[0].remaining, snap[0].used), (Some(2), Some(0), 2));
    }

    #[test]
    fn scope_ids() {
        assert_eq!(scope_id(QuotaScope::Ip, "n", "direct", Some("1.2.3.4"), None, None), "1.2.3.4");
        assert_eq!(scope_id(QuotaScope::Ip, "n", "direct", None, None, None), "route:n/direct");
        assert!(scope_id(QuotaScope::Key, "n", "direct", None, Some("sk-1"), None).starts_with("key:"));
        assert_eq!(scope_id(QuotaScope::Account, "n", "d", None, Some("sk-1"), Some("personal")), "account:personal");
    }
}
