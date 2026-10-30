//! keyproxy's own usage meter (ARCHITECTURE §6: "a second, authoritative
//! meter for budgets").
//!
//! The tokens OpenCode reports come from inside the container, where the
//! agent has a shell. keyproxy sees every provider response, so it reads the
//! usage the provider itself reports, from JSON bodies and from SSE streams,
//! chunk by chunk, without holding the response back:
//!
//! * OpenAI-compatible chat: `usage.{prompt_tokens, completion_tokens,
//!   prompt_tokens_details.cached_tokens, completion_tokens_details.reasoning_tokens}`
//!   (streams: the final chunk; keyproxy asks for it with `stream_options.include_usage`);
//! * OpenAI Responses: `response.usage.{input_tokens, output_tokens, …}`;
//! * Anthropic: `message_start.message.usage` + `message_delta.usage.output_tokens`;
//! * Gemini: `usageMetadata.{promptTokenCount, candidatesTokenCount, thoughtsTokenCount, cachedContentTokenCount}`.

use moocher_protocol::config::{ApiKind, Price};
use moocher_protocol::model::TokenUsage;
use serde_json::Value;

const MAX_JSON: usize = 8 << 20;

#[derive(Debug)]
pub struct Scanner {
    api: ApiKind,
    sse: bool,
    buf: Vec<u8>,
    usage: TokenUsage,
    seen: bool,
    overflow: bool,
    /// Extra numbers wanted from the provider's usage: (name, dotted path).
    fields: Vec<(String, String)>,
    found: Option<Found>,
}

/// Numbers found for `Meter::watch`, by name.
pub type Found = std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, f64>>>;

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

impl Scanner {
    pub fn new(api: ApiKind, content_type: &str) -> Scanner {
        Scanner { api, sse: content_type.contains("event-stream"), buf: vec![], usage: TokenUsage::default(), seen: false, overflow: false, fields: vec![], found: None }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        if self.overflow {
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.sse {
            // Handle complete lines; keep the partial tail.
            while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line);
                if let Some(data) = line.trim().strip_prefix("data:") {
                    if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                        self.event(&v);
                    }
                }
            }
        }
        if self.buf.len() > MAX_JSON {
            self.overflow = true;
            self.buf.clear();
        }
    }

    /// Usage the provider reported, if it reported any.
    pub fn finish(mut self) -> Option<TokenUsage> {
        if !self.sse && !self.overflow {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.buf) {
                self.event(&v);
            }
        } else if self.sse && !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.feed(&[rest.as_slice(), b"\n"].concat());
        }
        self.seen.then_some(self.usage)
    }

    fn event(&mut self, v: &Value) {
        if let Some(found) = &self.found {
            for (name, path) in &self.fields {
                for root in [&v["usage"], &v["response"]["usage"], &v["message"]["usage"], &v["usageMetadata"]] {
                    if let Some(n) = path.split('.').fold(root, |r, k| &r[k]).as_f64() {
                        found.lock().unwrap().insert(name.clone(), n);
                    }
                }
            }
        }
        match self.api {
            ApiKind::Openai => {
                // Chat completions (final chunk or whole body).
                let u = &v["usage"];
                if u.is_object() && (u.get("prompt_tokens").is_some() || u.get("completion_tokens").is_some()) {
                    let cached = n(&u["prompt_tokens_details"]["cached_tokens"]);
                    let reasoning = n(&u["completion_tokens_details"]["reasoning_tokens"]);
                    self.usage = TokenUsage {
                        input: n(&u["prompt_tokens"]).saturating_sub(cached),
                        output: n(&u["completion_tokens"]).saturating_sub(reasoning),
                        reasoning,
                        cache_read: cached,
                        cache_write: 0,
                        cost_usd: 0.0,
                    };
                    self.seen = true;
                    return;
                }
                // Responses API: `{"type":"response.completed","response":{"usage":…}}` or a whole response.
                let u = if v["response"]["usage"].is_object() { &v["response"]["usage"] } else { &v["usage"] };
                if u.get("input_tokens").is_some() {
                    let cached = n(&u["input_tokens_details"]["cached_tokens"]);
                    let reasoning = n(&u["output_tokens_details"]["reasoning_tokens"]);
                    self.usage = TokenUsage {
                        input: n(&u["input_tokens"]).saturating_sub(cached),
                        output: n(&u["output_tokens"]).saturating_sub(reasoning),
                        reasoning,
                        cache_read: cached,
                        cache_write: 0,
                        cost_usd: 0.0,
                    };
                    self.seen = true;
                }
            }
            ApiKind::Anthropic => {
                // Streams: message_start carries input, message_delta the (cumulative) output.
                let start = if v["type"] == "message_start" { &v["message"]["usage"] } else if v["type"] == "message" { &v["usage"] } else { &Value::Null };
                if start.is_object() {
                    self.usage.input = n(&start["input_tokens"]);
                    self.usage.cache_read = n(&start["cache_read_input_tokens"]);
                    self.usage.cache_write = n(&start["cache_creation_input_tokens"]);
                    self.usage.output = self.usage.output.max(n(&start["output_tokens"]));
                    self.seen = true;
                }
                if v["type"] == "message_delta" && v["usage"].is_object() {
                    self.usage.output = n(&v["usage"]["output_tokens"]);
                    self.seen = true;
                }
            }
            ApiKind::Google => {
                let u = &v["usageMetadata"];
                if u.is_object() {
                    let cached = n(&u["cachedContentTokenCount"]);
                    // Later chunks carry the running totals: keep the latest.
                    self.usage = TokenUsage {
                        input: n(&u["promptTokenCount"]).saturating_sub(cached),
                        output: n(&u["candidatesTokenCount"]),
                        reasoning: n(&u["thoughtsTokenCount"]),
                        cache_read: cached,
                        cache_write: 0,
                        cost_usd: 0.0,
                    };
                    self.seen = true;
                }
            }
            // Never proxied: OpenCode reports its usage itself.
            ApiKind::Opencode => {}
        }
    }
}

/// A session's metered total, and a wake-up for whoever reports it.
#[derive(Default)]
pub struct Metered {
    pub usage: std::sync::Mutex<TokenUsage>,
    pub changed: tokio::sync::Notify,
}

impl Metered {
    pub fn new(u: TokenUsage) -> Metered {
        Metered { usage: std::sync::Mutex::new(u), changed: Default::default() }
    }
    pub fn get(&self) -> TokenUsage {
        *self.usage.lock().unwrap()
    }
    pub fn add(&self, u: &TokenUsage) {
        self.usage.lock().unwrap().add(u);
        self.changed.notify_one();
    }
}

/// The meter is ahead of OpenCode by more than in-flight noise: 25% and 2000 tokens.
pub fn mismatched(reported: u64, metered: u64) -> bool {
    metered > reported + 2000 && metered * 4 > reported * 5
}

/// Cost from the provider's price list (reasoning is billed as output).
pub fn price(u: &mut TokenUsage, p: Option<&Price>) {
    if let Some(p) = p {
        u.cost_usd = (u.input as f64 * p.input_per_mtok + (u.output + u.reasoning) as f64 * p.output_per_mtok) / 1e6;
    }
}

/// Scans one response as it streams through and reports its usage once the
/// body is done or dropped (the client went away mid-stream).
pub struct Meter {
    scanner: Option<Scanner>,
    done: Option<Box<dyn FnOnce(TokenUsage) + Send>>,
}

impl Meter {
    pub fn new(api: ApiKind, content_type: &str, done: impl FnOnce(TokenUsage) + Send + 'static) -> Meter {
        Meter { scanner: Some(Scanner::new(api, content_type)), done: Some(Box::new(done)) }
    }
    /// Also read these numbers from the usage the provider reports.
    pub fn watch(mut self, fields: Vec<(String, String)>, found: Found) -> Meter {
        if let Some(s) = &mut self.scanner {
            s.fields = fields;
            s.found = Some(found);
        }
        self
    }
    pub fn feed(&mut self, chunk: &[u8]) {
        if let Some(s) = &mut self.scanner {
            s.feed(chunk);
        }
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        if let (Some(s), Some(done)) = (self.scanner.take(), self.done.take()) {
            if let Some(u) = s.finish() {
                done(u);
            }
        }
    }
}

/// For OpenAI-compatible streams, ask for the usage chunk if the request does not.
pub fn ensure_stream_usage(api: ApiKind, body: &mut Value) -> bool {
    if api != ApiKind::Openai || body.get("stream") != Some(&Value::Bool(true)) || body.get("input").is_some() {
        return false; // the Responses API reports usage in response.completed anyway
    }
    match body.get_mut("stream_options") {
        Some(o) if o.get("include_usage").is_some() => false,
        Some(Value::Object(o)) => {
            o.insert("include_usage".into(), Value::Bool(true));
            true
        }
        _ => {
            body["stream_options"] = serde_json::json!({"include_usage": true});
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scan(api: ApiKind, ct: &str, parts: &[&str]) -> Option<TokenUsage> {
        let mut s = Scanner::new(api, ct);
        for p in parts {
            s.feed(p.as_bytes());
        }
        s.finish()
    }

    #[test]
    fn openai_json_and_stream_split_anywhere() {
        let body = r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":20},"completion_tokens_details":{"reasoning_tokens":10}}}"#;
        let u = scan(ApiKind::Openai, "application/json", &[&body[..40], &body[40..]]).unwrap();
        assert_eq!((u.input, u.cache_read, u.output, u.reasoning), (100, 20, 20, 10));
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n";
        for cut in [1, 17, 60, sse.len() - 3] {
            let u = scan(ApiKind::Openai, "text/event-stream", &[&sse[..cut], &sse[cut..]]).unwrap();
            assert_eq!((u.input, u.output), (7, 3), "cut at {cut}");
        }
        assert!(scan(ApiKind::Openai, "text/event-stream", &["data: {\"choices\":[]}\n\n"]).is_none(), "no usage reported");
    }

    #[test]
    fn responses_anthropic_gemini() {
        let r = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":10}}}}\n\n";
        let u = scan(ApiKind::Openai, "text/event-stream", &[r]).unwrap();
        assert_eq!((u.input, u.cache_read, u.output), (40, 10, 9));
        let a = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n"
        );
        let u = scan(ApiKind::Anthropic, "text/event-stream", &[a]).unwrap();
        assert_eq!((u.input, u.cache_read, u.cache_write, u.output), (25, 100, 5, 42));
        let g = "data: {\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":2}}\r\n\r\ndata: {\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":8,\"thoughtsTokenCount\":4}}\r\n\r\n";
        let u = scan(ApiKind::Google, "text/event-stream", &[g]).unwrap();
        assert_eq!((u.input, u.output, u.reasoning), (12, 8, 4));
    }

    #[test]
    fn meter_reports_on_drop_and_mismatch_threshold() {
        let got = std::sync::Arc::new(std::sync::Mutex::new(None));
        let g2 = got.clone();
        let mut m = Meter::new(ApiKind::Openai, "text/event-stream", move |u| *g2.lock().unwrap() = Some(u));
        m.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}");
        drop(m); // no trailing newline: the tail still counts
        assert_eq!(got.lock().unwrap().map(|u| u.total()), Some(6));
        assert!(!mismatched(10_000, 12_000));
        assert!(mismatched(10_000, 12_600));
        assert!(!mismatched(0, 1_500));
        assert!(mismatched(0, 2_500));
    }

    #[test]
    fn asks_for_stream_usage() {
        let mut b = json!({"model": "m", "stream": true});
        assert!(ensure_stream_usage(ApiKind::Openai, &mut b));
        assert_eq!(b["stream_options"]["include_usage"], true);
        assert!(!ensure_stream_usage(ApiKind::Openai, &mut b), "only once");
        let mut b = json!({"model": "m", "stream": false});
        assert!(!ensure_stream_usage(ApiKind::Openai, &mut b));
        let mut b = json!({"model": "m", "stream": true});
        assert!(!ensure_stream_usage(ApiKind::Anthropic, &mut b));
    }
}
