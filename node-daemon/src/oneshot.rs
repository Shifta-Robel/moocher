//! One model call outside any instance, for the hub's manager. It goes through
//! the keyproxy like an instance's call, so keys stay on the node and routes,
//! allowances and provider statistics all apply. OpenAI-style providers take the
//! request as it is; Anthropic's is translated both ways.

use crate::gateway::{proxied, Gateway, GatewayDeps, WsRef};
use anyhow::{anyhow, bail, Context, Result};
use moocher_protocol::config::{ApiKind, SpawnOverrides};
use moocher_protocol::model::{SessionRole, TokenUsage};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MAX_TOKENS: u32 = 2000;

pub async fn chat(deps: &GatewayDeps, data_root: &Path, agent: &str, system: &str, messages: &[Value], tools: &[Value]) -> Result<Value> {
    let snap = deps.cfg.get();
    let loaded = snap.agents.get(agent).ok_or_else(|| anyhow!("agent {agent} is not configured on this node"))?;
    let provider = loaded.cfg.provider.id.clone();
    let model = loaded.cfg.provider.model.clone();
    let api = snap.providers.get(&provider).ok_or_else(|| anyhow!("provider {provider} is not configured on this node"))?.api;
    let cap: String = moocher_protocol::new_id("mgr");
    // A fresh gateway per call: the agent's per-session limits apply to each question's calls.
    let g = Gateway::new(
        &format!("manager-{agent}"),
        agent,
        SessionRole::Worker,
        &cap,
        Arc::new(Mutex::new(TokenUsage::default())),
        Arc::new(crate::usage::Metered::new(TokenUsage::default())),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Default::default(),
        Arc::new(Mutex::new(SpawnOverrides::default())),
        WsRef { root: data_root.to_path_buf(), runtime_path: String::new() },
        deps,
    );
    let (rest, headers, body): (&str, Vec<(&str, &str)>, Value) = match api {
        ApiKind::Openai => ("chat/completions", vec![], openai_request(&model, system, messages, tools)),
        ApiKind::Anthropic => ("messages", vec![("anthropic-version", "2023-06-01")], anthropic_request(&model, system, messages, tools)),
        _ => bail!("the manager needs an OpenAI-style or Anthropic provider; {provider} is neither"),
    };
    let (status, bytes) = proxied(&g, &provider, rest, &headers, serde_json::to_vec(&body)?).await?;
    let v: Value = serde_json::from_slice(&bytes).with_context(|| format!("provider answered HTTP {status} with something other than JSON"))?;
    if !(200..300).contains(&status) {
        let why = v["error"]["message"].as_str().or(v["error"].as_str()).unwrap_or("no details");
        bail!("{provider} answered HTTP {status}: {why}");
    }
    let (content, tool_calls) = match api {
        ApiKind::Anthropic => anthropic_reply(&v),
        _ => (v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string(), v["choices"][0]["message"]["tool_calls"].as_array().cloned().unwrap_or_default()),
    };
    let u = g.metered();
    Ok(json!({"content": content, "tool_calls": tool_calls, "usage": {"input": u.input, "output": u.output, "cost_usd": u.cost_usd}}))
}

fn openai_request(model: &str, system: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut all = vec![json!({"role": "system", "content": system})];
    all.extend_from_slice(messages);
    let mut b = json!({"model": model, "messages": all, "max_tokens": MAX_TOKENS, "stream": false});
    if !tools.is_empty() {
        b["tools"] = Value::Array(tools.to_vec());
    }
    b
}

fn anthropic_request(model: &str, system: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut out: Vec<Value> = vec![];
    for m in messages {
        match m["role"].as_str() {
            Some("assistant") => {
                let mut blocks = vec![];
                if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    let input: Value = serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                    blocks.push(json!({"type": "tool_use", "id": c["id"], "name": c["function"]["name"], "input": input}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Some("tool") => {
                // Results of one round go together in a single user message.
                let block = json!({"type": "tool_result", "tool_use_id": m["tool_call_id"], "content": m["content"]});
                match out.last_mut().filter(|l| l["role"] == "user" && l["content"].is_array()) {
                    Some(l) => l["content"].as_array_mut().unwrap().push(block),
                    None => out.push(json!({"role": "user", "content": [block]})),
                }
            }
            _ => out.push(json!({"role": "user", "content": m["content"]})),
        }
    }
    let tools: Vec<Value> = tools.iter().map(|t| json!({"name": t["function"]["name"], "description": t["function"]["description"], "input_schema": t["function"]["parameters"]})).collect();
    let mut b = json!({"model": model, "system": system, "messages": out, "max_tokens": MAX_TOKENS});
    if !tools.is_empty() {
        b["tools"] = Value::Array(tools);
    }
    b
}

fn anthropic_reply(v: &Value) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut calls = vec![];
    for b in v["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
            Some("tool_use") => calls.push(json!({"id": b["id"], "type": "function", "function": {"name": b["name"], "arguments": b["input"].to_string()}})),
            _ => {}
        }
    }
    (text, calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_round_trip() {
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "a", "type": "function", "function": {"name": "x", "arguments": "{\"n\":1}"}}, {"id": "b", "type": "function", "function": {"name": "y", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "one"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "two"}),
        ];
        let tools = vec![json!({"type": "function", "function": {"name": "x", "description": "d", "parameters": {"type": "object"}}})];
        let b = anthropic_request("m", "sys", &msgs, &tools);
        assert_eq!(b["system"], "sys");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "both results in one user message");
        assert_eq!(m[1]["content"][0]["input"]["n"], 1);
        assert_eq!(m[2]["content"][1]["tool_use_id"], "b");
        let (text, calls) = anthropic_reply(&json!({"content": [{"type": "text", "text": "ok"}, {"type": "tool_use", "id": "c", "name": "x", "input": {"n": 2}}]}));
        assert_eq!(text, "ok");
        assert_eq!(calls[0]["function"]["arguments"], "{\"n\":2}");
    }
}
