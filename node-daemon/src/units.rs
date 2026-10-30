//! Provider units: neurons, pollen, credits (`quota.units` in providers.yaml).
//!
//! Some providers meter in a unit of their own, and one token costs a
//! different amount on each model. For every call keyproxy works out the cost,
//! best source first:
//! 1. what the provider says: a response header or a number in its usage;
//! 2. what was learned for that model: measured, or from a calibration (the
//!    operator read "4210 used" off the provider's dashboard);
//! 3. the configured `per_mtok` starting estimate.
//!
//! A pool is (provider, scope id, unit, model); the model is empty unless the
//! unit is `per_model`. What one unit buys is learned per model either way.

use crate::quota::{fixed_window, Target};
use crate::store::Store;
use moocher_protocol::config::*;
use moocher_protocol::model::{now_ms, QuotaBucket, Ts, UnitInfo, UnitModelRate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Counter that never resets: `used` of a unit without limits.
const LIFETIME: &str = "";
/// Measurements are halved past this many tokens, so a provider's price change shows.
const RATE_TOKENS: u64 = 10_000_000;

type Key = (String, String, String, String); // provider, scope id, unit, model

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pool {
    scope: String,
    counters: BTreeMap<String, Counter>,
    /// Units per token, per model.
    rates: BTreeMap<String, Rate>,
    /// `estimated`, `measured` or `calibrated`.
    source: String,
    routes: BTreeSet<String>,
    sessions: BTreeMap<String, Ts>,
    updated_at: Ts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Counter {
    start: Ts,
    used: f64,
    tokens: BTreeMap<String, u64>,
    events: VecDeque<Event>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Event {
    ts: Ts,
    units: f64,
    model: String,
    tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Rate {
    units: f64,
    tokens: u64,
    /// `measured` or `calibrated`.
    source: String,
}

impl Rate {
    fn per_token(&self) -> Option<f64> {
        (self.tokens > 0 && self.units > 0.0).then(|| self.units / self.tokens as f64)
    }
}

/// One finished call.
pub struct UnitCall<'a> {
    pub target: &'a Target,
    pub route: &'a str,
    pub session: &'a str,
    pub model: &'a str,
    pub input: u64,
    pub output: u64,
}

fn limit_key(l: &UnitLimit) -> String {
    format!("{}|{}|{}|{}", l.window, l.at.as_deref().unwrap_or(""), l.tz.as_deref().unwrap_or(""), if l.rolling { "r" } else { "f" })
}

fn pool_model(spec: &UnitSpec, model: &str) -> String {
    if spec.per_model {
        model.to_string()
    } else {
        String::new()
    }
}

/// Bucket kind at the hub: `unit:neurons`, or `unit:neurons:<model>` for a per-model pool.
pub fn unit_kind(unit: &str, pool_model: &str) -> String {
    if pool_model.is_empty() {
        format!("unit:{unit}")
    } else {
        format!("unit:{unit}:{pool_model}")
    }
}

/// (used, tokens per model) in the current window, and when it frees up.
fn window_state(c: &Counter, l: Option<&UnitLimit>, now: Ts) -> (f64, BTreeMap<String, u64>, Option<Ts>) {
    let Some(l) = l else { return (c.used, c.tokens.clone(), None) };
    if l.rolling {
        let len = parse_duration_ms(&l.window).unwrap_or(0);
        let live: Vec<&Event> = c.events.iter().filter(|e| now - e.ts < len).collect();
        let mut tokens: BTreeMap<String, u64> = BTreeMap::new();
        for e in &live {
            *tokens.entry(e.model.clone()).or_default() += e.tokens;
        }
        (live.iter().map(|e| e.units).sum(), tokens, live.first().map(|e| e.ts + len))
    } else {
        match fixed_window(&l.as_quota_limit(), now) {
            Some((start, len)) if c.start == start => (c.used, c.tokens.clone(), Some(start + len)),
            Some((start, len)) => (0.0, BTreeMap::new(), Some(start + len)),
            None => (0.0, BTreeMap::new(), None),
        }
    }
}

pub struct Units {
    node: String,
    store: Arc<Store>,
    pools: Mutex<HashMap<Key, Pool>>,
    dirty: AtomicBool,
}

impl Units {
    pub fn new(node: &str, store: Arc<Store>) -> Units {
        let mut pools = HashMap::new();
        for (provider, scope_id, unit, model, body) in store.unit_rows().unwrap_or_default() {
            if let Ok(p) = serde_json::from_str::<Pool>(&body) {
                pools.insert((provider, scope_id, unit, model), p);
            }
        }
        Units { node: node.into(), store, pools: Mutex::new(pools), dirty: AtomicBool::new(true) }
    }

    /// Make sure pools exist so they show (and can be calibrated) before first use.
    pub fn ensure(&self, t: &Target, route: &str, model: &str, specs: &BTreeMap<String, UnitSpec>) {
        let mut m = self.pools.lock().unwrap();
        for (unit, spec) in specs {
            let key = (t.provider.clone(), t.scope_id.clone(), unit.clone(), pool_model(spec, model));
            let p = m.entry(key).or_insert_with(|| Pool { scope: t.scope.as_str().into(), source: "estimated".into(), ..Default::default() });
            if p.routes.insert(route.to_string()) {
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    /// If a pool this model draws from is used up: until when, and which unit.
    pub fn exhausted(&self, t: &Target, model: &str, specs: &BTreeMap<String, UnitSpec>, now: Ts) -> Option<(Ts, String)> {
        let m = self.pools.lock().unwrap();
        let mut out: Option<(Ts, String)> = None;
        for (unit, spec) in specs {
            let Some(p) = m.get(&(t.provider.clone(), t.scope_id.clone(), unit.clone(), pool_model(spec, model))) else { continue };
            for l in &spec.limits {
                let (used, _, reset) = p.counters.get(&limit_key(l)).map(|c| window_state(c, Some(l), now)).unwrap_or_default();
                if let Some(r) = reset.filter(|_| used >= l.max) {
                    if out.as_ref().is_none_or(|(u, _)| r > *u) {
                        out = Some((r, unit.clone()));
                    }
                }
            }
        }
        out
    }

    /// Account a finished call. `measured` is what the provider said it cost, by unit.
    pub fn record(&self, c: &UnitCall, specs: &BTreeMap<String, UnitSpec>, measured: &BTreeMap<String, f64>) {
        let now = now_ms();
        let tokens = c.input + c.output;
        let mut m = self.pools.lock().unwrap();
        for (unit, spec) in specs {
            let key = (c.target.provider.clone(), c.target.scope_id.clone(), unit.clone(), pool_model(spec, c.model));
            let p = m.entry(key).or_insert_with(|| Pool { scope: c.target.scope.as_str().into(), source: "estimated".into(), ..Default::default() });
            let said = measured.get(unit).copied().filter(|v| v.is_finite() && *v >= 0.0);
            if let Some(v) = said {
                let r = p.rates.entry(c.model.to_string()).or_default();
                if r.source != "measured" {
                    *r = Rate { source: "measured".into(), ..Default::default() };
                }
                r.units += v;
                r.tokens += tokens;
                if r.tokens > RATE_TOKENS {
                    r.units /= 2.0;
                    r.tokens /= 2;
                }
                p.source = "measured".into();
            }
            let cost = said.unwrap_or_else(|| estimate(p, spec, c));
            add(p, spec, c.model, cost, tokens, now);
            p.routes.insert(c.route.to_string());
            p.sessions.insert(c.session.to_string(), now);
            p.sessions.retain(|_, ts| now - *ts < 600_000);
            p.updated_at = now;
        }
        drop(m);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The provider says `used` units are gone in the current window: set the
    /// counters to that and learn what the tokens counted in the window cost.
    /// Rates the provider itself reports are not touched.
    pub fn calibrate(&self, provider: &str, scope_id: &str, unit: &str, model: Option<&str>, used: f64, spec: &UnitSpec) -> Result<(), String> {
        let now = now_ms();
        let mut m = self.pools.lock().unwrap();
        let pm = if spec.per_model { model.ok_or_else(|| format!("{unit} is per model: name the model"))?.to_string() } else { String::new() };
        let key = (provider.to_string(), scope_id.to_string(), unit.to_string(), pm);
        let p = m.get_mut(&key).ok_or_else(|| format!("no {unit} pool for {provider} {scope_id} on {}", self.node))?;
        // The window the operator is reading: the first limit's, else since the start.
        let first = spec.limits.first();
        let main = first.map(limit_key).unwrap_or(LIFETIME.to_string());
        let (_, tokens, _) = p.counters.get(&main).map(|c| window_state(c, first, now)).unwrap_or_default();
        learn_from_total(p, spec, used, &tokens);
        for l in &spec.limits {
            set_used(p.counters.entry(limit_key(l)).or_default(), l, used, now);
        }
        let life = p.counters.entry(LIFETIME.to_string()).or_default();
        life.used = life.used.max(used);
        if p.source != "measured" {
            p.source = "calibrated".into();
        }
        p.updated_at = now;
        drop(m);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Full view for the hub, next to the request and token buckets.
    pub fn snapshot(&self, specs: &BTreeMap<String, BTreeMap<String, UnitSpec>>) -> Vec<QuotaBucket> {
        let now = now_ms();
        let m = self.pools.lock().unwrap();
        let mut out = vec![];
        for ((provider, scope_id, unit, model), p) in m.iter() {
            // Gone from the config: keep the numbers, stop showing them.
            let Some(spec) = specs.get(provider).and_then(|u| u.get(unit)) else { continue };
            let mut used = p.counters.get(LIFETIME).map_or(0.0, |c| c.used);
            let (mut limit, mut remaining, mut resets_at, mut window) = (None, None, None, None);
            for l in &spec.limits {
                let (u, _, reset) = p.counters.get(&limit_key(l)).map(|c| window_state(c, Some(l), now)).unwrap_or_else(|| (0.0, BTreeMap::new(), fixed_window(&l.as_quota_limit(), now).map(|(s, n)| s + n)));
                let rem = (l.max - u).max(0.0);
                if remaining.is_none_or(|r| rem < r) {
                    used = u;
                    limit = Some(l.max);
                    remaining = Some(rem);
                    resets_at = reset;
                    window = Some(format!("{}{}", l.window, if l.rolling { " rolling" } else { "" }));
                }
            }
            let rates = p.rates.iter().filter_map(|(model, r)| Some(UnitModelRate { model: model.clone(), units_per_mtok: r.per_token()? * 1e6, source: r.source.clone() })).collect();
            out.push(QuotaBucket {
                provider: provider.clone(),
                scope: p.scope.clone(),
                scope_id: scope_id.clone(),
                kind: unit_kind(unit, model),
                limit: limit.map(|l| l.ceil() as u64),
                used: used.ceil() as u64,
                remaining: remaining.map(|r| r.floor() as u64),
                resets_at,
                exhausted: remaining.is_some_and(|r| r <= 0.0) && resets_at.is_some_and(|r| r > now),
                source: p.source.clone(),
                window,
                node: self.node.clone(),
                routes: p.routes.iter().cloned().collect(),
                sessions: p.sessions.iter().filter(|(_, ts)| now - **ts < 600_000).map(|(s, _)| s.clone()).collect(),
                updated_at: p.updated_at,
                unit: Some(UnitInfo { name: unit.clone(), model: (!model.is_empty()).then(|| model.clone()), used, limit, remaining, rates }),
            });
        }
        out.sort_by(|a, b| (&a.provider, &a.scope_id, &a.kind).cmp(&(&b.provider, &b.scope_id, &b.kind)));
        out
    }

    /// Persist and say whether anything changed since the last call.
    pub fn flush(&self) -> bool {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return false;
        }
        let rows: Vec<(String, String, String, String, String)> =
            self.pools.lock().unwrap().iter().filter_map(|((p, s, u, m), b)| Some((p.clone(), s.clone(), u.clone(), m.clone(), serde_json::to_string(b).ok()?))).collect();
        if let Err(e) = self.store.put_unit_rows(&rows) {
            tracing::warn!(error = %e, "persisting provider units failed");
        }
        true
    }
}

/// What a call cost when the provider did not say: learned, then configured.
fn estimate(p: &Pool, spec: &UnitSpec, c: &UnitCall) -> f64 {
    if let Some(r) = p.rates.get(c.model).and_then(Rate::per_token) {
        return r * (c.input + c.output) as f64;
    }
    match spec.per_mtok.get(c.model).or_else(|| spec.per_mtok.get("default")) {
        Some(r) => (c.input as f64 * r.input + c.output as f64 * r.output) / 1e6,
        None => 0.0,
    }
}

fn add(p: &mut Pool, spec: &UnitSpec, model: &str, units: f64, tokens: u64, now: Ts) {
    for l in &spec.limits {
        let c = p.counters.entry(limit_key(l)).or_default();
        if l.rolling {
            let len = parse_duration_ms(&l.window).unwrap_or(0);
            c.events.push_back(Event { ts: now, units, model: model.to_string(), tokens });
            while c.events.front().is_some_and(|e| now - e.ts >= len) {
                c.events.pop_front();
            }
        } else if let Some((start, _)) = fixed_window(&l.as_quota_limit(), now) {
            if c.start != start {
                *c = Counter { start, ..Default::default() };
            }
            c.used += units;
            *c.tokens.entry(model.to_string()).or_default() += tokens;
        }
    }
    let c = p.counters.entry(LIFETIME.to_string()).or_default();
    c.used += units;
    *c.tokens.entry(model.to_string()).or_default() += tokens;
}

fn set_used(c: &mut Counter, l: &UnitLimit, used: f64, now: Ts) {
    if l.rolling {
        let len = parse_duration_ms(&l.window).unwrap_or(0);
        c.events.retain(|e| now - e.ts < len);
        let sum: f64 = c.events.iter().map(|e| e.units).sum();
        if sum > 0.0 {
            // Keep when each call leaves the window; only the amounts were off.
            let f = used / sum;
            c.events.iter_mut().for_each(|e| e.units *= f);
        } else if !c.events.is_empty() {
            let n = c.events.len() as f64;
            c.events.iter_mut().for_each(|e| e.units = used / n);
        } else {
            c.events.push_back(Event { ts: now, units: used, ..Default::default() });
        }
    } else if let Some((start, _)) = fixed_window(&l.as_quota_limit(), now) {
        if c.start != start {
            *c = Counter { start, ..Default::default() };
        }
        c.used = used;
    }
}

/// `used` units paid for `tokens`. One figure cannot tell models apart, so
/// models with a rate keep their proportions and the rest share what is left.
fn learn_from_total(p: &mut Pool, spec: &UnitSpec, used: f64, tokens: &BTreeMap<String, u64>) {
    let rate_of = |p: &Pool, m: &str| -> Option<f64> {
        p.rates.get(m).and_then(Rate::per_token).or_else(|| {
            let r = spec.per_mtok.get(m).or_else(|| spec.per_mtok.get("default"))?;
            // No split of the tokens is known here: take the mean of the two prices.
            Some((r.input + r.output) / 2e6)
        })
    };
    let open: Vec<(&String, u64)> = tokens.iter().filter(|(m, n)| **n > 0 && p.rates.get(*m).is_none_or(|r| r.source != "measured")).map(|(m, n)| (m, *n)).collect();
    let fixed: f64 = tokens.iter().filter(|(m, n)| **n > 0 && p.rates.get(*m).is_some_and(|r| r.source == "measured")).filter_map(|(m, n)| Some(rate_of(p, m)? * *n as f64)).sum();
    let left = used - fixed;
    if open.is_empty() || left <= 0.0 {
        return;
    }
    let known_cost: f64 = open.iter().filter_map(|(m, n)| Some(rate_of(p, m)? * *n as f64)).sum();
    let unknown_tokens: u64 = open.iter().filter(|(m, _)| rate_of(p, m).is_none()).map(|(_, n)| n).sum();
    let total: u64 = open.iter().map(|(_, n)| n).sum();
    let new: Vec<(String, f64)> = open
        .iter()
        .map(|(m, _)| {
            let per_token = match rate_of(p, m) {
                Some(r) if unknown_tokens == 0 && known_cost > 0.0 => r * left / known_cost,
                Some(r) if left > known_cost => r,
                None if left > known_cost => (left - known_cost) / unknown_tokens as f64,
                // The known rates alone overshoot the figure: they were wrong too.
                _ => left / total as f64,
            };
            (m.to_string(), per_token)
        })
        .collect();
    for (m, per_token) in new {
        p.rates.insert(m, Rate { units: per_token * 1e6, tokens: 1_000_000, source: "calibrated".into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target { provider: "cf".into(), scope: QuotaScope::Key, scope_id: "key:abc".into() }
    }

    fn call<'a>(t: &'a Target, model: &'a str, input: u64, output: u64) -> UnitCall<'a> {
        UnitCall { target: t, route: "direct", session: "s-1", model, input, output }
    }

    fn spec(per_model: bool, max: f64) -> UnitSpec {
        UnitSpec { limits: vec![UnitLimit { max, window: "1d".into(), at: Some("00:00".into()), tz: None, rolling: false }], per_model, header: None, usage_field: None, per_mtok: BTreeMap::new() }
    }

    fn units() -> (tempfile::TempDir, Arc<Store>, Units) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("n.db")).unwrap());
        let u = Units::new("n1", store.clone());
        (dir, store, u)
    }

    fn view(u: &Units, specs: &BTreeMap<String, UnitSpec>, kind: &str) -> QuotaBucket {
        u.snapshot(&BTreeMap::from([("cf".to_string(), specs.clone())])).into_iter().find(|b| b.kind == kind).unwrap()
    }

    #[test]
    fn measured_costs_differ_per_model_and_exhaust_a_shared_pool() {
        let (_d, store, u) = units();
        let t = target();
        let specs = BTreeMap::from([("neurons".to_string(), spec(false, 100.0))]);
        u.record(&call(&t, "big", 1000, 0), &specs, &BTreeMap::from([("neurons".to_string(), 40.0)]));
        u.record(&call(&t, "small", 1000, 0), &specs, &BTreeMap::from([("neurons".to_string(), 4.0)]));
        let b = view(&u, &specs, "unit:neurons");
        let i = b.unit.clone().unwrap();
        assert_eq!((i.used, i.remaining, b.source.as_str()), (44.0, Some(56.0), "measured"));
        let rate = |m: &str| i.rates.iter().find(|r| r.model == m).unwrap().units_per_mtok;
        assert_eq!((rate("big"), rate("small")), (40_000.0, 4_000.0));
        // No figure from the provider this time: the model's own rate applies.
        u.record(&call(&t, "big", 500, 0), &specs, &BTreeMap::new());
        assert_eq!(view(&u, &specs, "unit:neurons").unit.unwrap().used, 64.0);
        assert!(u.exhausted(&t, "small", &specs, now_ms()).is_none());
        u.record(&call(&t, "big", 1000, 0), &specs, &BTreeMap::from([("neurons".to_string(), 40.0)]));
        // Shared: the big model used it up for the small one too.
        let (until, unit) = u.exhausted(&t, "small", &specs, now_ms()).unwrap();
        assert!(until > now_ms() && unit == "neurons");
        assert!(view(&u, &specs, "unit:neurons").exhausted);
        // Survives a restart.
        assert!(u.flush());
        assert!(Units::new("n1", store).exhausted(&t, "small", &specs, now_ms()).is_some());
    }

    #[test]
    fn per_model_pools_exhaust_separately() {
        let (_d, _s, u) = units();
        let t = target();
        let specs = BTreeMap::from([("credits".to_string(), spec(true, 10.0))]);
        u.record(&call(&t, "big", 100, 0), &specs, &BTreeMap::from([("credits".to_string(), 10.0)]));
        assert!(u.exhausted(&t, "big", &specs, now_ms()).is_some());
        assert!(u.exhausted(&t, "small", &specs, now_ms()).is_none());
        assert!(view(&u, &specs, "unit:credits:big").exhausted);
    }

    #[test]
    fn starting_estimate_splits_input_and_output() {
        let (_d, _s, u) = units();
        let t = target();
        let mut s = spec(false, 1000.0);
        s.per_mtok.insert("default".into(), UnitRate { input: 10.0, output: 100.0 });
        s.per_mtok.insert("big".into(), UnitRate { input: 1000.0, output: 2000.0 });
        let specs = BTreeMap::from([("n".to_string(), s)]);
        u.record(&call(&t, "other", 1_000_000, 1_000_000), &specs, &BTreeMap::new());
        assert_eq!(view(&u, &specs, "unit:n").unit.unwrap().used, 110.0);
        u.record(&call(&t, "big", 100_000, 0), &specs, &BTreeMap::new());
        assert_eq!(view(&u, &specs, "unit:n").unit.unwrap().used, 210.0);
    }

    #[test]
    fn calibration_sets_usage_and_learns_what_tokens_cost() {
        let (_d, _s, u) = units();
        let t = target();
        let specs = BTreeMap::from([("n".to_string(), spec(false, 1000.0))]);
        // Nothing known: the calls count as zero until the dashboard is read.
        u.record(&call(&t, "m", 2000, 0), &specs, &BTreeMap::new());
        assert_eq!(view(&u, &specs, "unit:n").unit.unwrap().used, 0.0);
        u.calibrate("cf", "key:abc", "n", None, 50.0, &specs["n"]).unwrap();
        let b = view(&u, &specs, "unit:n");
        let i = b.unit.unwrap();
        assert_eq!((i.used, b.source.as_str(), i.rates[0].source.as_str()), (50.0, "calibrated", "calibrated"));
        assert_eq!(i.rates[0].units_per_mtok, 25_000.0);
        // And the next call is estimated with it.
        u.record(&call(&t, "m", 1000, 0), &specs, &BTreeMap::new());
        assert_eq!(view(&u, &specs, "unit:n").unit.unwrap().used, 75.0);
        // A new model takes what the known one does not explain.
        u.record(&call(&t, "m2", 3000, 0), &specs, &BTreeMap::new());
        u.calibrate("cf", "key:abc", "n", None, 225.0, &specs["n"]).unwrap();
        let rate = |u: &Units, m: &str| view(u, &specs, "unit:n").unit.unwrap().rates.iter().find(|r| r.model == m).unwrap().units_per_mtok;
        assert_eq!(view(&u, &specs, "unit:n").unit.unwrap().used, 225.0);
        assert_eq!((rate(&u, "m"), rate(&u, "m2")), (25_000.0, 50_000.0));
        // With every model known, a reading scales them and keeps their proportions.
        u.calibrate("cf", "key:abc", "n", None, 450.0, &specs["n"]).unwrap();
        assert_eq!((rate(&u, "m"), rate(&u, "m2")), (50_000.0, 100_000.0));
        assert!(u.calibrate("cf", "key:other", "n", None, 1.0, &specs["n"]).is_err());
    }

    #[test]
    fn calibration_leaves_measured_rates_alone() {
        let (_d, _s, u) = units();
        let t = target();
        let specs = BTreeMap::from([("n".to_string(), spec(false, 1000.0))]);
        u.record(&call(&t, "m", 1000, 0), &specs, &BTreeMap::from([("n".to_string(), 5.0)]));
        u.calibrate("cf", "key:abc", "n", None, 500.0, &specs["n"]).unwrap();
        let b = view(&u, &specs, "unit:n");
        let i = b.unit.unwrap();
        assert_eq!((i.used, i.rates[0].units_per_mtok, b.source.as_str()), (500.0, 5000.0, "measured"));
    }

    #[test]
    fn rolling_windows_calibrate_in_place() {
        let (_d, _s, u) = units();
        let t = target();
        let mut s = spec(false, 100.0);
        s.limits[0] = UnitLimit { max: 100.0, window: "1h".into(), at: None, tz: None, rolling: true };
        let specs = BTreeMap::from([("n".to_string(), s)]);
        u.record(&call(&t, "m", 1000, 0), &specs, &BTreeMap::from([("n".to_string(), 10.0)]));
        u.calibrate("cf", "key:abc", "n", None, 100.0, &specs["n"]).unwrap();
        assert!(u.exhausted(&t, "m", &specs, now_ms()).is_some());
    }
}
