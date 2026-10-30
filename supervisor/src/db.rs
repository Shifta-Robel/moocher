//! Supervisor state in SQLite (WAL). Domain objects are stored as JSON with a
//! few indexed columns; the event log and audit chain are append-only.

use anyhow::{anyhow, Result};
use moocher_protocol::model::*;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    c: Mutex<Connection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Operate,
    Admin,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        Some(match s {
            "read" => Scope::Read,
            "operate" => Scope::Operate,
            "admin" => Scope::Admin,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Operate => "operate",
            Scope::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: Ts,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: serde_json::Value,
    pub ok: bool,
    pub hash: String,
}

pub fn hash_token(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let c = Connection::open(path)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS nodes(name TEXT PRIMARY KEY, info TEXT NOT NULL, last_seq INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, node TEXT NOT NULL, state TEXT NOT NULL, info TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS sessions_node ON sessions(node);
             CREATE TABLE IF NOT EXISTS held(id INTEGER PRIMARY KEY AUTOINCREMENT, to_session TEXT NOT NULL, from_session TEXT NOT NULL, text TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS held_to ON held(to_session, id);
             CREATE TABLE IF NOT EXISTS inbox(id INTEGER PRIMARY KEY AUTOINCREMENT, to_session TEXT NOT NULL, from_session TEXT NOT NULL, text TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks(id INTEGER PRIMARY KEY AUTOINCREMENT, rank REAL NOT NULL, status TEXT NOT NULL, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY, status TEXT NOT NULL, info TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, kind TEXT NOT NULL, node TEXT, session TEXT, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_session ON events(session);
             CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
                target TEXT NOT NULL, detail TEXT NOT NULL, ok INTEGER NOT NULL, prev TEXT NOT NULL, hash TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS api_tokens(hash TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, scope TEXT NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS join_tokens(hash TEXT PRIMARY KEY, node TEXT NOT NULL, role TEXT NOT NULL, labels TEXT NOT NULL,
                created_by TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER);
             CREATE TABLE IF NOT EXISTS node_certs(fingerprint TEXT PRIMARY KEY, node TEXT NOT NULL, serial TEXT NOT NULL,
                issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS node_certs_node ON node_certs(node);
             CREATE TABLE IF NOT EXISTS node_auth(node TEXT PRIMARY KEY, status TEXT NOT NULL, role TEXT NOT NULL, labels TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS allowed_sources(cidr TEXT PRIMARY KEY, note TEXT NOT NULL, added_by TEXT NOT NULL, added_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS egress_ips(node TEXT NOT NULL, route TEXT NOT NULL, info TEXT NOT NULL, PRIMARY KEY(node, route));
             CREATE TABLE IF NOT EXISTS instance_stats(ts INTEGER NOT NULL, session TEXT NOT NULL, node TEXT NOT NULL, info TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS instance_stats_session ON instance_stats(session, ts);
             CREATE TABLE IF NOT EXISTS route_stats(node TEXT NOT NULL, route TEXT NOT NULL, provider TEXT NOT NULL, info TEXT NOT NULL, PRIMARY KEY(node, route, provider));
             CREATE TABLE IF NOT EXISTS observations(hour INTEGER NOT NULL, node TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
                scope_id TEXT NOT NULL, route TEXT NOT NULL, info TEXT NOT NULL, PRIMARY KEY(hour, node, provider, model, scope_id, route));
             CREATE INDEX IF NOT EXISTS observations_provider ON observations(provider, model, hour);
             CREATE TABLE IF NOT EXISTS queued_spawns(id TEXT PRIMARY KEY, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS provider_events(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, provider TEXT NOT NULL,
                model TEXT NOT NULL, kind TEXT NOT NULL, info TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS provider_events_provider ON provider_events(provider, model, kind, ts);
             CREATE TABLE IF NOT EXISTS quota_buckets(node TEXT NOT NULL, provider TEXT NOT NULL, scope_id TEXT NOT NULL, kind TEXT NOT NULL,
                info TEXT NOT NULL, PRIMARY KEY(node, provider, scope_id, kind));
             CREATE TABLE IF NOT EXISTS artifacts(hash TEXT PRIMARY KEY, name TEXT NOT NULL, task_id INTEGER, created_at INTEGER NOT NULL, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS questions(id TEXT PRIMARY KEY, status TEXT NOT NULL, asker TEXT NOT NULL, holder TEXT NOT NULL,
                request TEXT, updated_at INTEGER NOT NULL, info TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS questions_status ON questions(status);
             CREATE TABLE IF NOT EXISTS assumptions(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, task_id INTEGER, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS checkpoints(session TEXT PRIMARY KEY, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS harvests(id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL, ts INTEGER NOT NULL, info TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS harvests_session ON harvests(session);
             CREATE TABLE IF NOT EXISTS halts(id TEXT PRIMARY KEY, status TEXT NOT NULL, scope TEXT NOT NULL, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS notifications(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, read INTEGER NOT NULL DEFAULT 0, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS workspaces(name TEXT PRIMARY KEY, info TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS users(name TEXT PRIMARY KEY, pw_hash TEXT NOT NULL, scope TEXT NOT NULL, totp TEXT,
                totp_last INTEGER NOT NULL DEFAULT -1, lockouts INTEGER NOT NULL DEFAULT 0, locked_until INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS web_sessions(hash TEXT PRIMARY KEY, user TEXT NOT NULL, csrf TEXT NOT NULL, created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL, ip TEXT NOT NULL, agent TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS push_subscriptions(endpoint TEXT PRIMARY KEY, p256dh TEXT NOT NULL, auth TEXT NOT NULL, user TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_ok INTEGER, failures INTEGER NOT NULL DEFAULT 0);",
        )?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Db { c: Mutex::new(c) })
    }

    // ------------------------------------------------------------ tokens

    pub fn create_token(&self, name: &str, scope: Scope) -> Result<String> {
        use rand::Rng;
        let raw: [u8; 32] = rand::thread_rng().gen();
        let token = format!("moo_{}", hex::encode(raw));
        self.c.lock().unwrap().execute(
            "INSERT INTO api_tokens(hash,name,scope,created_at) VALUES(?1,?2,?3,?4)",
            params![hash_token(&token), name, scope.as_str(), now_ms()],
        )?;
        Ok(token)
    }

    pub fn token_lookup(&self, token: &str) -> Option<(String, Scope)> {
        self.c
            .lock()
            .unwrap()
            .query_row("SELECT name, scope FROM api_tokens WHERE hash=?1", params![hash_token(token)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .optional()
            .ok()
            .flatten()
            .and_then(|(n, s)| Some((n, Scope::parse(&s)?)))
    }

    pub fn tokens(&self) -> Result<Vec<(String, String, Ts)>> {
        let c = self.c.lock().unwrap();
        let mut st = c.prepare("SELECT name, scope, created_at FROM api_tokens ORDER BY created_at")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(|r| r.ok()).collect();
        Ok(v)
    }

    pub fn revoke_token(&self, name: &str) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM api_tokens WHERE name=?1", params![name])? > 0)
    }

    // -------------------------------------------------------- enrollment

    pub fn create_join_token(&self, node: &str, role: &str, labels: &serde_json::Value, by: &str, ttl_ms: i64) -> Result<(String, Ts)> {
        use rand::Rng;
        let raw: [u8; 16] = rand::thread_rng().gen();
        let secret = hex::encode(raw);
        let now = now_ms();
        let c = self.c.lock().unwrap();
        // One outstanding token per node name: a new invite replaces the old one.
        c.execute("DELETE FROM join_tokens WHERE node=?1 AND used_at IS NULL", params![node])?;
        c.execute(
            "INSERT INTO join_tokens(hash,node,role,labels,created_by,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![hash_token(&secret), node, role, labels.to_string(), by, now, now + ttl_ms],
        )?;
        Ok((secret, now + ttl_ms))
    }

    /// Atomically consume a join token. Returns (node, role, labels).
    pub fn use_join_token(&self, secret: &str) -> Result<(String, String, serde_json::Value)> {
        let c = self.c.lock().unwrap();
        let row: Option<(String, String, String, Ts, Option<Ts>)> = c
            .query_row("SELECT node, role, labels, expires_at, used_at FROM join_tokens WHERE hash=?1", params![hash_token(secret)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let Some((node, role, labels, expires, used)) = row else { return Err(anyhow!("unknown join token")) };
        if used.is_some() {
            return Err(anyhow!("join token already used"));
        }
        if expires < now_ms() {
            return Err(anyhow!("join token expired"));
        }
        let n = c.execute("UPDATE join_tokens SET used_at=?2 WHERE hash=?1 AND used_at IS NULL", params![hash_token(secret), now_ms()])?;
        if n != 1 {
            return Err(anyhow!("join token already used"));
        }
        Ok((node, role, serde_json::from_str(&labels).unwrap_or_default()))
    }

    pub fn add_node_cert(&self, node: &str, fingerprint: &str, serial: &str, expires_at: Ts) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO node_certs(fingerprint,node,serial,issued_at,expires_at) VALUES(?1,?2,?3,?4,?5)",
            params![fingerprint, node, serial, now_ms(), expires_at],
        )?;
        Ok(())
    }

    /// (node, revoked, expires_at) for a presented client certificate.
    pub fn node_cert(&self, fingerprint: &str) -> Option<(String, bool, Ts)> {
        self.c
            .lock()
            .unwrap()
            .query_row("SELECT node, revoked, expires_at FROM node_certs WHERE fingerprint=?1", params![fingerprint], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0, r.get::<_, i64>(2)?))
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Revoke a node's certificates, optionally keeping one (rotation keeps the newest).
    pub fn revoke_node_certs(&self, node: &str, except: Option<&str>) -> Result<usize> {
        Ok(self.c.lock().unwrap().execute(
            "UPDATE node_certs SET revoked=1 WHERE node=?1 AND revoked=0 AND fingerprint IS NOT ?2",
            params![node, except],
        )?)
    }

    pub fn set_node_auth(&self, node: &str, status: &str, role: &str, labels: &serde_json::Value) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO node_auth(node,status,role,labels,updated_at) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(node) DO UPDATE SET status=excluded.status, role=excluded.role, labels=excluded.labels, updated_at=excluded.updated_at",
            params![node, status, role, labels.to_string(), now_ms()],
        )?;
        Ok(())
    }

    /// (status, role, labels) of an enrolled node.
    pub fn node_auth(&self, node: &str) -> Option<(String, String, serde_json::Value)> {
        self.c
            .lock()
            .unwrap()
            .query_row("SELECT status, role, labels FROM node_auth WHERE node=?1", params![node], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .optional()
            .ok()
            .flatten()
            .map(|(s, r, l)| (s, r, serde_json::from_str(&l).unwrap_or_default()))
    }

    pub fn allowed_sources(&self) -> Vec<(String, String, String, Ts)> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT cidr, note, added_by, added_at FROM allowed_sources ORDER BY added_at") else { return vec![] };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    pub fn add_allowed_source(&self, cidr: &str, note: &str, by: &str) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute(
            "INSERT OR IGNORE INTO allowed_sources(cidr,note,added_by,added_at) VALUES(?1,?2,?3,?4)",
            params![cidr, note, by, now_ms()],
        )? > 0)
    }

    pub fn remove_allowed_source(&self, cidr: &str) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM allowed_sources WHERE cidr=?1", params![cidr])? > 0)
    }

    pub fn meta(&self, k: &str) -> Option<String> {
        self.c.lock().unwrap().query_row("SELECT v FROM meta WHERE k=?1", params![k], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.c.lock().unwrap().execute("INSERT INTO meta(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", params![k, v])?;
        Ok(())
    }

    // ----------------------------------------------------- egress & quota

    /// Replace a node's egress IPs (full snapshot).
    pub fn put_egress(&self, node: &str, ips: &[EgressIp]) -> Result<()> {
        let mut c = self.c.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM egress_ips WHERE node=?1", params![node])?;
        for e in ips {
            tx.execute("INSERT INTO egress_ips(node,route,info) VALUES(?1,?2,?3)", params![node, e.route, serde_json::to_string(e)?])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn egress(&self) -> Vec<EgressIp> {
        self.json_rows("SELECT info FROM egress_ips ORDER BY node, route", [])
    }

    pub fn add_instance_stats(&self, s: &InstanceStats) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO instance_stats(ts,session,node,info) VALUES(?1,?2,?3,?4)",
            params![s.ts, s.session, s.node, serde_json::to_string(s)?],
        )?;
        Ok(())
    }

    pub fn instance_stats(&self, session: &str, since: Ts) -> Vec<InstanceStats> {
        self.json_rows("SELECT info FROM instance_stats WHERE session=?1 AND ts>=?2 ORDER BY ts", params![session, since])
    }

    pub fn prune_instance_stats(&self, before: Ts) -> Result<usize> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM instance_stats WHERE ts < ?1", params![before])?)
    }

    pub fn put_route_stats(&self, node: &str, stats: &[RouteStat]) -> Result<()> {
        let mut c = self.c.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM route_stats WHERE node=?1", params![node])?;
        for s in stats {
            tx.execute("INSERT INTO route_stats(node,route,provider,info) VALUES(?1,?2,?3,?4)", params![node, s.route, s.provider, serde_json::to_string(s)?])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Hourly rows replace earlier versions of the same hour.
    pub fn put_observations(&self, rows: &[ObsRow]) -> Result<()> {
        let mut c = self.c.lock().unwrap();
        let tx = c.transaction()?;
        for r in rows {
            tx.execute(
                "INSERT INTO observations(hour,node,provider,model,scope_id,route,info) VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(hour,node,provider,model,scope_id,route) DO UPDATE SET info=excluded.info",
                params![r.hour, r.node, r.provider, r.model, r.scope_id, r.route, serde_json::to_string(r)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn observations(&self, since: Ts, provider: Option<&str>) -> Vec<ObsRow> {
        match provider {
            Some(p) => self.json_rows("SELECT info FROM observations WHERE hour >= ?1 AND provider = ?2 ORDER BY hour", params![since, p]),
            None => self.json_rows("SELECT info FROM observations WHERE hour >= ?1 ORDER BY hour", params![since]),
        }
    }

    pub fn add_provider_events(&self, events: &[ProviderEvent]) -> Result<()> {
        let mut c = self.c.lock().unwrap();
        let tx = c.transaction()?;
        for e in events {
            tx.execute("INSERT INTO provider_events(ts,provider,model,kind,info) VALUES(?1,?2,?3,?4,?5)", params![e.ts, e.provider, e.model, e.kind, serde_json::to_string(e)?])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn put_queued_spawn(&self, id: &str, info: &serde_json::Value) -> Result<()> {
        self.c.lock().unwrap().execute("INSERT INTO queued_spawns(id,info) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET info=excluded.info", params![id, info.to_string()])?;
        Ok(())
    }

    pub fn queued_spawns(&self) -> Vec<serde_json::Value> {
        self.json_rows("SELECT info FROM queued_spawns", [])
    }

    pub fn del_queued_spawn(&self, id: &str) -> Result<()> {
        self.c.lock().unwrap().execute("DELETE FROM queued_spawns WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Newest first.
    pub fn provider_events(&self, provider: Option<&str>, kind: Option<&str>, limit: i64) -> Vec<ProviderEvent> {
        self.json_rows(
            "SELECT info FROM provider_events WHERE (?1 IS NULL OR provider = ?1) AND (?2 IS NULL OR kind = ?2) ORDER BY ts DESC, id DESC LIMIT ?3",
            params![provider, kind, limit],
        )
    }

    pub fn route_stats(&self) -> Vec<RouteStat> {
        self.json_rows("SELECT info FROM route_stats ORDER BY node, route, provider", [])
    }

    /// Replace a node's quota buckets (full snapshot).
    pub fn put_quotas(&self, node: &str, buckets: &[QuotaBucket]) -> Result<()> {
        let mut c = self.c.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM quota_buckets WHERE node=?1", params![node])?;
        for b in buckets {
            tx.execute(
                "INSERT INTO quota_buckets(node,provider,scope_id,kind,info) VALUES(?1,?2,?3,?4,?5)",
                params![node, b.provider, b.scope_id, b.kind, serde_json::to_string(b)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn quotas(&self) -> Vec<QuotaBucket> {
        self.json_rows("SELECT info FROM quota_buckets ORDER BY provider, scope_id, kind, node", [])
    }

    // ------------------------------------------------------------- nodes

    pub fn put_node(&self, n: &NodeInfo) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO nodes(name, info) VALUES(?1, ?2) ON CONFLICT(name) DO UPDATE SET info=excluded.info",
            params![n.name, serde_json::to_string(n)?],
        )?;
        Ok(())
    }

    pub fn node(&self, name: &str) -> Option<NodeInfo> {
        let s: Option<String> = self.c.lock().unwrap().query_row("SELECT info FROM nodes WHERE name=?1", params![name], |r| r.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn nodes(&self) -> Vec<NodeInfo> {
        self.json_rows("SELECT info FROM nodes ORDER BY name", [])
    }

    pub fn delete_node(&self, name: &str) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM nodes WHERE name=?1", params![name])? > 0)
    }

    pub fn node_seq(&self, name: &str) -> u64 {
        self.c
            .lock()
            .unwrap()
            .query_row("SELECT last_seq FROM nodes WHERE name=?1", params![name], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as u64
    }

    pub fn set_node_seq(&self, name: &str, seq: u64) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE nodes SET last_seq=?2 WHERE name=?1", params![name, seq as i64])?;
        Ok(())
    }

    // ---------------------------------------------------------- sessions

    pub fn put_session(&self, s: &SessionInfo) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO sessions(id,node,state,info,updated_at) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(id) DO UPDATE SET node=excluded.node, state=excluded.state, info=excluded.info, updated_at=excluded.updated_at",
            params![s.report.id, s.report.node, s.report.state.as_str(), serde_json::to_string(s)?, s.report.updated_at],
        )?;
        Ok(())
    }

    pub fn session(&self, id: &str) -> Option<SessionInfo> {
        let s: Option<String> = self.c.lock().unwrap().query_row("SELECT info FROM sessions WHERE id=?1", params![id], |r| r.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Sessions, newest first. `all=false` hides stopped/lost sessions older than a day.
    pub fn sessions(&self, all: bool) -> Vec<SessionInfo> {
        if all {
            self.json_rows("SELECT info FROM sessions ORDER BY updated_at DESC", [])
        } else {
            let cutoff = now_ms() - 86_400_000;
            self.json_rows(
                "SELECT info FROM sessions WHERE state NOT IN ('stopped','lost') OR updated_at > ?1 ORDER BY updated_at DESC",
                params![cutoff],
            )
        }
    }

    pub fn sessions_on(&self, node: &str) -> Vec<SessionInfo> {
        self.json_rows("SELECT info FROM sessions WHERE node=?1", params![node])
    }

    // -------------------------------------------------------------- held

    pub fn held_push(&self, to: &str, from: &str, text: &str) -> Result<()> {
        self.c.lock().unwrap().execute("INSERT INTO held(to_session,from_session,text,ts) VALUES(?1,?2,?3,?4)", params![to, from, text, now_ms()])?;
        Ok(())
    }

    pub fn held(&self, to: &str) -> Result<Vec<crate::held::Held>> {
        let c = self.c.lock().unwrap();
        let mut st = c.prepare("SELECT id, from_session, text, ts FROM held WHERE to_session=?1 ORDER BY id")?;
        let rows = st.query_map(params![to], |r| Ok(crate::held::Held { id: r.get(0)?, from: r.get(1)?, text: r.get(2)?, ts: r.get(3)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn held_count(&self, to: &str) -> u32 {
        self.c.lock().unwrap().query_row("SELECT COUNT(*) FROM held WHERE to_session=?1", params![to], |r| r.get(0)).unwrap_or(0)
    }

    /// Forget what was delivered: those up to `up_to`, not any that came since.
    pub fn held_clear(&self, to: &str, up_to: i64) -> Result<()> {
        self.c.lock().unwrap().execute("DELETE FROM held WHERE to_session=?1 AND id<=?2", params![to, up_to])?;
        Ok(())
    }

    /// A replacement inherits what its predecessor had not received, in order.
    pub fn held_move(&self, from: &str, to: &str) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE held SET to_session=?2 WHERE to_session=?1", params![from, to])?;
        Ok(())
    }

    // ------------------------------------------------------------- inbox

    pub fn inbox_push(&self, to: &str, from: &str, text: &str) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO inbox(to_session,from_session,text,ts) VALUES(?1,?2,?3,?4)",
            params![to, from, text, now_ms()],
        )?;
        Ok(())
    }

    pub fn inbox_take(&self, to: &str) -> Result<Vec<serde_json::Value>> {
        let c = self.c.lock().unwrap();
        let mut st = c.prepare("SELECT from_session, text, ts FROM inbox WHERE to_session=?1 ORDER BY id")?;
        let v: Vec<serde_json::Value> = st
            .query_map(params![to], |r| {
                Ok(serde_json::json!({"from": r.get::<_, String>(0)?, "text": r.get::<_, String>(1)?, "ts": r.get::<_, i64>(2)?}))
            })?
            .filter_map(|r| r.ok())
            .collect();
        c.execute("DELETE FROM inbox WHERE to_session=?1", params![to])?;
        Ok(v)
    }

    // ------------------------------------------------------------- tasks

    pub fn insert_task(&self, t: &mut Task) -> Result<()> {
        let c = self.c.lock().unwrap();
        c.execute("INSERT INTO tasks(rank,status,info) VALUES(?1,?2,'{}')", params![t.rank, t.status.as_str()])?;
        t.id = c.last_insert_rowid();
        c.execute("UPDATE tasks SET info=?2 WHERE id=?1", params![t.id, serde_json::to_string(t)?])?;
        Ok(())
    }

    pub fn put_task(&self, t: &Task) -> Result<()> {
        self.c.lock().unwrap().execute(
            "UPDATE tasks SET rank=?2, status=?3, info=?4 WHERE id=?1",
            params![t.id, t.rank, t.status.as_str(), serde_json::to_string(t)?],
        )?;
        Ok(())
    }

    pub fn task(&self, id: i64) -> Option<Task> {
        let s: Option<String> = self.c.lock().unwrap().query_row("SELECT info FROM tasks WHERE id=?1", params![id], |r| r.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// All tasks in board order: running first, then queued by rank, then finished (newest first).
    pub fn tasks(&self) -> Vec<Task> {
        self.json_rows(
            "SELECT info FROM tasks ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END,
               CASE WHEN status IN ('running','queued') THEN rank ELSE -id END",
            [],
        )
    }

    pub fn delete_task(&self, id: i64) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM tasks WHERE id=?1", params![id])? > 0)
    }

    pub fn max_rank(&self) -> f64 {
        self.c.lock().unwrap().query_row("SELECT COALESCE(MAX(rank), 0) FROM tasks", [], |r| r.get(0)).unwrap_or(0.0)
    }

    // -------------------------------------------------------------- runs

    pub fn put_run(&self, r: &WorkflowRun) -> Result<()> {
        let status = serde_json::to_value(r.status)?.as_str().unwrap_or("running").to_string();
        self.c.lock().unwrap().execute(
            "INSERT INTO runs(id,status,info,updated_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, info=excluded.info, updated_at=excluded.updated_at",
            params![r.id, status, serde_json::to_string(r)?, r.updated_at],
        )?;
        Ok(())
    }

    pub fn run(&self, id: &str) -> Option<WorkflowRun> {
        let s: Option<String> = self.c.lock().unwrap().query_row("SELECT info FROM runs WHERE id=?1", params![id], |r| r.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn runs(&self) -> Vec<WorkflowRun> {
        self.json_rows("SELECT info FROM runs ORDER BY updated_at DESC LIMIT 200", [])
    }

    // ------------------------------------------------------------ events

    pub fn log(&self, kind: &str, node: Option<&str>, session: Option<&str>, data: &serde_json::Value) -> Result<LogEvent> {
        let ts = now_ms();
        let c = self.c.lock().unwrap();
        c.execute(
            "INSERT INTO events(ts,kind,node,session,data) VALUES(?1,?2,?3,?4,?5)",
            params![ts, kind, node, session, data.to_string()],
        )?;
        Ok(LogEvent { id: c.last_insert_rowid(), ts, kind: kind.into(), node: node.map(String::from), session: session.map(String::from), data: data.clone() })
    }

    pub fn events(&self, after: i64, session: Option<&str>, limit: i64) -> Vec<LogEvent> {
        let c = self.c.lock().unwrap();
        let map = |r: &rusqlite::Row| {
            Ok(LogEvent {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                node: r.get(3)?,
                session: r.get(4)?,
                data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            })
        };
        let rows = match session {
            Some(s) => c
                .prepare("SELECT id,ts,kind,node,session,data FROM events WHERE id>?1 AND session=?2 ORDER BY id LIMIT ?3")
                .and_then(|mut st| st.query_map(params![after, s, limit], map).map(|it| it.filter_map(|r| r.ok()).collect::<Vec<_>>())),
            None => c
                .prepare("SELECT id,ts,kind,node,session,data FROM events WHERE id>?1 ORDER BY id LIMIT ?2")
                .and_then(|mut st| st.query_map(params![after, limit], map).map(|it| it.filter_map(|r| r.ok()).collect::<Vec<_>>())),
        };
        rows.unwrap_or_default()
    }

    /// Events after `after_id` with `ts < before`, oldest first (for export).
    pub fn events_for_export(&self, after_id: i64, before: Ts, limit: i64) -> Vec<LogEvent> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT id,ts,kind,node,session,data FROM events WHERE id>?1 AND ts<?2 ORDER BY id LIMIT ?3") else { return vec![] };
        st.query_map(params![after_id, before, limit], |r| {
            Ok(LogEvent { id: r.get(0)?, ts: r.get(1)?, kind: r.get(2)?, node: r.get(3)?, session: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default() })
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Drop events older than `before` (and, when given, only up to `max_id`).
    pub fn prune_events(&self, before: Ts, max_id: Option<i64>) -> Result<usize> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM events WHERE ts<?1 AND id<=?2", params![before, max_id.unwrap_or(i64::MAX)])?)
    }

    pub fn all_artifacts(&self) -> Vec<ArtifactMeta> {
        self.json_rows("SELECT info FROM artifacts", [])
    }

    pub fn delete_artifact(&self, hash: &str) -> Result<()> {
        self.c.lock().unwrap().execute("DELETE FROM artifacts WHERE hash=?1", params![hash])?;
        Ok(())
    }

    /// Text of everything that may still point at an artifact (`art://b3/…`):
    /// shared workspaces, checkpoints, open halts, unfinished tasks and runs,
    /// sessions that are not over.
    pub fn live_reference_text(&self) -> String {
        let c = self.c.lock().unwrap();
        let mut out = String::new();
        for sql in [
            "SELECT info FROM workspaces",
            "SELECT info FROM checkpoints",
            "SELECT info FROM halts WHERE status='open'",
            "SELECT info FROM tasks WHERE status NOT IN ('done','failed','cancelled')",
            "SELECT info FROM runs WHERE status IN ('running','halted')",
            "SELECT info FROM sessions WHERE state NOT IN ('stopped','lost')",
        ] {
            if let Ok(mut st) = c.prepare(sql) {
                if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                    for r in rows.flatten() {
                        out.push_str(&r);
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    // ------------------------------------------------------------- audit

    /// Append to the hash chain: hash = sha256(prev || canonical entry).
    pub fn audit(&self, actor: &str, action: &str, target: &str, detail: &serde_json::Value, ok: bool) -> Result<()> {
        let c = self.c.lock().unwrap();
        let prev: String = c.query_row("SELECT hash FROM audit ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).optional()?.unwrap_or_else(|| "genesis".into());
        let ts = now_ms();
        let hash = audit_hash(&prev, ts, actor, action, target, &detail.to_string(), ok);
        c.execute(
            "INSERT INTO audit(ts,actor,action,target,detail,ok,prev,hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![ts, actor, action, target, detail.to_string(), ok as i64, prev, hash],
        )?;
        Ok(())
    }

    pub fn audit_list(&self, limit: i64) -> Vec<AuditEntry> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT id,ts,actor,action,target,detail,ok,hash FROM audit ORDER BY id DESC LIMIT ?1") else { return vec![] };
        st.query_map(params![limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                actor: r.get(2)?,
                action: r.get(3)?,
                target: r.get(4)?,
                detail: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                ok: r.get::<_, i64>(6)? != 0,
                hash: r.get(7)?,
            })
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Verify the whole chain. Returns the number of entries checked.
    pub fn audit_verify(&self) -> Result<usize> {
        let c = self.c.lock().unwrap();
        let mut st = c.prepare("SELECT id,ts,actor,action,target,detail,ok,prev,hash FROM audit ORDER BY id")?;
        let mut rows = st.query([])?;
        let mut prev = "genesis".to_string();
        let mut n = 0;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let p: String = r.get(7)?;
            let h: String = r.get(8)?;
            let expect = audit_hash(&prev, r.get(1)?, &r.get::<_, String>(2)?, &r.get::<_, String>(3)?, &r.get::<_, String>(4)?, &r.get::<_, String>(5)?, r.get::<_, i64>(6)? != 0);
            if p != prev || h != expect {
                return Err(anyhow!("audit chain broken at entry {id}"));
            }
            prev = h;
            n += 1;
        }
        Ok(n)
    }

    // --------------------------------------------------------- artifacts

    pub fn put_artifact(&self, a: &ArtifactMeta) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO artifacts(hash,name,task_id,created_at,info) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(hash) DO UPDATE SET info=excluded.info, name=excluded.name, task_id=excluded.task_id",
            params![a.hash, a.name, a.task_id, a.created_at, serde_json::to_string(a)?],
        )?;
        Ok(())
    }

    /// By full hash or a unique prefix.
    pub fn artifact(&self, hash: &str) -> Option<ArtifactMeta> {
        let rows: Vec<ArtifactMeta> = self.json_rows("SELECT info FROM artifacts WHERE hash LIKE ?1 || '%' LIMIT 2", params![hash]);
        if rows.len() == 1 {
            rows.into_iter().next()
        } else {
            rows.into_iter().find(|a| a.hash == hash)
        }
    }

    pub fn artifacts(&self, query: Option<&str>, task: Option<i64>, limit: i64) -> Vec<ArtifactMeta> {
        let q = format!("%{}%", query.unwrap_or(""));
        match task {
            Some(t) => self.json_rows("SELECT info FROM artifacts WHERE task_id=?1 AND (name LIKE ?2 OR info LIKE ?2) ORDER BY created_at DESC LIMIT ?3", params![t, q, limit]),
            None => self.json_rows("SELECT info FROM artifacts WHERE name LIKE ?1 OR info LIKE ?1 ORDER BY created_at DESC LIMIT ?2", params![q, limit]),
        }
    }

    // --------------------------------------------------------- questions

    pub fn put_question(&self, q: &Question) -> Result<()> {
        let status = serde_json::to_value(q.status)?.as_str().unwrap_or("open").to_string();
        self.c.lock().unwrap().execute(
            "INSERT INTO questions(id,status,asker,holder,request,updated_at,info) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, holder=excluded.holder, updated_at=excluded.updated_at, info=excluded.info",
            params![q.id, status, q.asker, q.holder, q.request, q.updated_at, serde_json::to_string(q)?],
        )?;
        Ok(())
    }

    pub fn question(&self, id: &str) -> Option<Question> {
        self.json_rows("SELECT info FROM questions WHERE id=?1", params![id]).into_iter().next()
    }

    pub fn question_by_request(&self, asker: &str, request: &str) -> Option<Question> {
        self.json_rows("SELECT info FROM questions WHERE asker=?1 AND request=?2", params![asker, request]).into_iter().next()
    }

    pub fn questions(&self, open_only: bool) -> Vec<Question> {
        if open_only {
            self.json_rows("SELECT info FROM questions WHERE status='open' ORDER BY updated_at", [])
        } else {
            self.json_rows("SELECT info FROM questions ORDER BY updated_at DESC LIMIT 500", [])
        }
    }

    pub fn add_assumption(&self, a: &Assumption) -> Result<i64> {
        let c = self.c.lock().unwrap();
        c.execute("INSERT INTO assumptions(ts,task_id,info) VALUES(?1,?2,'{}')", params![a.ts, a.task_id])?;
        let id = c.last_insert_rowid();
        let mut a = a.clone();
        a.id = id;
        c.execute("UPDATE assumptions SET info=?2 WHERE id=?1", params![id, serde_json::to_string(&a)?])?;
        Ok(id)
    }

    pub fn assumptions(&self, task: Option<i64>) -> Vec<Assumption> {
        match task {
            Some(t) => self.json_rows("SELECT info FROM assumptions WHERE task_id=?1 ORDER BY id", params![t]),
            None => self.json_rows("SELECT info FROM assumptions ORDER BY id DESC LIMIT 500", []),
        }
    }

    // --------------------------------------------- checkpoints & harvests

    pub fn put_checkpoint(&self, c: &Checkpoint) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO checkpoints(session,info) VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET info=excluded.info",
            params![c.session, serde_json::to_string(c)?],
        )?;
        Ok(())
    }

    pub fn checkpoint(&self, session: &str) -> Option<Checkpoint> {
        self.json_rows("SELECT info FROM checkpoints WHERE session=?1", params![session]).into_iter().next()
    }

    pub fn add_harvest(&self, h: &Harvest) -> Result<()> {
        self.c.lock().unwrap().execute("INSERT INTO harvests(session,ts,info) VALUES(?1,?2,?3)", params![h.session, h.at, serde_json::to_string(h)?])?;
        Ok(())
    }

    pub fn harvests(&self, session: Option<&str>) -> Vec<Harvest> {
        match session {
            Some(s) => self.json_rows("SELECT info FROM harvests WHERE session=?1 ORDER BY id", params![s]),
            None => self.json_rows("SELECT info FROM harvests ORDER BY id DESC LIMIT 200", []),
        }
    }

    // ------------------------------------------------- halts & notifications

    pub fn put_halt(&self, h: &Halt) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO halts(id,status,scope,info) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET status=excluded.status, info=excluded.info",
            params![h.id, h.status, h.scope, serde_json::to_string(h)?],
        )?;
        Ok(())
    }

    pub fn halt(&self, id: &str) -> Option<Halt> {
        self.json_rows("SELECT info FROM halts WHERE id=?1", params![id]).into_iter().next()
    }

    pub fn halts(&self, open_only: bool) -> Vec<Halt> {
        if open_only {
            self.json_rows("SELECT info FROM halts WHERE status='open' ORDER BY rowid", [])
        } else {
            self.json_rows("SELECT info FROM halts ORDER BY rowid DESC LIMIT 200", [])
        }
    }

    pub fn add_notification(&self, n: &Notification) -> Result<Notification> {
        let c = self.c.lock().unwrap();
        c.execute("INSERT INTO notifications(ts,info) VALUES(?1,'{}')", params![n.ts])?;
        let mut n = n.clone();
        n.id = c.last_insert_rowid();
        c.execute("UPDATE notifications SET info=?2 WHERE id=?1", params![n.id, serde_json::to_string(&n)?])?;
        Ok(n)
    }

    pub fn notifications(&self, unread_only: bool, limit: i64) -> Vec<Notification> {
        let rows: Vec<(Notification, bool)> = {
            let c = self.c.lock().unwrap();
            let Ok(mut st) = c.prepare("SELECT info, read FROM notifications ORDER BY id DESC LIMIT ?1") else { return vec![] };
            st.query_map(params![limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))
                .map(|it| it.filter_map(|r| r.ok()).filter_map(|(s, rd)| Some((serde_json::from_str(&s).ok()?, rd))).collect())
                .unwrap_or_default()
        };
        rows.into_iter()
            .map(|(mut n, rd)| {
                n.read = rd;
                n
            })
            .filter(|n| !unread_only || !n.read)
            .collect()
    }

    pub fn mark_notifications_read(&self, upto: i64) -> Result<usize> {
        Ok(self.c.lock().unwrap().execute("UPDATE notifications SET read=1 WHERE id<=?1 AND read=0", params![upto])?)
    }

    // -------------------------------------------------------- workspaces

    pub fn put_workspace(&self, name: &str, info: &serde_json::Value) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO workspaces(name,info) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET info=excluded.info",
            params![name, info.to_string()],
        )?;
        Ok(())
    }

    pub fn workspace(&self, name: &str) -> Option<serde_json::Value> {
        self.json_rows("SELECT info FROM workspaces WHERE name=?1", params![name]).into_iter().next()
    }

    // ------------------------------------------------------------- users

    pub fn put_user(&self, name: &str, pw_hash: &str, scope: Scope) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO users(name,pw_hash,scope,created_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(name) DO UPDATE SET pw_hash=excluded.pw_hash, scope=excluded.scope, lockouts=0, locked_until=0",
            params![name, pw_hash, scope.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn user(&self, name: &str) -> Option<crate::auth::UserRow> {
        self.c
            .lock()
            .unwrap()
            .query_row("SELECT name,pw_hash,scope,totp,totp_last,lockouts,locked_until FROM users WHERE name=?1", params![name], |r| {
                Ok(crate::auth::UserRow {
                    name: r.get(0)?,
                    pw_hash: r.get(1)?,
                    totp: r.get(3)?,
                    totp_last: r.get(4)?,
                    lockouts: r.get::<_, i64>(5)? as u32,
                    locked_until: r.get(6)?,
                })
            })
            .optional()
            .ok()
            .flatten()
    }

    /// (name, scope, totp enabled, locked until)
    pub fn users(&self) -> Vec<(String, String, bool, Ts)> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT name, scope, totp IS NOT NULL, locked_until FROM users ORDER BY name") else { return vec![] };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    pub fn delete_user(&self, name: &str) -> Result<bool> {
        self.delete_user_sessions(name)?;
        Ok(self.c.lock().unwrap().execute("DELETE FROM users WHERE name=?1", params![name])? > 0)
    }

    pub fn set_password(&self, name: &str, pw_hash: &str) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE users SET pw_hash=?2 WHERE name=?1", params![name, pw_hash])?;
        Ok(())
    }

    pub fn set_totp(&self, name: &str, secret: Option<&str>) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("UPDATE users SET totp=?2, totp_last=-1 WHERE name=?1", params![name, secret])? > 0)
    }

    pub fn set_totp_last(&self, name: &str, step: i64) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE users SET totp_last=?2 WHERE name=?1", params![name, step])?;
        Ok(())
    }

    pub fn lock_user(&self, name: &str, until: Ts) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE users SET locked_until=?2, lockouts=lockouts+1 WHERE name=?1", params![name, until])?;
        Ok(())
    }

    pub fn reset_lockouts(&self, name: &str) -> Result<()> {
        self.c.lock().unwrap().execute("UPDATE users SET lockouts=0, locked_until=0 WHERE name=?1", params![name])?;
        Ok(())
    }

    pub fn add_web_session(&self, hash: &str, user: &str, csrf: &str, expires_at: Ts, ip: &str, agent: &str) -> Result<()> {
        let c = self.c.lock().unwrap();
        c.execute("DELETE FROM web_sessions WHERE expires_at < ?1", params![now_ms()])?;
        c.execute(
            "INSERT INTO web_sessions(hash,user,csrf,created_at,expires_at,ip,agent) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![hash, user, csrf, now_ms(), expires_at, ip, agent],
        )?;
        Ok(())
    }

    pub fn web_session(&self, hash: &str) -> Option<crate::auth::WebSession> {
        self.c
            .lock()
            .unwrap()
            .query_row(
                "SELECT s.user, u.scope, s.csrf, s.expires_at FROM web_sessions s JOIN users u ON u.name = s.user WHERE s.hash=?1",
                params![hash],
                |r| {
                    Ok(crate::auth::WebSession {
                        user: r.get(0)?,
                        scope: Scope::parse(&r.get::<_, String>(1)?).unwrap_or(Scope::Read),
                        csrf: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn delete_web_session(&self, hash: &str) -> Result<()> {
        self.c.lock().unwrap().execute("DELETE FROM web_sessions WHERE hash=?1", params![hash])?;
        Ok(())
    }

    pub fn delete_user_sessions(&self, user: &str) -> Result<()> {
        self.c.lock().unwrap().execute("DELETE FROM web_sessions WHERE user=?1", params![user])?;
        Ok(())
    }

    // -------------------------------------------------- push subscriptions

    pub fn put_push_sub(&self, endpoint: &str, p256dh: &str, auth: &str, user: &str) -> Result<()> {
        self.c.lock().unwrap().execute(
            "INSERT INTO push_subscriptions(endpoint,p256dh,auth,user,created_at) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(endpoint) DO UPDATE SET p256dh=excluded.p256dh, auth=excluded.auth, user=excluded.user, failures=0",
            params![endpoint, p256dh, auth, user, now_ms()],
        )?;
        Ok(())
    }

    /// (endpoint, p256dh, auth, user)
    pub fn push_subs(&self) -> Vec<(String, String, String, String)> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT endpoint, p256dh, auth, user FROM push_subscriptions") else { return vec![] };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    pub fn delete_push_sub(&self, endpoint: &str) -> Result<bool> {
        Ok(self.c.lock().unwrap().execute("DELETE FROM push_subscriptions WHERE endpoint=?1", params![endpoint])? > 0)
    }

    pub fn push_result(&self, endpoint: &str, ok: bool) -> Result<()> {
        let c = self.c.lock().unwrap();
        if ok {
            c.execute("UPDATE push_subscriptions SET last_ok=?2, failures=0 WHERE endpoint=?1", params![endpoint, now_ms()])?;
        } else {
            c.execute("UPDATE push_subscriptions SET failures=failures+1 WHERE endpoint=?1", params![endpoint])?;
            c.execute("DELETE FROM push_subscriptions WHERE failures >= 10", [])?;
        }
        Ok(())
    }

    fn json_rows<T: serde::de::DeserializeOwned, P: rusqlite::Params>(&self, sql: &str, p: P) -> Vec<T> {
        let c = self.c.lock().unwrap();
        let Ok(mut st) = c.prepare(sql) else { return vec![] };
        st.query_map(p, |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(|r| r.ok()).filter_map(|s| serde_json::from_str(&s).ok()).collect())
            .unwrap_or_default()
    }
}

fn audit_hash(prev: &str, ts: i64, actor: &str, action: &str, target: &str, detail: &str, ok: bool) -> String {
    let mut h = Sha256::new();
    for part in [prev, &ts.to_string(), actor, action, target, detail, if ok { "1" } else { "0" }] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_chain_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        for i in 0..5 {
            db.audit("op", "spawn", &format!("s-{i}"), &serde_json::json!({"i": i}), true).unwrap();
        }
        assert_eq!(db.audit_verify().unwrap(), 5);
        db.c.lock().unwrap().execute("UPDATE audit SET target='evil' WHERE id=3", []).unwrap();
        assert!(db.audit_verify().is_err());
    }

    #[test]
    fn tokens_hash_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let t = db.create_token("cli", Scope::Operate).unwrap();
        assert_eq!(db.token_lookup(&t), Some(("cli".into(), Scope::Operate)));
        assert_eq!(db.token_lookup("moo_wrong"), None);
        let stored: String = db.c.lock().unwrap().query_row("SELECT hash FROM api_tokens", [], |r| r.get(0)).unwrap();
        assert_ne!(stored, t);
    }

    #[test]
    fn task_board_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let mk = |title: &str, rank: f64, status: TaskStatus| Task {
            id: 0,
            title: title.into(),
            body: String::new(),
            label: TaskLabel::WhenFree,
            rank,
            status,
            tags: vec![],
            coordinator_session: None,
            result: None,
            notes: vec![],
            created_at: 0,
            updated_at: 0,
            started_at: None,
            finished_at: None,
            question_mode: None,
        };
        for (t, r, s) in [("b", 2.0, TaskStatus::Queued), ("done", 0.5, TaskStatus::Done), ("a", 1.0, TaskStatus::Queued), ("run", 9.0, TaskStatus::Running)] {
            db.insert_task(&mut mk(t, r, s)).unwrap();
        }
        let order: Vec<String> = db.tasks().into_iter().map(|t| t.title).collect();
        assert_eq!(order, vec!["run", "a", "b", "done"]);
    }
}
