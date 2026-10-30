//! What keyproxy learns about providers from the calls it makes: latency to
//! the first byte, output speed, failures by class, tokens, and notable
//! events (a context overflow with the limit the provider named, credits
//! running out). Kept as hourly rows per (provider, model, scope, route),
//! persisted in node.db so a restart keeps counting, and reported to the hub
//! once a minute. The context limits learned here also cap the `context:`
//! that new sessions of the same model are configured with.

use crate::store::Store;
use moocher_protocol::model::{now_ms, ObsRow, ProviderEvent, Ts, OBS_MS_BUCKETS, OBS_TPS_BUCKETS};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const HOUR: Ts = 3_600_000;
/// Error bodies are read up to this much for classification.
pub const ERR_BODY: usize = 8 << 10;

type Key = (Ts, String, String, String, String);

/// One call, as keyproxy saw it.
#[derive(Debug, Clone, Default)]
pub struct Call {
    pub provider: String,
    pub model: String,
    pub scope_id: String,
    pub route: String,
    pub session: String,
    pub status: Option<u16>,
    pub ttft_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// The start of an error body (for classification).
    pub error_body: String,
    /// The request never got an answer (connection, TLS, relay).
    pub unreachable: Option<String>,
}

/// Why a call failed, and what it teaches.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub class: &'static str,
    /// For a context overflow: the window the provider named, if it did.
    pub limit: Option<u64>,
}

const OVERFLOW: [&str; 14] = [
    "context length",
    "context_length",
    "context window",
    "maximum context",
    "too many tokens",
    "prompt is too long",
    "input is too long",
    "reduce the length",
    "maximum number of tokens",
    "input token count",
    "tokens exceeds",
    "exceeds the context",
    "token limit",
    "too long for the model",
];

const CREDITS: [&str; 11] = [
    "insufficient_quota",
    "insufficient credit",
    "insufficient balance",
    "insufficient funds",
    "credit balance is too low",
    "out of credits",
    "not enough credits",
    "exceeded your current quota",
    "payment required",
    "no credits",
    "requires more credits",
];

/// Phrases that put the window size right after them.
const LIMIT_AFTER: [&str; 12] = [
    "maximum context length is",
    "context length is",
    "context length of",
    "context window of",
    "context window is",
    "maximum context length of",
    "maximum of",
    "allowed (",
    "limit of",
    "limit is",
    "max context",
    "> ",
];

fn first_int(s: &str) -> Option<u64> {
    let t = s.trim_start_matches(|c: char| !c.is_ascii_digit());
    if s.len() - t.len() > 12 {
        return None; // too far from the phrase
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_').filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Classify a failed call from its status and the start of its body.
pub fn classify(status: u16, body: &str) -> Verdict {
    let b = body.to_ascii_lowercase();
    if status == 402 || CREDITS.iter().any(|p| b.contains(p)) {
        return Verdict { class: "credits", limit: None };
    }
    if (400..500).contains(&status) && status != 429 && OVERFLOW.iter().any(|p| b.contains(p)) {
        let limit = LIMIT_AFTER.iter().filter_map(|p| b.find(p).and_then(|i| first_int(&b[i + p.len()..]))).find(|n| *n >= 512);
        return Verdict { class: "context_overflow", limit };
    }
    let class = match status {
        429 => "rate_limited",
        401 | 403 => "auth",
        400..=499 => "client",
        _ => "server",
    };
    Verdict { class, limit: None }
}

pub struct Observer {
    node: String,
    store: Arc<Store>,
    rows: Mutex<BTreeMap<Key, ObsRow>>,
    events: Mutex<Vec<ProviderEvent>>,
    /// (provider, model) -> smallest window an overflow named.
    learned: Mutex<BTreeMap<(String, String), u64>>,
    dirty: AtomicBool,
}

impl Observer {
    pub fn new(node: &str, store: Arc<Store>) -> Arc<Observer> {
        let since = now_ms() / HOUR * HOUR - HOUR;
        let rows = store.obs_rows(since).into_iter().map(|r| ((r.hour, r.provider.clone(), r.model.clone(), r.scope_id.clone(), r.route.clone()), r)).collect();
        let learned = store.learned_contexts().into_iter().collect();
        Arc::new(Observer { node: node.into(), store, rows: Mutex::new(rows), events: Mutex::new(vec![]), learned: Mutex::new(learned), dirty: AtomicBool::new(false) })
    }

    /// The smallest context window a provider named for this model, if any.
    pub fn learned_context(&self, provider: &str, model: &str) -> Option<u64> {
        self.learned.lock().unwrap().get(&(provider.to_string(), model.to_string())).copied()
    }

    pub fn record(&self, c: &Call) {
        let now = now_ms();
        let hour = now / HOUR * HOUR;
        let verdict = match (&c.unreachable, c.status) {
            (Some(_), _) => Some(Verdict { class: "unreachable", limit: None }),
            (None, Some(s)) if s >= 400 => Some(classify(s, &c.error_body)),
            _ => None,
        };
        {
            let mut rows = self.rows.lock().unwrap();
            let r = rows.entry((hour, c.provider.clone(), c.model.clone(), c.scope_id.clone(), c.route.clone())).or_insert_with(|| ObsRow {
                hour,
                node: self.node.clone(),
                provider: c.provider.clone(),
                model: c.model.clone(),
                scope_id: c.scope_id.clone(),
                route: c.route.clone(),
                ..Default::default()
            });
            r.requests += 1;
            match &verdict {
                Some(v) => *r.errors.entry(v.class.to_string()).or_default() += 1,
                None => r.ok += 1,
            }
            if verdict.is_none() {
                if let Some(t) = c.ttft_ms {
                    r.ttft_ms.add(&OBS_MS_BUCKETS, t);
                }
                if let Some(d) = c.duration_ms {
                    r.duration_ms.add(&OBS_MS_BUCKETS, d);
                    // Speed after the first byte; a short answer says little about it.
                    let gen_ms = d - c.ttft_ms.unwrap_or(0.0);
                    if c.output_tokens >= 20 && gen_ms > 50.0 {
                        r.tokens_per_s.add(&OBS_TPS_BUCKETS, c.output_tokens as f64 / (gen_ms / 1000.0));
                    }
                }
            }
            r.input_tokens += c.input_tokens;
            r.output_tokens += c.output_tokens;
            r.cost_usd += c.cost_usd;
            // Keep two hours in memory.
            rows.retain(|k, _| k.0 >= hour - HOUR);
            if let Some(r) = rows.get(&(hour, c.provider.clone(), c.model.clone(), c.scope_id.clone(), c.route.clone())) {
                let _ = self.store.put_obs_row(r);
            }
        }
        if let Some(v) = &verdict {
            let kind = match v.class {
                "context_overflow" => "context_overflow",
                "credits" => "credits_exhausted",
                "auth" => "auth_failed",
                _ => "",
            };
            if !kind.is_empty() {
                let mut detail = json!({"status": c.status, "message": c.error_body.chars().take(400).collect::<String>()});
                if let Some(l) = v.limit {
                    detail["limit"] = json!(l);
                    let mut learned = self.learned.lock().unwrap();
                    let k = (c.provider.clone(), c.model.clone());
                    if learned.get(&k).is_none_or(|old| l < *old) {
                        learned.insert(k, l);
                        let _ = self.store.put_learned_context(&c.provider, &c.model, l);
                        tracing::info!(provider = %c.provider, model = %c.model, limit = l, "learned a context window from an overflow");
                    }
                }
                self.event(c, kind, detail);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn event(&self, c: &Call, kind: &str, detail: Value) {
        self.events.lock().unwrap().push(ProviderEvent {
            ts: now_ms(),
            node: self.node.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            scope_id: c.scope_id.clone(),
            route: c.route.clone(),
            session: (!c.session.is_empty()).then(|| c.session.clone()),
            kind: kind.into(),
            detail,
        });
    }

    /// Rows touched in the last two hours and the new events, if anything changed.
    pub fn take(&self) -> Option<(Vec<ObsRow>, Vec<ProviderEvent>)> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return None;
        }
        let rows = self.rows.lock().unwrap().values().cloned().collect();
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        Some((rows, events))
    }
}

/// Measures one streamed answer: first byte, end, error body. Records the
/// call when the response body is dropped (finished or abandoned).
pub struct Probe {
    obs: Arc<Observer>,
    call: Call,
    sent: Instant,
    first: Option<Instant>,
    /// Filled by the usage meter when the provider reports usage.
    pub usage: Arc<Mutex<Option<(u64, u64, f64)>>>,
}

impl Probe {
    pub fn new(obs: Arc<Observer>, call: Call, sent: Instant) -> Probe {
        Probe { obs, call, sent, first: None, usage: Arc::new(Mutex::new(None)) }
    }

    pub fn chunk(&mut self, b: &[u8]) {
        if self.first.is_none() {
            self.first = Some(Instant::now());
        }
        if self.call.status.is_some_and(|s| s >= 400) && self.call.error_body.len() < ERR_BODY {
            let room = ERR_BODY - self.call.error_body.len();
            self.call.error_body.push_str(&String::from_utf8_lossy(&b[..b.len().min(room)]));
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let end = Instant::now();
        self.call.duration_ms = Some(end.duration_since(self.sent).as_secs_f64() * 1000.0);
        self.call.ttft_ms = self.first.map(|f| f.duration_since(self.sent).as_secs_f64() * 1000.0);
        if let Some((i, o, cost)) = *self.usage.lock().unwrap() {
            self.call.input_tokens = i;
            self.call.output_tokens = o;
            self.call.cost_usd = cost;
        }
        self.obs.record(&self.call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_errors() {
        let c = |s, b| classify(s, b);
        assert_eq!(c(400, r#"{"error":{"message":"This model's maximum context length is 131072 tokens. However, you requested 140000 tokens (136000 in the messages, 4000 in the completion).","code":"context_length_exceeded"}}"#), Verdict { class: "context_overflow", limit: Some(131072) });
        assert_eq!(c(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#), Verdict { class: "context_overflow", limit: Some(200000) });
        assert_eq!(c(400, "The input token count (1234567) exceeds the maximum number of tokens allowed (1048576)."), Verdict { class: "context_overflow", limit: Some(1048576) });
        assert_eq!(c(413, "Request too large: too many tokens"), Verdict { class: "context_overflow", limit: None });
        assert_eq!(c(429, r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#).class, "credits");
        assert_eq!(c(402, "").class, "credits");
        assert_eq!(c(429, "Rate limit reached for requests per minute").class, "rate_limited");
        assert_eq!(c(429, "Quota exceeded for metric generate_content_requests per minute").class, "rate_limited");
        assert_eq!(c(401, "invalid api key").class, "auth");
        assert_eq!(c(400, "bad request").class, "client");
        assert_eq!(c(503, "overloaded").class, "server");
    }

    #[test]
    fn records_hours_and_learns_limits() {
        let t = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&t.path().join("n.db")).unwrap());
        let o = Observer::new("hub", store.clone());
        let base = Call { provider: "groq".into(), model: "m".into(), scope_id: "k1".into(), route: "direct".into(), ..Default::default() };
        o.record(&Call { status: Some(200), ttft_ms: Some(300.0), duration_ms: Some(2300.0), output_tokens: 100, ..base.clone() });
        o.record(&Call { status: Some(400), error_body: "maximum context length is 8192 tokens".into(), ..base.clone() });
        o.record(&Call { status: Some(400), error_body: "maximum context length is 32768 tokens".into(), ..base.clone() });
        o.record(&Call { unreachable: Some("timeout".into()), ..base.clone() });
        let (rows, events) = o.take().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.requests, r.ok, r.errors["context_overflow"], r.errors["unreachable"]), (4, 1, 2, 1));
        assert_eq!(r.tokens_per_s.mean().map(|v| v.round()), Some(50.0));
        assert_eq!(events.iter().filter(|e| e.kind == "context_overflow").count(), 2);
        assert_eq!(o.learned_context("groq", "m"), Some(8192));
        assert!(o.take().is_none());
        // A restart picks the hour and the learned window up again.
        let o2 = Observer::new("hub", store);
        assert_eq!(o2.learned_context("groq", "m"), Some(8192));
        o2.record(&Call { status: Some(200), ..base });
        assert_eq!(o2.take().unwrap().0[0].requests, 5);
    }
}
