//! Housekeeping, hourly: export the event log by day, prune what has been
//! exported, and expire artifacts nothing refers to any more.
//!
//! * `exports/events-YYYYMMDD.jsonl.zst`: one JSON event per line, by UTC day.
//!   Export is incremental by event id: a run appends a zstd frame (a file
//!   may hold several; `zstd -d` and `zstdcat` read them as one stream), so
//!   events that arrive late, or a run triggered by hand, never go missing or
//!   twice.
//! * Events leave the database after `events.retain`, but only once exported
//!   (when export is on).
//! * Artifacts past `artifacts.retain` go, unless tagged with a `keep_tags`
//!   tag or referenced from anything still in use (see `Db::live_reference_text`)
//!   or from the config files (templates, agents, workflows).

use crate::state::{Actor, Sup};
use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use ocl_protocol::config::parse_duration_ms;
use ocl_protocol::model::{now_ms, parse_art_ref, Ts};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const EXPORTED_ID: &str = "events_exported_id";
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub as_of: Ts,
    pub dry_run: bool,
    pub events_exported: usize,
    pub export_files: Vec<String>,
    pub events_pruned: usize,
    pub export_files_removed: Vec<String>,
    pub artifacts_removed: Vec<String>,
    pub artifact_bytes_freed: u64,
    pub artifacts_kept_referenced: usize,
    pub artifacts_kept_tagged: usize,
}

pub fn day_of(ts: Ts) -> String {
    Utc.timestamp_millis_opt(ts).single().map(|d| d.format("%Y%m%d").to_string()).unwrap_or_else(|| "19700101".into())
}

/// All `art://b3/<hash>` references in `text` (hashes only).
pub fn refs_in(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut rest = text;
    while let Some(i) = rest.find("art://b3/") {
        let tail = &rest[i + 9..];
        let end = tail.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(tail.len());
        if let Some(h) = parse_art_ref(&tail[..end]) {
            out.insert(h);
        }
        rest = &tail[end..];
    }
    out
}

fn config_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !matches!(p.file_name().and_then(|n| n.to_str()), Some("supervisor" | "data" | "exports")) {
                stack.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "yaml" || x == "yml" || x == "json" || x == "md") {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    out.push_str(&t);
                    out.push('\n');
                }
            }
        }
    }
    out
}

impl Sup {
    fn export_dir(&self) -> PathBuf {
        self.data_dir.join("exports")
    }

    /// One housekeeping pass, as if it were `as_of` (tests and `ocl housekeeping --as-of`).
    pub fn housekeeping(&self, as_of: Ts, dry_run: bool, actor: &Actor) -> Result<Report> {
        let mut r = Report { as_of, dry_run, ..Default::default() };
        let ev = &self.cfg.events;
        if ev.export {
            self.export_events(as_of, dry_run, &mut r)?;
        }
        if let Some(keep) = parse_duration_ms(&ev.retain) {
            let exported = ev.export.then(|| self.db.meta(EXPORTED_ID).and_then(|v| v.parse().ok()).unwrap_or(0));
            if !dry_run {
                r.events_pruned = self.db.prune_events(as_of - keep, exported)?;
            }
        }
        if let Some(keep) = ev.export_keep.as_deref().and_then(parse_duration_ms) {
            let cutoff = day_of(as_of - keep);
            for e in std::fs::read_dir(self.export_dir()).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(day) = name.strip_prefix("events-").and_then(|n| n.strip_suffix(".jsonl.zst")) {
                    if day < cutoff.as_str() {
                        if !dry_run {
                            let _ = std::fs::remove_file(e.path());
                        }
                        r.export_files_removed.push(name);
                    }
                }
            }
        }
        if let Some(keep) = self.cfg.artifacts.retain.as_deref().and_then(parse_duration_ms) {
            self.gc_artifacts(as_of - keep, dry_run, &mut r)?;
        }
        let changed = r.events_exported + r.events_pruned + r.artifacts_removed.len() + r.export_files_removed.len() > 0;
        if changed && !dry_run {
            self.audit(
                actor,
                "housekeeping",
                "-",
                json!({"events_exported": r.events_exported, "events_pruned": r.events_pruned, "artifacts_removed": r.artifacts_removed.len(), "bytes_freed": r.artifact_bytes_freed}),
                true,
            );
        }
        Ok(r)
    }

    fn export_events(&self, as_of: Ts, dry_run: bool, r: &mut Report) -> Result<()> {
        let dir = self.export_dir();
        let mut after: i64 = self.db.meta(EXPORTED_ID).and_then(|v| v.parse().ok()).unwrap_or(0);
        // Whole days only: today's events go out tomorrow.
        let today_start = as_of - as_of.rem_euclid(DAY_MS);
        loop {
            let batch = self.db.events_for_export(after, today_start, 5000);
            if batch.is_empty() {
                break;
            }
            let mut by_day: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for e in &batch {
                let line = by_day.entry(day_of(e.ts)).or_default();
                serde_json::to_writer(&mut *line, e)?;
                line.push(b'\n');
            }
            if !dry_run {
                std::fs::create_dir_all(&dir)?;
                for (day, lines) in &by_day {
                    let p = dir.join(format!("events-{day}.jsonl.zst"));
                    let frame = zstd::encode_all(lines.as_slice(), 9)?;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).with_context(|| format!("opening {}", p.display()))?;
                    f.write_all(&frame)?;
                    f.sync_all()?;
                }
            }
            for day in by_day.keys() {
                let n = format!("events-{day}.jsonl.zst");
                if !r.export_files.contains(&n) {
                    r.export_files.push(n);
                }
            }
            r.events_exported += batch.len();
            after = batch.last().map(|e| e.id).unwrap_or(after);
            if !dry_run {
                self.db.set_meta(EXPORTED_ID, &after.to_string())?;
            }
        }
        Ok(())
    }

    fn gc_artifacts(&self, cutoff: Ts, dry_run: bool, r: &mut Report) -> Result<()> {
        let mut referenced = refs_in(&self.db.live_reference_text());
        referenced.extend(refs_in(&config_text(&self.config_dir)));
        let keep_tags = &self.cfg.artifacts.keep_tags;
        for a in self.db.all_artifacts() {
            if a.created_at >= cutoff {
                continue;
            }
            if a.tags.iter().any(|t| keep_tags.contains(t)) {
                r.artifacts_kept_tagged += 1;
                continue;
            }
            if referenced.contains(&a.hash) {
                r.artifacts_kept_referenced += 1;
                continue;
            }
            if !dry_run {
                self.db.delete_artifact(&a.hash)?;
                let _ = std::fs::remove_file(self.blobs.path(&a.hash));
                self.log("artifact.expired", a.node.as_deref(), a.producer.as_deref(), json!({"ref": a.reference, "name": a.name, "size": a.size}));
            }
            r.artifact_bytes_freed += a.size;
            r.artifacts_removed.push(a.reference);
        }
        Ok(())
    }

    /// Hourly, starting a minute after start.
    pub async fn housekeeping_loop(self: std::sync::Arc<Self>) {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            iv.tick().await;
            let me = self.clone();
            match tokio::task::spawn_blocking(move || me.housekeeping(now_ms(), false, &Actor::System)).await {
                Ok(Ok(r)) => tracing::debug!(exported = r.events_exported, pruned = r.events_pruned, artifacts = r.artifacts_removed.len(), "housekeeping"),
                Ok(Err(e)) => tracing::warn!(error = %format!("{e:#}"), "housekeeping failed"),
                Err(e) => tracing::warn!(error = %e, "housekeeping panicked"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_refs() {
        assert_eq!(day_of(0), "19700101");
        assert_eq!(day_of(1_761_264_000_000 + 3_600_000), "20251024");
        let h = "a".repeat(64);
        let t = format!("seed: art://b3/{h}\nx art://b3/zz art://b3/{}", "b".repeat(64));
        let r = refs_in(&t);
        assert!(r.contains(&h) && r.contains(&"b".repeat(64)) && r.len() == 2, "{r:?}");
    }
}
