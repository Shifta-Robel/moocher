//! Node-local durable state: instances (for reconcile after restart) and the
//! telemetry outbox (survives supervisor downtime).

use anyhow::Result;
use ocl_protocol::link::NodeEvent;
use ocl_protocol::model::SessionReport;
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
               PRIMARY KEY(provider, scope_id, kind));",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Store { conn: Mutex::new(conn) })
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
