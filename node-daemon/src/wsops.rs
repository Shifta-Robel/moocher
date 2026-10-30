//! Workspace <-> artifact operations on this node: snapshots, three-way
//! merges of incoming snapshots, placing artifacts, and putting agent files.
//! All file access inside a workspace goes through `moocher_protocol::archive`'s
//! link-safe helpers (agents control what is in there).

use crate::cas::{Cas, PutInfo};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use moocher_protocol::archive::{self, Manifest};
use moocher_protocol::model::ArtifactMeta;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest tree an incoming archive may expand to.
const MAX_TREE: u64 = 20 << 30;

pub struct WsOps {
    pub cas: Arc<Cas>,
    store: Arc<Store>,
    data_root: PathBuf,
    /// Rootful Podman: files written into workspaces belong to the container user.
    chown_to: Option<u32>,
}

/// Snapshots carry the whole tree (links and special files are never included).
fn skip(_rel: &str) -> bool {
    false
}

impl WsOps {
    pub fn new(cas: Arc<Cas>, store: Arc<Store>, data_root: PathBuf, chown_to: Option<u32>) -> WsOps {
        WsOps { cas, store, data_root, chown_to }
    }

    /// `shared:<name>` / `isolated:<session>` -> its directory on this node.
    pub fn dir_of(&self, label: &str) -> Result<PathBuf> {
        let ws = self.data_root.join("workspaces");
        if let Some(n) = label.strip_prefix("shared:") {
            if !moocher_protocol::config::valid_name(n) {
                bail!("bad workspace name {n:?}");
            }
            return Ok(ws.join(format!("shared-{n}")));
        }
        if let Some(id) = label.strip_prefix("isolated:") {
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                bail!("bad session id {id:?}");
            }
            return Ok(ws.join(id));
        }
        bail!("workspace must be shared:<name> or isolated:<session>, not {label:?}")
    }

    fn canon(&self, label: &str, create: bool) -> Result<PathBuf> {
        let d = self.dir_of(label)?;
        if create {
            std::fs::create_dir_all(&d)?;
        }
        std::fs::canonicalize(&d).with_context(|| format!("workspace {label} does not exist on this node"))
    }

    fn fix_owner(&self, p: &Path) {
        if let Some(uid) = self.chown_to {
            let _ = crate::runtime::chown_r(p, uid);
        }
    }

    /// Pack a workspace and upload it (skipped when nothing changed since the
    /// last snapshot or merge).
    pub async fn snapshot(&self, label: &str, session: Option<&str>, reason: &str) -> Result<Value> {
        let root = self.canon(label, false)?;
        let (m, skipped) = {
            let r = root.clone();
            tokio::task::spawn_blocking(move || archive::manifest(&r, "", &skip)).await??
        };
        let digest = archive::manifest_digest(&m);
        if let Some((_, reference, d)) = self.store.ws_base(label) {
            if d == digest {
                return Ok(json!({"reference": reference, "files": m.len(), "unchanged": true}));
            }
        }
        let out = self.cas.tmp_path("snap");
        {
            let (r, m, o) = (root.clone(), m.clone(), out.clone());
            tokio::task::spawn_blocking(move || archive::pack(&r, &m, &o)).await??;
        }
        let parents: Vec<String> = self.store.ws_base(label).map(|(_, r, _)| r).into_iter().collect();
        let meta = self
            .cas
            .put(&out, PutInfo { name: format!("{label} ({reason})"), kind: "tree".into(), media_type: String::new(), session: session.map(String::from), parents, tags: vec!["workspace".into(), reason.into()] })
            .await?;
        self.store.set_ws_base(label, &m, &meta.reference, &digest)?;
        let bytes: u64 = m.values().map(|e| e.size).sum();
        Ok(json!({"reference": meta.reference, "files": m.len(), "bytes": bytes, "skipped_links": skipped, "unchanged": false}))
    }

    /// Merge a snapshot into this node's copy (created if missing).
    pub async fn apply(&self, label: &str, reference: &str) -> Result<Value> {
        let (blob, meta) = self.cas.get(reference).await?;
        if meta.kind != "tree" {
            bail!("{reference} is a file, not a workspace snapshot");
        }
        let root = self.canon(label, true)?;
        let tmp = self.cas.tmp_path("ws");
        let res = async {
            let incoming = {
                let (b, t) = (blob.clone(), tmp.clone());
                tokio::task::spawn_blocking(move || archive::unpack(&b, &t, MAX_TREE)).await??
            };
            let base: Manifest = self.store.ws_base(label).map(|(m, _, _)| m).unwrap_or_default();
            let (r, t, inc) = (root.clone(), tmp.clone(), incoming.clone());
            let report = tokio::task::spawn_blocking(move || -> Result<archive::MergeReport> {
                let (local, _) = archive::manifest(&r, "", &skip)?;
                archive::merge3(&r, &t, &base, &local, &inc)
            })
            .await??;
            self.store.set_ws_base(label, &incoming, &meta.reference, &archive::manifest_digest(&incoming))?;
            self.fix_owner(&root);
            Ok::<_, anyhow::Error>(report)
        }
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
        let report = res?;
        Ok(json!({
            "applied": report.added.len() + report.updated.len() + report.deleted.len(),
            "added": report.added, "updated": report.updated, "deleted": report.deleted, "conflicts": report.conflicts,
        }))
    }

    /// Put an artifact into a workspace: a file at `dest` (default: its name;
    /// a trailing `/` means "into this directory"), a tree under the directory `dest`.
    pub async fn place(&self, label: &str, reference: &str, dest: &str) -> Result<Value> {
        let (blob, meta) = self.cas.get(reference).await?;
        let root = self.canon(label, true)?;
        let written = self.place_at(&root, &blob, &meta, dest).await?;
        self.fix_owner(&root);
        Ok(json!({"reference": meta.reference, "written": written}))
    }

    pub async fn place_at(&self, root: &Path, blob: &Path, meta: &ArtifactMeta, dest: &str) -> Result<Vec<String>> {
        let into_dir = dest.ends_with('/');
        let dest = archive::clean_rel(dest)?;
        if meta.kind == "tree" {
            let tmp = self.cas.tmp_path("place");
            let res = {
                let (b, t, r, d) = (blob.to_path_buf(), tmp.clone(), root.to_path_buf(), dest.clone());
                tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
                    let m = archive::unpack(&b, &t, MAX_TREE)?;
                    let mut out = vec![];
                    for (p, e) in &m {
                        let to = if d.is_empty() { p.clone() } else { format!("{d}/{p}") };
                        archive::copy_into(&t.join(p), &r, &to, e.exec)?;
                        out.push(to);
                    }
                    Ok(out)
                })
                .await?
            };
            let _ = std::fs::remove_dir_all(&tmp);
            return res;
        }
        let base = Path::new(&meta.name).file_name().and_then(|n| n.to_str()).unwrap_or("artifact").to_string();
        let to = if dest.is_empty() {
            archive::clean_rel(&meta.name).unwrap_or(base)
        } else if into_dir || root.join(&dest).is_dir() {
            format!("{}/{base}", dest.trim_end_matches('/'))
        } else {
            dest
        };
        let (b, r, t) = (blob.to_path_buf(), root.to_path_buf(), to.clone());
        tokio::task::spawn_blocking(move || archive::copy_into(&b, &r, &t, false)).await??;
        Ok(vec![to])
    }

    /// Upload a file or directory of a workspace (relative path, already cleaned).
    pub async fn put_path(&self, root: &Path, rel: &str, info: PutInfo) -> Result<ArtifactMeta> {
        let rel = archive::clean_rel(rel)?;
        let full = root.join(&rel);
        let meta = std::fs::symlink_metadata(&full).with_context(|| format!("{rel}: not found in your workspace"))?;
        if meta.file_type().is_symlink() {
            bail!("{rel} is a symlink; share the file it points to instead");
        }
        let out = self.cas.tmp_path("put");
        let kind = if meta.is_dir() {
            let (r, rl, o) = (root.to_path_buf(), rel.clone(), out.clone());
            let n = tokio::task::spawn_blocking(move || -> Result<usize> {
                let (m, _) = archive::manifest(&r, &rl, &skip)?;
                archive::pack(&r, &m, &o)?;
                Ok(m.len())
            })
            .await??;
            if n == 0 {
                let _ = std::fs::remove_file(&out);
                bail!("{rel} contains no files");
            }
            "tree"
        } else {
            let (r, rl, o) = (root.to_path_buf(), rel.clone(), out.clone());
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut from = archive::open_inside(&r, &rl)?;
                let mut to = std::fs::File::create(&o)?;
                std::io::copy(&mut from, &mut to)?;
                Ok(())
            })
            .await??;
            "file"
        };
        let name = if info.name.is_empty() { rel.clone() } else { info.name.clone() };
        self.cas.put(&out, PutInfo { name, kind: kind.into(), ..info }).await
    }

    /// Upload a set of workspace files matching a glob as one artifact
    /// (one file -> a file artifact, several -> a tree).
    pub async fn put_glob(&self, root: &Path, pattern: &str, info: PutInfo) -> Result<Option<(ArtifactMeta, Vec<String>)>> {
        let glob = globset::Glob::new(pattern).map_err(|e| anyhow!("bad output pattern {pattern:?}: {e}"))?.compile_matcher();
        let (all, _) = {
            let r = root.to_path_buf();
            tokio::task::spawn_blocking(move || archive::manifest(&r, "", &skip)).await??
        };
        let m: Manifest = all.into_iter().filter(|(p, _)| glob.is_match(p)).collect();
        match m.len() {
            0 => Ok(None),
            1 => {
                let p = m.keys().next().unwrap().clone();
                let meta = self.put_path(root, &p, PutInfo { name: if info.name.is_empty() { p.clone() } else { info.name.clone() }, ..info }).await?;
                Ok(Some((meta, vec![p])))
            }
            _ => {
                let out = self.cas.tmp_path("put");
                {
                    let (r, mm, o) = (root.to_path_buf(), m.clone(), out.clone());
                    tokio::task::spawn_blocking(move || archive::pack(&r, &mm, &o)).await??;
                }
                let name = if info.name.is_empty() { pattern.to_string() } else { info.name.clone() };
                let meta = self.cas.put(&out, PutInfo { name, kind: "tree".into(), ..info }).await?;
                Ok(Some((meta, m.keys().cloned().collect())))
            }
        }
    }
}
