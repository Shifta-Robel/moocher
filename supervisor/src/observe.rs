//! What the cluster has seen of each provider model: speed, failures,
//! context windows and credit exhaustion (from keyproxy's observations on
//! every node), summarised for people, agents and placement.

use crate::state::Sup;
use moocher_protocol::model::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const HOUR: Ts = 3_600_000;

/// Totals of a set of hourly rows.
#[derive(Default)]
struct Agg {
    requests: u64,
    ok: u64,
    errors: BTreeMap<String, u64>,
    ttft: Hist,
    tps: Hist,
    input: u64,
    output: u64,
    cost: f64,
}

impl Agg {
    fn add(&mut self, r: &ObsRow) {
        self.requests += r.requests;
        self.ok += r.ok;
        for (k, v) in &r.errors {
            *self.errors.entry(k.clone()).or_default() += v;
        }
        self.ttft.merge(&r.ttft_ms);
        self.tps.merge(&r.tokens_per_s);
        self.input += r.input_tokens;
        self.output += r.output_tokens;
        self.cost += r.cost_usd;
    }

    fn json(&self) -> Value {
        let round = |v: Option<f64>| v.map(|x| (x * 10.0).round() / 10.0);
        json!({
            "requests": self.requests,
            "ok": self.ok,
            "error_rate": if self.requests > 0 { ((self.requests - self.ok) as f64 / self.requests as f64 * 1000.0).round() / 1000.0 } else { 0.0 },
            "errors": self.errors,
            "ttft_ms_p50": self.ttft.quantile(&OBS_MS_BUCKETS, 0.5),
            "ttft_ms_p95": self.ttft.quantile(&OBS_MS_BUCKETS, 0.95),
            "ttft_ms_mean": round(self.ttft.mean()),
            "tokens_per_s_mean": round(self.tps.mean()),
            "tokens_per_s_p50": self.tps.quantile(&OBS_TPS_BUCKETS, 0.5),
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cost_usd": (self.cost * 10000.0).round() / 10000.0,
        })
    }
}

impl Sup {
    /// Per provider model over the last `hours`: totals, by route and node,
    /// and what was learned (context window, last time credits ran out).
    pub fn provider_stats(&self, hours: i64, provider: Option<&str>) -> Value {
        let since = (now_ms() / HOUR - hours.max(1) + 1) * HOUR;
        let rows = self.db.observations(since, provider);
        let mut by_model: BTreeMap<(String, String), (Agg, BTreeMap<(String, String), Agg>, Vec<String>)> = BTreeMap::new();
        for r in &rows {
            let e = by_model.entry((r.provider.clone(), r.model.clone())).or_default();
            e.0.add(r);
            e.1.entry((r.node.clone(), r.route.clone())).or_default().add(r);
            if !e.2.contains(&r.scope_id) {
                e.2.push(r.scope_id.clone());
            }
        }
        let events = self.db.provider_events(provider, None, 2000);
        // Models with events but no recent calls are listed too.
        for e in &events {
            by_model.entry((e.provider.clone(), e.model.clone())).or_default();
        }
        let out: Vec<Value> = by_model
            .into_iter()
            .map(|((p, m), (agg, routes, scopes))| {
                let mine: Vec<&ProviderEvent> = events.iter().filter(|e| e.provider == p && e.model == m).collect();
                let of = |k: &str| mine.iter().filter(|e| e.kind == k).copied().collect::<Vec<_>>();
                let learned_context = of("context_overflow").iter().filter_map(|e| e.detail["limit"].as_u64()).min();
                let last = |k: &str| of(k).iter().map(|e| e.ts).max();
                let mut v = agg.json();
                v["provider"] = json!(p);
                v["model"] = json!(m);
                v["scopes"] = json!(scopes);
                v["by_route"] = Value::Array(
                    routes
                        .into_iter()
                        .map(|((node, route), a)| {
                            let mut r = a.json();
                            r["node"] = json!(node);
                            r["route"] = json!(route);
                            r
                        })
                        .collect(),
                );
                v["learned"] = json!({
                    "context": learned_context,
                    "context_overflows": of("context_overflow").len(),
                    "last_context_overflow": last("context_overflow"),
                    "last_credits_exhausted": last("credits_exhausted"),
                    "credits_exhausted": of("credits_exhausted").len(),
                    "last_auth_failed": last("auth_failed"),
                });
                v
            })
            .collect();
        json!({"hours": hours, "since": since, "models": out})
    }

    /// One line per model for agents and the CLI: "groq/llama-3.3: p50 first
    /// byte 0.4 s, 180 tok/s, 2% errors (24 h)".
    pub fn provider_brief(&self, provider: &str, model: &str) -> Option<String> {
        let s = self.provider_stats(24, Some(provider));
        let m = s["models"].as_array()?.iter().find(|v| v["model"] == model)?.clone();
        if m["requests"].as_u64().unwrap_or(0) == 0 {
            return None;
        }
        let mut parts = vec![];
        if let Some(t) = m["ttft_ms_p50"].as_f64() {
            parts.push(format!("first byte ≤{:.1} s (p50)", t / 1000.0));
        }
        if let Some(t) = m["tokens_per_s_mean"].as_f64() {
            parts.push(format!("{t:.0} tok/s"));
        }
        parts.push(format!("{:.0}% errors over {} calls", m["error_rate"].as_f64().unwrap_or(0.0) * 100.0, m["requests"]));
        if let Some(c) = m["learned"]["context"].as_u64() {
            parts.push(format!("context window {c} (learned)"));
        }
        if let Some(ts) = m["learned"]["last_credits_exhausted"].as_i64() {
            parts.push(format!("out of credits {} min ago", (now_ms() - ts) / 60_000));
        }
        Some(parts.join(", "))
    }
}
