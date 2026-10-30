//! Workspace <-> artifact operations on this node: snapshots, three-way
//! merges of incoming snapshots, placing artifacts, and putting agent files.
//! All file access inside a workspace goes through `moocher_protocol::archive`'s
//! link-safe helpers (agents control what is in there).

use crate::cas::{Cas, PutInfo};
use crate::gitops::{self, GitRunner};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use moocher_protocol::archive::{self, Manifest};
use moocher_protocol::config::GitMerge;
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
    pub git: GitRunner,
}

/// Snapshots carry the whole tree except git internals (links and special
/// files are never included); repositories travel as bundles (`gitops`).
fn skip(rel: &str) -> bool {
    archive::is_git_internal(rel)
}

/// What decides whether a workspace changed: its files and its git refs.
fn snap_digest(m: &Manifest, refs: &str) -> String {
    let d = archive::manifest_digest(m);
    if refs.is_empty() {
        return d;
    }
    blake3::hash(format!("{d}:{refs}").as_bytes()).to_hex().to_string()
}

/// Remote names that bundles are fetched under: the machine a synced copy
/// came from, or the session that handed work back / was forked from.
fn remote_of_node(meta: &ArtifactMeta) -> String {
    sanitize(&format!("moo-{}", meta.node.as_deref().unwrap_or("hub")))
}

fn remote_of_producer(meta: &ArtifactMeta) -> String {
    meta.producer.as_deref().filter(|p| p.starts_with("s-")).map(sanitize).unwrap_or_else(|| remote_of_node(meta))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

impl WsOps {
    pub fn new(cas: Arc<Cas>, store: Arc<Store>, data_root: PathBuf, chown_to: Option<u32>, git: GitRunner) -> WsOps {
        WsOps { cas, store, data_root, chown_to, git }
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
        let (m, skipped, repos) = {
            let r = root.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                let (m, skipped) = archive::manifest(&r, "", &skip)?;
                Ok((m, skipped, gitops::find_repos(&r)))
            })
            .await??
        };
        let digest = snap_digest(&m, &gitops::workspace_refs_digest(&root, &repos));
        if let Some((_, reference, d)) = self.store.ws_base(label) {
            if d == digest {
                return Ok(json!({"reference": reference, "files": m.len(), "unchanged": true}));
            }
        }
        let scratch = self.cas.tmp_path("git");
        let bundles = match self.git.bundle(&root, &scratch, &repos).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(workspace = label, error = %format!("{e:#}"), "git history not included in the snapshot");
                vec![]
            }
        };
        let extra: Vec<(String, PathBuf)> = bundles.iter().map(|(r, f)| (archive::bundle_entry(r), f.clone())).collect();
        let out = self.cas.tmp_path("snap");
        let packed = {
            let (r, m, o) = (root.clone(), m.clone(), out.clone());
            tokio::task::spawn_blocking(move || archive::pack_with(&r, &m, &extra, &o)).await?
        };
        let _ = std::fs::remove_dir_all(&scratch);
        packed?;
        let parents: Vec<String> = self.store.ws_base(label).map(|(_, r, _)| r).into_iter().collect();
        let meta = self
            .cas
            .put(&out, PutInfo { name: format!("{label} ({reason})"), kind: "tree".into(), media_type: String::new(), session: session.map(String::from), parents, tags: vec!["workspace".into(), reason.into()] })
            .await?;
        self.store.set_ws_base(label, &m, &meta.reference, &digest)?;
        let bytes: u64 = m.values().map(|e| e.size).sum();
        Ok(json!({"reference": meta.reference, "files": m.len(), "bytes": bytes, "skipped_links": skipped, "unchanged": false, "git": bundles.iter().map(|(r, _)| r).collect::<Vec<_>>()}))
    }

    /// Merge a snapshot into this node's copy (created if missing): files
    /// three-way, git repositories by fetching their bundles.
    pub async fn apply(&self, label: &str, reference: &str) -> Result<Value> {
        let (blob, meta) = self.cas.get(reference).await?;
        if meta.kind != "tree" {
            bail!("{reference} is a file, not a workspace snapshot");
        }
        let root = self.canon(label, true)?;
        let tmp = self.cas.tmp_path("ws");
        let scratch = self.cas.tmp_path("git");
        let res = async {
            let unpacked = {
                let (b, t) = (blob.clone(), tmp.clone());
                tokio::task::spawn_blocking(move || archive::unpack(&b, &t, MAX_TREE)).await??
            };
            let bundles: Vec<(String, PathBuf)> = unpacked.keys().filter_map(|p| Some((archive::bundle_repo(p)?, tmp.join(p)))).collect();
            let incoming: Manifest = unpacked.into_iter().filter(|(p, _)| !skip(p)).collect();
            let base: Manifest = self.store.ws_base(label).map(|(m, _, _)| m).unwrap_or_default();
            let (r, t, inc) = (root.clone(), tmp.clone(), incoming.clone());
            let report = tokio::task::spawn_blocking(move || -> Result<archive::MergeReport> {
                let (local, _) = archive::manifest(&r, "", &skip)?;
                archive::merge3(&r, &t, &base, &local, &inc)
            })
            .await??;
            let git = self.git.import(&root, &scratch, &remote_of_node(&meta), "sync", &bundles, &self.git.identity()).await.unwrap_or_else(|e| {
                vec![gitops::ImportReport { repo: String::new(), lines: vec![format!("error {e:#}")] }]
            });
            let refs = gitops::workspace_refs_digest(&root, &gitops::find_repos(&root));
            self.store.set_ws_base(label, &incoming, &meta.reference, &snap_digest(&incoming, &refs))?;
            self.fix_owner(&root);
            Ok::<_, anyhow::Error>((report, git))
        }
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&scratch);
        let (report, git) = res?;
        let diverged: Vec<String> = git.iter().flat_map(|g| g.diverged().into_iter().map(move |b| if g.repo.is_empty() { b.to_string() } else { format!("{}: {b}", g.repo) })).collect();
        Ok(json!({
            "applied": report.added.len() + report.updated.len() + report.deleted.len(),
            "added": report.added, "updated": report.updated, "deleted": report.deleted, "conflicts": report.conflicts,
            "git": git, "diverged": diverged,
        }))
    }

    /// Put an artifact into a workspace: a file at `dest` (default: its name;
    /// a trailing `/` means "into this directory"), a tree under the directory
    /// `dest`, a git bundle into the repository at `dest` (see `merge`).
    pub async fn place(&self, label: &str, reference: &str, dest: &str, merge: GitMerge) -> Result<Value> {
        let (blob, meta) = self.cas.get(reference).await?;
        let root = self.canon(label, true)?;
        if meta.kind == "git" {
            let repo = archive::clean_rel(dest)?;
            let scratch = self.cas.tmp_path("git");
            let res = self.git.import(&root, &scratch, &remote_of_producer(&meta), merge.as_str(), &[(repo.clone(), blob)], &self.git.identity()).await;
            let _ = std::fs::remove_dir_all(&scratch);
            self.fix_owner(&root);
            let git = res?;
            return Ok(json!({"reference": meta.reference, "repository": repo, "remote": remote_of_producer(&meta), "git": git}));
        }
        let written = self.place_at(&root, &blob, &meta, dest).await?;
        self.fix_owner(&root);
        Ok(json!({"reference": meta.reference, "written": written}))
    }

    /// `place` for files and trees. A tree's git bundles (from a workspace
    /// snapshot) become repositories under `dest`.
    pub async fn place_at(&self, root: &Path, blob: &Path, meta: &ArtifactMeta, dest: &str) -> Result<Vec<String>> {
        let into_dir = dest.ends_with('/');
        let dest = archive::clean_rel(dest)?;
        if meta.kind == "tree" {
            let tmp = self.cas.tmp_path("place");
            let res = {
                let (b, t, r, d) = (blob.to_path_buf(), tmp.clone(), root.to_path_buf(), dest.clone());
                tokio::task::spawn_blocking(move || -> Result<(Vec<String>, Vec<(String, PathBuf)>)> {
                    let m = archive::unpack(&b, &t, MAX_TREE)?;
                    let join = |p: &str| if d.is_empty() { p.to_string() } else if p.is_empty() { d.clone() } else { format!("{d}/{p}") };
                    let (mut out, mut bundles) = (vec![], vec![]);
                    for (p, e) in &m {
                        if let Some(repo) = archive::bundle_repo(p) {
                            bundles.push((join(&repo), t.join(p)));
                            continue;
                        }
                        if skip(p) {
                            continue;
                        }
                        let to = join(p);
                        archive::copy_into(&t.join(p), &r, &to, e.exec)?;
                        out.push(to);
                    }
                    Ok((out, bundles))
                })
                .await?
            };
            let res = match res {
                Ok((written, bundles)) if !bundles.is_empty() => {
                    let scratch = self.cas.tmp_path("git");
                    let r = self.git.import(root, &scratch, &remote_of_producer(meta), "sync", &bundles, &self.git.identity()).await;
                    let _ = std::fs::remove_dir_all(&scratch);
                    match r {
                        Ok(reps) => tracing::info!(repos = reps.len(), "git history restored"),
                        Err(e) => tracing::warn!(error = %format!("{e:#}"), "git history not restored"),
                    }
                    Ok(written)
                }
                other => other.map(|(w, _)| w),
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

    /// Upload the git history of the repository at `repo` (relative; "" =
    /// the root) as a bundle artifact. `None` when there is no repository or
    /// it has no commits.
    pub async fn put_git(&self, root: &Path, repo: &str, info: PutInfo) -> Result<Option<ArtifactMeta>> {
        let repo = archive::clean_rel(repo)?;
        if !gitops::find_repos(root).contains(&repo) {
            return Ok(None);
        }
        let scratch = self.cas.tmp_path("git");
        let res = async {
            let Some((_, file)) = self.git.bundle(root, &scratch, std::slice::from_ref(&repo)).await?.into_iter().next() else {
                return Ok(None);
            };
            let name = if info.name.is_empty() { format!("{} (git)", if repo.is_empty() { "workspace" } else { &repo }) } else { info.name.clone() };
            let meta = self.cas.put(&file, PutInfo { name, kind: "git".into(), media_type: "application/x-git-bundle".into(), ..info }).await?;
            Ok(Some(meta))
        }
        .await;
        let _ = std::fs::remove_dir_all(&scratch);
        res
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
