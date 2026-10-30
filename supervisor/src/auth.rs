//! Operator login for the web UI (ARCHITECTURE D6, PHASE1_5 §13e).
//!
//! * Passwords are Argon2id hashes; TOTP (RFC 6238, 30 s, 6 digits) is optional
//!   per account and a code is accepted only once.
//! * A login creates a random session token kept only as its SHA-256 at the
//!   hub, sent as an `HttpOnly; SameSite=Strict` cookie (`Secure` over HTTPS),
//!   plus a CSRF token the UI echoes in `X-CSRF-Token` on every change.
//! * Failed logins count per account and per source IP. Too many within 15
//!   minutes lock that account (or IP) for `lockout_secs`, doubling with each
//!   further lockout up to 24 h; a lockout raises a vital notification.
//! * Every login, failure, lockout and logout is audited.
//!
//! API tokens (Bearer) keep working for the CLI and scripts.

use crate::db::{hash_token, Scope};
use crate::state::{Actor, Sup};
use anyhow::{anyhow, bail, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use moocher_protocol::model::{now_ms, Ts};
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

const WINDOW_MS: Ts = 15 * 60_000;

#[derive(Debug, Clone)]
pub struct UserRow {
    pub name: String,
    pub pw_hash: String,
    pub totp: Option<String>,
    /// Last TOTP time step accepted (replay protection).
    pub totp_last: i64,
    pub lockouts: u32,
    pub locked_until: Ts,
}

#[derive(Debug, Clone)]
pub struct WebSession {
    pub user: String,
    pub scope: Scope,
    pub csrf: String,
    pub expires_at: Ts,
}

/// Recent failures per account / per IP, and IP lockouts (in memory).
#[derive(Default)]
pub struct LoginGuard {
    fails: Mutex<HashMap<String, Vec<Ts>>>,
    ip_locked: Mutex<HashMap<IpAddr, (Ts, u32)>>,
}

#[derive(Debug)]
pub enum LoginError {
    /// Wrong user, password or code (deliberately not told apart).
    Invalid,
    /// A TOTP code is needed for this account.
    NeedTotp,
    Locked(Ts),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::Invalid => write!(f, "wrong user name, password or code"),
            LoginError::NeedTotp => write!(f, "enter the code from your authenticator app"),
            LoginError::Locked(until) => write!(f, "too many failed logins; try again in {} min", ((until - now_ms()) / 60_000).max(1)),
        }
    }
}

pub fn hash_password(pw: &str) -> Result<String> {
    if pw.chars().count() < 10 {
        bail!("use a password of at least 10 characters");
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow!("{e}"))?.to_string())
}

fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

pub fn random_hex(n: usize) -> String {
    use rand::Rng;
    let b: Vec<u8> = (0..n).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(b)
}

// ------------------------------------------------------------------ TOTP

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn unbase32(s: &str) -> Option<Vec<u8>> {
    let (mut buf, mut bits, mut out) = (0u32, 0u32, vec![]);
    for c in s.chars().filter(|c| *c != '=' && !c.is_whitespace()) {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// RFC 4226 HOTP with SHA-1, `digits` digits.
fn hotp(key: &[u8], counter: u64, digits: u32) -> u32 {
    use hmac::{Hmac, Mac};
    let mut m = Hmac::<sha1::Sha1>::new_from_slice(key).expect("any key length");
    m.update(&counter.to_be_bytes());
    let h = m.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let code = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    code % 10u32.pow(digits)
}

/// The accepted time step for `code` (now ±1 step), if any.
pub fn totp_step(secret_b32: &str, code: &str, now_s: i64) -> Option<i64> {
    let key = unbase32(secret_b32)?;
    let code: u32 = code.trim().parse().ok()?;
    let step = now_s / 30;
    (step - 1..=step + 1).find(|s| *s >= 0 && hotp(&key, *s as u64, 6) == code)
}

pub fn otpauth_uri(user: &str, secret: &str) -> String {
    format!("otpauth://totp/moocher:{user}?secret={secret}&issuer=moocher&digits=6&period=30")
}

// --------------------------------------------------------------- logins

impl Sup {
    /// Record a failure; returns the failures within the last 15 minutes.
    fn note_fail(&self, key: &str) -> usize {
        let now = now_ms();
        let mut f = self.login_guard.fails.lock().unwrap();
        let v = f.entry(key.to_string()).or_default();
        v.retain(|t| now - t < WINDOW_MS);
        v.push(now);
        v.len()
    }

    fn lockout_ms(&self, n: u32) -> Ts {
        ((self.cfg.login.lockout_secs as i64 * 1000) << n.min(10)).min(86_400_000)
    }

    /// Check credentials; returns (session token, csrf token, expiry).
    pub fn login(&self, user: &str, password: &str, totp: Option<&str>, ip: IpAddr, agent: &str) -> Result<(String, String, Ts), LoginError> {
        let now = now_ms();
        let max = self.cfg.login.max_failures.max(1) as usize;
        if let Some((until, _)) = self.login_guard.ip_locked.lock().unwrap().get(&ip).copied() {
            if until > now {
                return Err(LoginError::Locked(until));
            }
        }
        let row = self.db.user(user);
        if let Some(r) = &row {
            if r.locked_until > now {
                return Err(LoginError::Locked(r.locked_until));
            }
        }
        // Same work whether or not the account exists.
        let ok_pw = match &row {
            Some(r) => verify_password(password, &r.pw_hash),
            None => {
                let _ = verify_password(password, "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$5C1n+3lN4cXGmHn8sJd2rT8mR9gUs7jBC1m3nF3n5ng");
                false
            }
        };
        let mut need_totp = false;
        let ok = ok_pw
            && match row.as_ref().and_then(|r| r.totp.clone()) {
                None => true,
                Some(secret) => match totp.filter(|c| !c.trim().is_empty()) {
                    None => {
                        need_totp = true;
                        false
                    }
                    Some(code) => match totp_step(&secret, code, now / 1000) {
                        Some(step) if step > row.as_ref().map(|r| r.totp_last).unwrap_or(-1) => {
                            let _ = self.db.set_totp_last(user, step);
                            true
                        }
                        _ => false,
                    },
                },
            };
        if ok {
            self.login_guard.fails.lock().unwrap().remove(&format!("u:{user}"));
            let row = row.unwrap();
            let token = random_hex(32);
            let csrf = random_hex(16);
            let expires = now + self.cfg.login.session_hours as i64 * 3_600_000;
            if let Err(e) = self.db.add_web_session(&hash_token(&token), user, &csrf, expires, &ip.to_string(), agent) {
                tracing::error!(error = %e, "could not store web session");
                return Err(LoginError::Invalid);
            }
            let _ = self.db.reset_lockouts(user);
            self.audit(&Actor::Operator(row.name.clone()), "login", user, json!({"ip": ip.to_string(), "agent": truncate_ua(agent)}), true);
            return Ok((token, csrf, expires));
        }
        if need_totp {
            // Right password, code still to come: not a failure.
            return Err(LoginError::NeedTotp);
        }
        let ufails = self.note_fail(&format!("u:{user}"));
        let ifails = self.note_fail(&format!("ip:{ip}"));
        self.audit(&Actor::System, "login.failed", user, json!({"ip": ip.to_string(), "user_failures": ufails, "ip_failures": ifails}), false);
        if row.is_some() && ufails >= max {
            let n = row.as_ref().map(|r| r.lockouts).unwrap_or(0);
            let until = now + self.lockout_ms(n);
            let _ = self.db.lock_user(user, until);
            self.login_guard.fails.lock().unwrap().remove(&format!("u:{user}"));
            self.audit(&Actor::System, "login.lockout", user, json!({"ip": ip.to_string(), "until": until}), true);
            self.notify("login.lockout", "vital", format!("login locked for {user}"), format!("{ufails} failed logins in 15 min (last from {ip}); locked for {} min", (until - now) / 60_000), Some(format!("user:{user}")));
        }
        if ifails >= max * 4 {
            let n = self.login_guard.ip_locked.lock().unwrap().get(&ip).map(|x| x.1).unwrap_or(0);
            let until = now + self.lockout_ms(n);
            self.login_guard.ip_locked.lock().unwrap().insert(ip, (until, n + 1));
            self.login_guard.fails.lock().unwrap().remove(&format!("ip:{ip}"));
            self.audit(&Actor::System, "login.lockout", &format!("ip:{ip}"), json!({"until": until}), true);
            self.notify("login.lockout", "vital", format!("logins from {ip} blocked"), format!("{ifails} failed logins in 15 min; blocked for {} min", (until - now) / 60_000), Some(format!("ip:{ip}")));
        }
        Err(LoginError::Invalid)
    }

    pub fn web_session(&self, token: &str) -> Option<WebSession> {
        let s = self.db.web_session(&hash_token(token))?;
        (s.expires_at > now_ms()).then_some(s)
    }

    pub fn logout(&self, token: &str) {
        if let Some(s) = self.db.web_session(&hash_token(token)) {
            let _ = self.db.delete_web_session(&hash_token(token));
            self.audit(&Actor::Operator(s.user.clone()), "logout", &s.user, json!({}), true);
        }
    }

    /// Change a password (needs the current one); ends the user's other sessions.
    pub fn change_password(&self, user: &str, old: &str, new: &str) -> Result<()> {
        let row = self.db.user(user).ok_or_else(|| anyhow!("no such user"))?;
        if !verify_password(old, &row.pw_hash) {
            self.audit(&Actor::Operator(user.into()), "password.change", user, json!({}), false);
            bail!("the current password is wrong");
        }
        self.db.set_password(user, &hash_password(new)?)?;
        self.db.delete_user_sessions(user)?;
        self.audit(&Actor::Operator(user.into()), "password.change", user, json!({}), true);
        Ok(())
    }

    #[cfg(test)]
    pub fn reset_login_guard(&self) {
        *self.login_guard.fails.lock().unwrap() = HashMap::new();
    }
}

fn truncate_ua(s: &str) -> String {
    s.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_and_base32() {
        // RFC 6238 appendix B, SHA-1 key "12345678901234567890".
        let key = b"12345678901234567890";
        assert_eq!(hotp(key, 59 / 30, 8), 94287082);
        assert_eq!(hotp(key, 1111111109 / 30, 8), 7081804);
        assert_eq!(hotp(key, 20000000000 / 30, 8), 65353130);
        let b = base32(key);
        assert_eq!(b, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(unbase32(&b).unwrap(), key);
        assert_eq!(totp_step(&b, "287082", 59), Some(1));
        assert_eq!(totp_step(&b, "287082", 59 + 30), Some(1), "one step of clock skew");
        assert_eq!(totp_step(&b, "287082", 59 + 90), None);
    }

    #[test]
    fn login_lockout_and_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("t.db")).unwrap();
        db.put_user("admin", &hash_password("correct horse battery").unwrap(), Scope::Admin).unwrap();
        let mut cfg = moocher_protocol::config::SupervisorConfig::default();
        cfg.login.max_failures = 3;
        let sup = Sup::new(db, cfg, dir.path().into());
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let (tok, csrf, _) = sup.login("admin", "correct horse battery", None, ip, "t").unwrap();
        let s = sup.web_session(&tok).unwrap();
        assert_eq!((s.user.as_str(), s.csrf), ("admin", csrf));
        assert!(sup.web_session("forged").is_none());
        for _ in 0..3 {
            assert!(matches!(sup.login("admin", "nope", None, ip, "t"), Err(LoginError::Invalid)));
        }
        // Locked now, even with the right password.
        assert!(matches!(sup.login("admin", "correct horse battery", None, ip, "t"), Err(LoginError::Locked(_))));
        // Unknown users fail the same way.
        assert!(matches!(sup.login("ghost", "x", None, ip, "t"), Err(LoginError::Invalid)));
        sup.logout(&tok);
        assert!(sup.web_session(&tok).is_none());
        // TOTP: password alone asks for the code; a code works once.
        let secret = base32(b"12345678901234567890");
        sup.db.put_user("op", &hash_password("another long password").unwrap(), Scope::Operate).unwrap();
        sup.db.set_totp("op", Some(&secret)).unwrap();
        assert!(matches!(sup.login("op", "another long password", None, ip, "t"), Err(LoginError::NeedTotp)));
        let step = now_ms() / 1000 / 30;
        let code = format!("{:06}", hotp(&unbase32(&secret).unwrap(), step as u64, 6));
        assert!(sup.login("op", "another long password", Some(&code), ip, "t").is_ok());
        assert!(sup.login("op", "another long password", Some(&code), ip, "t").is_err(), "a code is accepted once");
        sup.reset_login_guard();
    }
}
