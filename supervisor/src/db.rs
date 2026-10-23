//! Supervisor state in SQLite (WAL). Domain objects are stored as JSON with a
//! few indexed columns; the event log and audit chain are append-only.

use anyhow::{anyhow, Result};
use ocl_protocol::model::*;
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
             CREATE TABLE IF NOT EXISTS allowed_sources(cidr TEXT PRIMARY KEY, note TEXT NOT NULL, added_by TEXT NOT NULL, added_at INTEGER NOT NULL);",
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
        let token = format!("ocl_{}", hex::encode(raw));
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
        assert_eq!(db.token_lookup("ocl_wrong"), None);
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
        };
        for (t, r, s) in [("b", 2.0, TaskStatus::Queued), ("done", 0.5, TaskStatus::Done), ("a", 1.0, TaskStatus::Queued), ("run", 9.0, TaskStatus::Running)] {
            db.insert_task(&mut mk(t, r, s)).unwrap();
        }
        let order: Vec<String> = db.tasks().into_iter().map(|t| t.title).collect();
        assert_eq!(order, vec!["run", "a", "b", "done"]);
    }
}
