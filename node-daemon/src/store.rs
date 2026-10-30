//! Node-local durable state: instances (for reconcile after restart) and the
//! telemetry outbox (survives supervisor downtime).

use anyhow::Result;
use moocher_protocol::link::NodeEvent;
use moocher_protocol::model::SessionReport;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub id: String,
    pub report: SessionReport,
    pub cap: String,
    pub password: String,
    pub role: String,
    pub last_seq: i64,
    pub spec_json: String,
}

/// Outbox rows beyond this are dropped oldest-first (Activity events are the bulk).
const OUTBOX_MAX: i64 = 50_000;

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS instances(
               id TEXT PRIMARY KEY, report TEXT NOT NULL, cap TEXT NOT NULL, password TEXT NOT NULL,
               role TEXT NOT NULL DEFAULT 'worker', last_seq INTEGER NOT NULL DEFAULT 0, spec TEXT NOT NULL DEFAULT '{}');
             CREATE TABLE IF NOT EXISTS outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS quota(provider TEXT NOT NULL, scope_id TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL,
               PRIMARY KEY(provider, scope_id, kind));
             CREATE TABLE IF NOT EXISTS unit_pools(provider TEXT NOT NULL, scope_id TEXT NOT NULL, unit TEXT NOT NULL, model TEXT NOT NULL, body TEXT NOT NULL,
                 PRIMARY KEY(provider, scope_id, unit, model));
             CREATE TABLE IF NOT EXISTS ws_base(label TEXT PRIMARY KEY, manifest TEXT NOT NULL, reference TEXT NOT NULL, digest TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS reap(id TEXT PRIMARY KEY, at INTEGER NOT NULL, paths TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS obs(hour INTEGER NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL, scope_id TEXT NOT NULL, route TEXT NOT NULL,
               body TEXT NOT NULL, PRIMARY KEY(hour, provider, model, scope_id, route));
             CREATE TABLE IF NOT EXISTS learned_context(provider TEXT NOT NULL, model TEXT NOT NULL, tokens INTEGER NOT NULL, PRIMARY KEY(provider, model));",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Store { conn: Mutex::new(conn) })
    }

    /// Observation rows of hours starting at or after `since` (older ones are dropped).
    pub fn obs_rows(&self, since: i64) -> Vec<moocher_protocol::model::ObsRow> {
        let c = self.conn.lock().unwrap();
        let _ = c.execute("DELETE FROM obs WHERE hour < ?1", params![since]);
        let Ok(mut st) = c.prepare("SELECT body FROM obs WHERE hour >= ?1") else { return vec![] };
        st.query_map(params![since], |r| r.get::<_, String>(0)).map(|rows| rows.filter_map(|r| r.ok()).filter_map(|b| serde_json::from_str(&b).ok()).collect()).unwrap_or_default()
    }

    pub fn put_obs_row(&self, r: &moocher_protocol::model::ObsRow) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO obs(hour,provider,model,scope_id,route,body) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(hour,provider,model,scope_id,route) DO UPDATE SET body=excluded.body",
            params![r.hour, r.provider, r.model, r.scope_id, r.route, serde_json::to_string(r)?],
        )?;
        Ok(())
    }

    pub fn learned_contexts(&self) -> Vec<((String, String), u64)> {
        let c = self.conn.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT provider, model, tokens FROM learned_context") else { return vec![] };
        st.query_map([], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, String>(1)?), r.get::<_, i64>(2)? as u64)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn put_learned_context(&self, provider: &str, model: &str, tokens: u64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO learned_context(provider,model,tokens) VALUES(?1,?2,?3) ON CONFLICT(provider,model) DO UPDATE SET tokens=excluded.tokens",
            params![provider, model, tokens as i64],
        )?;
        Ok(())
    }

    pub fn put_instance(&self, row: &InstanceRow) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO instances(id,report,cap,password,role,last_seq,spec) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET report=excluded.report, last_seq=excluded.last_seq",
            params![row.id, serde_json::to_string(&row.report)?, row.cap, row.password, row.role, row.last_seq, row.spec_json],
        )?;
        Ok(())
    }

    pub fn update_report(&self, report: &SessionReport, last_seq: i64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE instances SET report=?2, last_seq=?3 WHERE id=?1",
            params![report.id, serde_json::to_string(report)?, last_seq],
        )?;
        Ok(())
    }

    pub fn instances(&self) -> Result<Vec<InstanceRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT id,report,cap,password,role,last_seq,spec FROM instances")?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, rep, cap, password, role, last_seq, spec_json)| {
                Some(InstanceRow { id, report: serde_json::from_str(&rep).ok()?, cap, password, role, last_seq, spec_json })
            })
            .collect();
        Ok(rows)
    }

    pub fn delete_instance(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM instances WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn quota_rows(&self) -> Result<Vec<(String, String, String, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT provider, scope_id, kind, body FROM quota")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.filter_map(|r| r.ok()).collect();
        Ok(rows)
    }

    pub fn put_quota_rows(&self, rows: &[(String, String, String, String)]) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for (p, s, k, b) in rows {
            tx.execute(
                "INSERT INTO quota(provider,scope_id,kind,body) VALUES(?1,?2,?3,?4) ON CONFLICT(provider,scope_id,kind) DO UPDATE SET body=excluded.body",
                params![p, s, k, b],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn unit_rows(&self) -> Result<Vec<(String, String, String, String, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT provider, scope_id, unit, model, body FROM unit_pools")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.filter_map(|r| r.ok()).collect();
        Ok(rows)
    }

    pub fn put_unit_rows(&self, rows: &[(String, String, String, String, String)]) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for (p, s, u, m, b) in rows {
            tx.execute(
                "INSERT INTO unit_pools(provider,scope_id,unit,model,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(provider,scope_id,unit,model) DO UPDATE SET body=excluded.body",
                params![p, s, u, m, b],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The last state of a workspace both this node and the hub agree on:
    /// (manifest, artifact reference, manifest digest).
    pub fn ws_base(&self, label: &str) -> Option<(moocher_protocol::archive::Manifest, String, String)> {
        let row: Option<(String, String, String)> = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT manifest, reference, digest FROM ws_base WHERE label=?1", params![label], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .ok()
            .flatten();
        row.and_then(|(m, r, d)| Some((serde_json::from_str(&m).ok()?, r, d)))
    }

    pub fn set_ws_base(&self, label: &str, m: &moocher_protocol::archive::Manifest, reference: &str, digest: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO ws_base(label,manifest,reference,digest) VALUES(?1,?2,?3,?4)
             ON CONFLICT(label) DO UPDATE SET manifest=excluded.manifest, reference=excluded.reference, digest=excluded.digest",
            params![label, serde_json::to_string(m)?, reference, digest],
        )?;
        Ok(())
    }

    pub fn drop_ws_base(&self, label: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM ws_base WHERE label=?1", params![label])?;
        Ok(())
    }

    /// Remove `paths` of instance `id` at `at` (retention of stopped instances).
    pub fn schedule_reap(&self, id: &str, at: i64, paths: &[String]) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO reap(id,at,paths) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET at=excluded.at, paths=excluded.paths",
            params![id, at, serde_json::to_string(paths)?],
        )?;
        Ok(())
    }

    pub fn due_reaps(&self, now: i64) -> Vec<(String, Vec<String>)> {
        let c = self.conn.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT id, paths FROM reap WHERE at <= ?1") else { return vec![] };
        st.query_map(params![now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|it| it.filter_map(|r| r.ok()).map(|(id, p)| (id, serde_json::from_str(&p).unwrap_or_default())).collect())
            .unwrap_or_default()
    }

    pub fn done_reap(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM reap WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn outbox_push(&self, ev: &NodeEvent) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO outbox(body) VALUES(?1)", params![serde_json::to_string(ev)?])?;
        let seq = c.last_insert_rowid();
        if seq % 1000 == 0 {
            c.execute("DELETE FROM outbox WHERE seq <= ?1", params![seq - OUTBOX_MAX])?;
        }
        Ok(seq as u64)
    }

    pub fn outbox_after(&self, after: u64, limit: usize) -> Result<Vec<(u64, NodeEvent)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT seq, body FROM outbox WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = st
            .query_map(params![after as i64, limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter_map(|(s, b)| Some((s as u64, serde_json::from_str(&b).ok()?)))
            .collect();
        Ok(rows)
    }

    pub fn outbox_ack(&self, upto: u64) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM outbox WHERE seq <= ?1", params![upto as i64])?;
        Ok(())
    }

    pub fn outbox_depth(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0)
    }
}
