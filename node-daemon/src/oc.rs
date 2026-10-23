//! Minimal OpenCode server client over a Unix socket (relayed by ocl-bridge).
//!
//! Sessions run on OpenCode's v1 session API: in 1.19.4 the newer v2 runner
//! (`/api/session/*`) does not expose MCP tools yet, and agents need the
//! cluster MCP tools. Telemetry comes from the v1 instance bus (`/event`) and
//! is rebuilt from `GET /session/{id}/message` after any reconnect.
//!
//! Only the endpoints listed in [`ENDPOINTS`] are used; a unit test checks them
//! against the pinned OpenCode version's OpenAPI paths.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// (method, OpenAPI path) pairs this client depends on.
#[cfg_attr(not(test), allow(dead_code))]
pub const ENDPOINTS: &[(&str, &str)] = &[
    ("get", "/api/health"),
    ("get", "/api/model"),
    ("post", "/session"),
    ("post", "/session/{sessionID}/prompt_async"),
    ("post", "/session/{sessionID}/abort"),
    ("get", "/session/{sessionID}/message"),
    ("get", "/session/{sessionID}/todo"),
    ("get", "/session/status"),
    ("get", "/event"),
    ("get", "/permission"),
    ("post", "/permission/{requestID}/reply"),
];

#[derive(Clone)]
pub struct Client {
    sock: PathBuf,
    auth: String,
    /// Query suffix scoping v1 calls to the instance's workspace directory.
    dir: String,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_./~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Token usage of one assistant message (v1 `AssistantMessage.tokens/cost`).
pub fn message_usage(info: &Value) -> ocl_protocol::model::TokenUsage {
    let tk = &info["tokens"];
    ocl_protocol::model::TokenUsage {
        input: tk["input"].as_u64().unwrap_or(0),
        output: tk["output"].as_u64().unwrap_or(0),
        reasoning: tk["reasoning"].as_u64().unwrap_or(0),
        cache_read: tk["cache"]["read"].as_u64().unwrap_or(0),
        cache_write: tk["cache"]["write"].as_u64().unwrap_or(0),
        cost_usd: info["cost"].as_f64().unwrap_or(0.0),
    }
}

impl Client {
    pub fn new(sock: PathBuf, password: &str, directory: &str) -> Self {
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")));
        Client { sock, auth, dir: format!("directory={}", enc(directory)) }
    }

    fn scoped(&self, path: &str) -> String {
        format!("{path}{}{}", if path.contains('?') { "&" } else { "?" }, self.dir)
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, timeout: Duration) -> Result<Response<Incoming>> {
        let stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&self.sock))
            .await
            .map_err(|_| anyhow!("connect timeout"))?
            .with_context(|| format!("connecting {}", self.sock.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder().method(method).uri(path).header("host", "opencode").header("authorization", &self.auth);
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Full::new(Bytes::from(serde_json::to_vec(&v)?))
            }
            None => Full::new(Bytes::new()),
        };
        let resp = tokio::time::timeout(timeout, sender.send_request(req.body(body)?))
            .await
            .map_err(|_| anyhow!("{path}: timeout"))??;
        Ok(resp)
    }

    pub async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let resp = self.send(method, path, body, Duration::from_secs(30)).await?;
        let status = resp.status();
        let bytes = tokio::time::timeout(Duration::from_secs(30), resp.into_body().collect()).await.map_err(|_| anyhow!("{path}: body timeout"))??.to_bytes();
        if !status.is_success() {
            bail!("{path}: HTTP {status}: {}", String::from_utf8_lossy(&bytes).chars().take(300).collect::<String>());
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    pub async fn healthy(&self) -> bool {
        matches!(self.call(Method::GET, "/api/health", None).await, Ok(v) if v["healthy"] == true)
    }

    pub async fn wait_healthy(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.healthy().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("opencode did not become healthy within {timeout:?}")
    }

    /// OpenCode loads its model catalog lazily after reporting healthy; a
    /// prompt sent before our model is in it fails with ModelUnavailable.
    pub async fn wait_model(&self, provider: &str, model: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if let Ok(v) = self.call(Method::GET, "/api/model", None).await {
                let found = v["data"].as_array().is_some_and(|ms| ms.iter().any(|m| m["providerID"] == provider && m["id"] == model));
                if found {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("model {provider}/{model} never appeared in OpenCode's catalog (check the generated opencode.json)")
    }

    pub async fn create_session(&self, title: &str) -> Result<String> {
        let v = self.call(Method::POST, &self.scoped("/session"), Some(json!({"title": title}))).await?;
        v["id"].as_str().map(String::from).ok_or_else(|| anyhow!("no session id in {v}"))
    }

    /// Queue a user message; OpenCode runs it when the current turn (if any) ends.
    pub async fn prompt(&self, session: &str, text: &str, provider: &str, model: &str, agent: &str) -> Result<()> {
        self.call(
            Method::POST,
            &self.scoped(&format!("/session/{session}/prompt_async")),
            Some(json!({"parts": [{"type": "text", "text": text}], "model": {"providerID": provider, "modelID": model}, "agent": agent})),
        )
        .await?;
        Ok(())
    }

    pub async fn abort(&self, session: &str) -> Result<()> {
        self.call(Method::POST, &self.scoped(&format!("/session/{session}/abort")), None).await?;
        Ok(())
    }

    pub async fn busy(&self, session: &str) -> Result<bool> {
        let v = self.call(Method::GET, &self.scoped("/session/status"), None).await?;
        Ok(v.get(session).is_some_and(|s| s["type"].as_str() != Some("idle")))
    }

    pub async fn todos(&self, session: &str) -> Result<Vec<ocl_protocol::model::Todo>> {
        let v = self.call(Method::GET, &self.scoped(&format!("/session/{session}/todo")), None).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// Full message history (`[{info, parts}]`).
    pub async fn messages(&self, session: &str) -> Result<Vec<Value>> {
        let v = self.call(Method::GET, &self.scoped(&format!("/session/{session}/message")), None).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub fn events_path(&self) -> String {
        self.scoped("/event")
    }

    pub async fn pending_permissions(&self) -> Result<Vec<Value>> {
        let v = self.call(Method::GET, &self.scoped("/permission"), None).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub async fn reply_permission(&self, request: &str, reply: &str) -> Result<()> {
        self.call(Method::POST, &self.scoped(&format!("/permission/{request}/reply")), Some(json!({"reply": reply}))).await?;
        Ok(())
    }

    /// Stream SSE `data:` payloads from `path` into `tx` until the stream ends or errors.
    pub async fn sse(&self, path: &str, tx: &mpsc::Sender<Value>) -> Result<()> {
        let resp = self.send(Method::GET, path, None, Duration::from_secs(15)).await?;
        if !resp.status().is_success() {
            bail!("{path}: HTTP {}", resp.status());
        }
        let mut body = resp.into_body();
        let mut buf = String::new();
        // OpenCode sends heartbeats; treat 60s of silence as a dead stream.
        while let Ok(frame) = tokio::time::timeout(Duration::from_secs(60), body.frame()).await {
            let Some(frame) = frame else { return Ok(()) };
            let frame = frame?;
            let Some(data) = frame.data_ref() else { continue };
            buf.push_str(&String::from_utf8_lossy(data));
            while let Some(i) = buf.find("\n\n") {
                let block: String = buf.drain(..i + 2).collect();
                let payload: String = block
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.trim_start())
                    .collect::<Vec<_>>()
                    .join("\n");
                if payload.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                    if tx.send(v).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        bail!("{path}: stream idle timeout")
    }
}

#[cfg(test)]
mod tests {
    /// Every endpoint we depend on exists in the pinned OpenCode version's
    /// OpenAPI paths (trimmed from `GET /doc`). Re-generate the fixture when
    /// bumping the image's OpenCode version.
    #[test]
    fn endpoints_exist_in_pinned_opencode() {
        let f: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/opencode-1.19.4-paths.json")).unwrap();
        for (method, path) in super::ENDPOINTS {
            let methods = f["paths"][path].as_array().unwrap_or_else(|| panic!("{path} missing from OpenCode {}", f["opencode_version"]));
            assert!(methods.iter().any(|m| m == method), "{method} {path} missing");
        }
    }
}
