//! Outbound notifications (PHASE1_5 §11): rules, quiet hours, and the
//! channels beyond the in-app centre — Web Push, ntfy, a webhook.
//!
//! Web Push is implemented from the RFCs: messages are encrypted with
//! `aes128gcm` (RFC 8291: ECDH P-256 + HKDF + AES-128-GCM) and the request is
//! signed with VAPID (RFC 8292: an ES256 JWT). The VAPID key pair is created
//! on first start (`<data>/vapid.key`, 0600); browsers subscribe with its
//! public key.

use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ocl_protocol::config::{NotificationsConfig, NotifyRule};
use ocl_protocol::model::{now_ms, Notification};
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToSec1Point;
use serde_json::json;
use std::path::Path;

pub struct Notifier {
    vapid: p256::SecretKey,
    http: reqwest::Client,
}

fn sev_rank(s: &str) -> u8 {
    match s {
        "vital" => 2,
        "warning" => 1,
        _ => 0,
    }
}

impl Notifier {
    pub fn new(data_dir: &Path) -> Notifier {
        let path = data_dir.join("vapid.key");
        let key = std::fs::read(&path).ok().and_then(|b| p256::SecretKey::from_slice(&b).ok()).unwrap_or_else(|| {
            let k = <p256::SecretKey as p256::elliptic_curve::Generate>::generate();
            if std::fs::create_dir_all(data_dir).is_ok() && std::fs::write(&path, k.to_bytes()).is_ok() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            k
        });
        Notifier { vapid: key, http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default() }
    }

    /// The VAPID public key (base64url, uncompressed point) browsers subscribe with.
    pub fn vapid_public(&self) -> String {
        B64.encode(self.vapid.public_key().to_sec1_point(false).as_bytes())
    }

    /// Which external channels a notification goes to, given the rules and quiet hours.
    pub fn channels_for(cfg: &NotificationsConfig, n: &Notification, local_minutes: u32) -> Vec<String> {
        let all: Vec<String> = ["push", "ntfy", "webhook"].iter().map(|s| s.to_string()).collect();
        let default_rule = NotifyRule { kinds: vec![], min_severity: "warning".into(), channels: all };
        let rules: Vec<&NotifyRule> = if cfg.rules.is_empty() { vec![&default_rule] } else { cfg.rules.iter().collect() };
        let rule = rules.into_iter().find(|r| r.kinds.is_empty() || r.kinds.iter().any(|k| k == "*" || k == &n.kind || (k.ends_with(".*") && n.kind.starts_with(&k[..k.len() - 1]))));
        let Some(rule) = rule else { return vec![] };
        if sev_rank(&n.severity) < sev_rank(&rule.min_severity) {
            return vec![];
        }
        if n.severity != "vital" {
            if let Some(q) = &cfg.quiet_hours {
                if let (Some(a), Some(b)) = (hhmm(&q.from), hhmm(&q.to)) {
                    let quiet = if a <= b { local_minutes >= a && local_minutes < b } else { local_minutes >= a || local_minutes < b };
                    if quiet {
                        return vec![];
                    }
                }
            }
        }
        rule.channels.clone()
    }

    /// Send to the external channels (in the background; failures are logged).
    pub fn dispatch(&self, sup: &std::sync::Arc<crate::state::Sup>, n: &Notification) {
        let cfg = &sup.cfg.notifications;
        let local = {
            use chrono::Timelike;
            let t = chrono::Local::now();
            t.hour() * 60 + t.minute()
        };
        let channels = Self::channels_for(cfg, n, local);
        if channels.is_empty() {
            return;
        }
        let title = n.title.clone();
        let body = n.body.clone();
        let payload = json!({"title": n.title, "body": n.body, "kind": n.kind, "severity": n.severity, "target": n.target, "id": n.id, "ts": n.ts});
        if channels.iter().any(|c| c == "push") && cfg.web_push.is_some() {
            let subject = cfg.web_push.as_ref().and_then(|w| w.subject.clone()).unwrap_or_else(|| "mailto:operator@localhost".into());
            for (endpoint, p256dh, auth, _) in sup.db.push_subs() {
                let msg = serde_json::to_vec(&payload).unwrap_or_default();
                let urgent = n.severity == "vital";
                match self.push_request(&endpoint, &p256dh, &auth, &msg, &subject, urgent) {
                    Ok(req) => {
                        let ep = endpoint.clone();
                        let sup = sup.clone();
                        tokio::spawn(async move {
                            let db = &sup.db;
                            match req.send().await {
                                Ok(r) if r.status().is_success() => {
                                    let _ = db.push_result(&ep, true);
                                }
                                Ok(r) if r.status().as_u16() == 404 || r.status().as_u16() == 410 => {
                                    tracing::info!(endpoint = %ep, "push subscription gone; removed");
                                    let _ = db.delete_push_sub(&ep);
                                }
                                Ok(r) => {
                                    tracing::warn!(endpoint = %ep, status = %r.status(), "push refused");
                                    let _ = db.push_result(&ep, false);
                                }
                                Err(e) => {
                                    tracing::warn!(endpoint = %ep, error = %e, "push failed");
                                    let _ = db.push_result(&ep, false);
                                }
                            }
                        });
                    }
                    Err(e) => tracing::warn!(endpoint, error = %e, "bad push subscription"),
                }
            }
        }
        if let (true, Some(nt)) = (channels.iter().any(|c| c == "ntfy"), cfg.ntfy.clone()) {
            let mut req = self
                .http
                .post(&nt.url)
                .header("Title", title.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).collect::<String>())
                .header("Priority", match n.severity.as_str() { "vital" => "5", "warning" => "4", _ => "3" })
                .header("Tags", n.kind.replace('.', "_"))
                .body(body.clone());
            if let Some(t) = nt.token.as_deref().and_then(|r| resolve_ref(r).ok()) {
                req = req.bearer_auth(t);
            }
            tokio::spawn(async move {
                match req.send().await {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => tracing::warn!(status = %r.status(), "ntfy refused the notification"),
                    Err(e) => tracing::warn!(error = %e, "ntfy failed"),
                }
            });
        }
        if let (true, Some(wh)) = (channels.iter().any(|c| c == "webhook"), cfg.webhook.clone()) {
            let bytes = serde_json::to_vec(&payload).unwrap_or_default();
            let mut req = self.http.post(&wh.url).header("content-type", "application/json");
            if let Some(secret) = wh.secret.as_deref().and_then(|r| resolve_ref(r).ok()) {
                use hmac::{Hmac, Mac};
                let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac");
                m.update(&bytes);
                req = req.header("X-OCL-Signature", format!("sha256={}", hex::encode(m.finalize().into_bytes())));
            }
            let req = req.body(bytes);
            tokio::spawn(async move {
                match req.send().await {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => tracing::warn!(status = %r.status(), "webhook refused the notification"),
                    Err(e) => tracing::warn!(error = %e, "webhook failed"),
                }
            });
        }
    }

    /// A ready-to-send Web Push request (RFC 8291 body + RFC 8292 VAPID).
    fn push_request(&self, endpoint: &str, p256dh: &str, auth: &str, msg: &[u8], subject: &str, urgent: bool) -> Result<reqwest::RequestBuilder> {
        let ua_public = B64.decode(p256dh.trim_end_matches('=')).context("p256dh")?;
        let auth = B64.decode(auth.trim_end_matches('=')).context("auth")?;
        let eph = <p256::SecretKey as p256::elliptic_curve::Generate>::generate();
        let salt: [u8; 16] = rand::random();
        let body = encrypt_aes128gcm(msg, &ua_public, &auth, &eph, &salt)?;
        let url = reqwest::Url::parse(endpoint)?;
        let aud = format!("{}://{}", url.scheme(), url.host_str().ok_or_else(|| anyhow!("endpoint has no host"))?)
            + &url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let jwt = self.vapid_jwt(&aud, subject, now_ms() / 1000 + 12 * 3600);
        Ok(self
            .http
            .post(endpoint)
            .header("Authorization", format!("vapid t={jwt}, k={}", self.vapid_public()))
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "86400")
            .header("Urgency", if urgent { "high" } else { "normal" })
            .body(body))
    }

    /// RFC 8292 JWT, ES256 over P-256 (signature as raw r||s).
    pub fn vapid_jwt(&self, aud: &str, sub: &str, exp: i64) -> String {
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(serde_json::to_vec(&json!({"aud": aud, "exp": exp, "sub": sub})).unwrap_or_default());
        let input = format!("{header}.{claims}");
        let sk = p256::ecdsa::SigningKey::from(self.vapid.clone());
        let sig: p256::ecdsa::Signature = sk.sign(input.as_bytes());
        format!("{input}.{}", B64.encode(sig.to_bytes()))
    }
}

fn hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// `env:NAME` or `file:/path` (the supervisor's own channel credentials).
pub fn resolve_ref(r: &str) -> Result<String> {
    if let Some(n) = r.strip_prefix("env:") {
        return std::env::var(n).with_context(|| format!("environment variable {n} is not set"));
    }
    if let Some(p) = r.strip_prefix("file:") {
        return Ok(std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?.trim().to_string());
    }
    bail!("a secret reference must be env:NAME or file:/path")
}

fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    hk.expand(info, &mut out).expect("valid length");
    out
}

/// RFC 8291 §3–4: one aes128gcm record, no padding.
pub fn encrypt_aes128gcm(plain: &[u8], ua_public: &[u8], auth: &[u8], as_private: &p256::SecretKey, salt: &[u8; 16]) -> Result<Vec<u8>> {
    let ua = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|_| anyhow!("p256dh is not a P-256 public key"))?;
    let as_public = as_private.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(as_private.to_nonzero_scalar(), ua.as_affine());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public.as_bytes());
    let ikm = hkdf32(auth, shared.raw_secret_bytes().as_slice(), &key_info, 32);
    let cek = hkdf32(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf32(salt, &ikm, b"Content-Encoding: nonce\0", 12);
    let mut record = plain.to_vec();
    record.push(2); // last record, no padding
    let cipher = aes_gcm::Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow!("cek"))?;
    let ct = cipher.encrypt(aes_gcm::Nonce::from_slice(&nonce), record.as_slice()).map_err(|_| anyhow!("encryption failed"))?;
    let mut out = salt.to_vec();
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&ct);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8291 Appendix A.
    #[test]
    fn rfc8291_vector() {
        let d = |s: &str| B64.decode(s).unwrap();
        let plain = d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let as_private = p256::SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let ua_public = d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let out = encrypt_aes128gcm(&plain, &ua_public, &auth, &as_private, &salt).unwrap();
        assert_eq!(
            B64.encode(out),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_jwt_verifies() {
        use p256::ecdsa::signature::Verifier;
        let dir = tempfile::tempdir().unwrap();
        let n = Notifier::new(dir.path());
        assert_eq!(Notifier::new(dir.path()).vapid_public(), n.vapid_public(), "the key persists");
        let jwt = n.vapid_jwt("https://push.example.net", "mailto:a@b.c", 1_900_000_000);
        let parts: Vec<&str> = jwt.split('.').collect();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&B64.decode(n.vapid_public()).unwrap()).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net");
    }

    #[test]
    fn rules_and_quiet_hours() {
        let n = |kind: &str, sev: &str| Notification { id: 1, ts: 0, kind: kind.into(), severity: sev.into(), title: "t".into(), body: String::new(), target: None, read: false };
        let mut cfg = NotificationsConfig::default();
        assert!(Notifier::channels_for(&cfg, &n("x", "info"), 600).is_empty(), "default: info stays in-app");
        assert_eq!(Notifier::channels_for(&cfg, &n("x", "warning"), 600).len(), 3);
        cfg.quiet_hours = Some(ocl_protocol::config::QuietHours { from: "22:00".into(), to: "07:00".into() });
        assert!(Notifier::channels_for(&cfg, &n("x", "warning"), 23 * 60).is_empty(), "quiet at 23:00");
        assert!(Notifier::channels_for(&cfg, &n("x", "warning"), 6 * 60 + 59).is_empty(), "quiet at 06:59");
        assert_eq!(Notifier::channels_for(&cfg, &n("x", "warning"), 7 * 60).len(), 3);
        assert_eq!(Notifier::channels_for(&cfg, &n("task.halted", "vital"), 23 * 60).len(), 3, "vital ignores quiet hours");
        cfg.rules = vec![
            NotifyRule { kinds: vec!["question.*".into()], min_severity: "info".into(), channels: vec!["ntfy".into()] },
            NotifyRule { kinds: vec![], min_severity: "vital".into(), channels: vec!["push".into()] },
        ];
        assert_eq!(Notifier::channels_for(&cfg, &n("question.waiting", "info"), 600), vec!["ntfy"]);
        assert!(Notifier::channels_for(&cfg, &n("node.offline", "warning"), 600).is_empty());
        assert_eq!(Notifier::channels_for(&cfg, &n("node.offline", "vital"), 600), vec!["push"]);
    }
}
