//! Git repositories inside workspaces.
//!
//! A workspace snapshot never carries `.git` as files (a file-by-file merge of
//! `.git` corrupts it). Instead each repository's branches, tags and the
//! `refs/notes/moo` notes travel as a git bundle next to the files, and the
//! receiving side fetches it and moves its branches forward where that is a
//! fast-forward. Handing work back (`outputs: [{git: true}]`) uses the same
//! bundles.
//!
//! Git itself never runs on the host against a workspace: the repository's
//! config and hooks belong to the agent and could run anything. With the
//! podman runtime every git command runs in a throwaway container from the
//! agent image (no network, no capabilities, as the agent's user), so it can
//! do no more than the agent could. The process runtime (development only, no
//! isolation anyway) runs git directly. Refs are read host-side without git,
//! through the link-safe helpers, to tell whether anything changed.

use crate::cfgstore::ConfigStore;
use anyhow::{bail, Context, Result};
use moocher_protocol::archive;
use moocher_protocol::config::{GitConfig, RuntimeKind};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Repositories deeper than this below the workspace root are not looked for.
const MAX_DEPTH: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(300);
const CONTAINER_UID: u32 = 1000;

/// Directories never searched for repositories.
fn prune(name: &str) -> bool {
    matches!(name, ".moo-git" | "node_modules" | ".venv" | "venv" | "target" | ".cache")
}

/// Directories (relative, "" = root) of the git repositories in `root`.
pub fn find_repos(root: &Path) -> Vec<String> {
    let mut out = vec![];
    let mut it = walkdir::WalkDir::new(root).follow_links(false).max_depth(MAX_DEPTH).sort_by_file_name().into_iter();
    while let Some(e) = it.next() {
        let Ok(e) = e else { continue };
        if e.depth() == 0 || !e.file_type().is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if name == ".git" {
            if e.path().join("HEAD").is_file() {
                let repo = e.path().parent().and_then(|p| p.strip_prefix(root).ok()).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                out.push(repo);
            }
            it.skip_current_dir();
        } else if prune(&name) {
            it.skip_current_dir();
        }
    }
    out
}

/// A digest of a repository's HEAD and refs (branches, tags, notes): equal
/// digests mean nothing to send. Read without running git.
pub fn refs_digest(root: &Path, repo: &str) -> String {
    let git = if repo.is_empty() { ".git".to_string() } else { format!("{repo}/.git") };
    let mut h = blake3::Hasher::new();
    for f in ["HEAD", "packed-refs"] {
        if let Ok(mut file) = archive::open_inside(root, &format!("{git}/{f}")) {
            let _ = std::io::copy(&mut file, &mut h);
        }
        h.update(&[0]);
    }
    if let Ok((m, _)) = archive::manifest(root, &format!("{git}/refs"), &|_| false) {
        h.update(archive::manifest_digest(&m).as_bytes());
    }
    h.finalize().to_hex().to_string()
}

/// One digest over every repository in the workspace ("" when there is none).
pub fn workspace_refs_digest(root: &Path, repos: &[String]) -> String {
    if repos.is_empty() {
        return String::new();
    }
    let mut h = blake3::Hasher::new();
    for r in repos {
        h.update(r.as_bytes());
        h.update(refs_digest(root, r).as_bytes());
    }
    h.finalize().to_hex().to_string()
}

/// Runs git scripts against a workspace, sandboxed as described above.
pub struct GitRunner {
    kind: RuntimeKind,
    podman: String,
    rootless: bool,
    cfg: ConfigStore,
}

/// Bundles every listed repository: `$X/<n>.bundle` for the n-th argument.
/// Prints `ok <n>`, `empty <n>` (no refs yet) or `fail <n> <why>`.
const BUNDLE: &str = r#"
n=0
for r in "$@"; do
  n=$((n+1))
  d="$W/$r"
  refs=$(git -C "$d" for-each-ref --format='%(refname)' refs/heads refs/tags refs/notes/moo 2>/dev/null)
  if [ -z "$refs" ]; then echo "empty $n"; continue; fi
  if { git -C "$d" rev-parse -q --verify HEAD >/dev/null && echo HEAD; printf '%s\n' "$refs"; } \
       | git -C "$d" bundle create -q "$X/$n.bundle" --stdin 2>"$X/$n.err"; then
    echo "ok $n"
  else
    echo "fail $n $(head -c 300 "$X/$n.err" | tr '\n' ' ')"
  fi
done
"#;

/// Imports bundles: `$1` = remote namespace, `$2` = mode, then pairs of
/// (repository dir, bundle number). Modes:
///   sync   the bundle is the same workspace elsewhere (its files are already
///          merged): create missing branches, fast-forward the others (the
///          checked-out one with `reset --mixed`, so the index matches HEAD
///          and the working tree stays as merged); a new repository takes
///          the bundle's HEAD.
///   none / ff / merge   a hand-back: fetch into refs/remotes/<ns>/, then leave
///          the checked-out branch, fast-forward it, or merge the bundle's HEAD.
///          A directory that is not a repository yet becomes a clone.
/// Prints one line per finding: `created|ff|same|ahead|diverged <branch>`,
/// `head <branch>`, `merged <how>`, `notmerged <why>`, `cloned`, `error <why>`.
const IMPORT: &str = r#"
ns=$1; mode=$2; shift 2
while [ $# -ge 2 ]; do
  r=$1; b="$X/$2.bundle"; shift 2
  d="$W/$r"
  echo "repo $r"
  heads=$(git bundle list-heads "$b" 2>/dev/null) || { echo "error not a git bundle"; continue; }
  head_sha=$(printf '%s\n' "$heads" | awk '$2=="HEAD"{print $1}')
  mkdir -p "$d" && cd "$d" || { echo "error cannot enter $r"; continue; }
  fresh=0
  if [ ! -d .git ]; then
    if [ "$mode" != sync ] && [ -n "$(ls -A 2>/dev/null)" ]; then
      echo "error $r has files but no git repository; not touching it"; continue
    fi
    git init -q && fresh=1
  fi
  git fetch -q --no-tags "$b" "+refs/heads/*:refs/remotes/$ns/*" 2>"$X/f.err" || { echo "error fetch: $(head -c 300 "$X/f.err" | tr '\n' ' ')"; continue; }
  printf '%s\n' "$heads" | grep -q ' refs/tags/' && git fetch -q --no-tags "$b" "refs/tags/*:refs/tags/*" 2>/dev/null
  if printf '%s\n' "$heads" | grep -q ' refs/notes/moo$'; then
    git fetch -q "$b" "+refs/notes/moo:refs/notes/moo-incoming" 2>/dev/null
    if git rev-parse -q --verify refs/notes/moo >/dev/null; then
      git notes --ref=moo merge -q -s cat_sort_uniq refs/notes/moo-incoming >/dev/null 2>&1 || echo "error notes merge failed"
    else
      git update-ref refs/notes/moo refs/notes/moo-incoming
    fi
    git update-ref -d refs/notes/moo-incoming
  fi
  cur=$(git symbolic-ref -q --short HEAD)
  if [ "$mode" = sync ]; then
    for br in $(git for-each-ref --format='%(refname:strip=3)' "refs/remotes/$ns/"); do
      them=$(git rev-parse "refs/remotes/$ns/$br")
      us=$(git rev-parse -q --verify "refs/heads/$br")
      if [ -z "$us" ]; then git update-ref "refs/heads/$br" "$them"; echo "created $br"
      elif [ "$us" = "$them" ]; then echo "same $br"
      elif git merge-base --is-ancestor "$us" "$them"; then
        if [ "$br" = "$cur" ]; then git reset -q --mixed "$them"; else git update-ref "refs/heads/$br" "$them" "$us"; fi
        echo "ff $br"
      elif git merge-base --is-ancestor "$them" "$us"; then echo "ahead $br"
      else echo "diverged $br"; fi
    done
    if [ $fresh = 1 ] && [ -n "$head_sha" ]; then
      pick=""
      for br in $(git for-each-ref --format='%(refname:strip=2)' --points-at "$head_sha" refs/heads/); do
        if [ -z "$pick" ] || [ "$br" = main ] || [ "$br" = master ]; then pick=$br; fi
      done
      if [ -n "$pick" ]; then git symbolic-ref HEAD "refs/heads/$pick"; else git update-ref --no-deref HEAD "$head_sha"; fi
      git reset -q --mixed
      echo "head ${pick:-$head_sha}"
    fi
    continue
  fi
  [ -n "$head_sha" ] || { echo "notmerged the bundle has no HEAD"; continue; }
  if [ $fresh = 1 ]; then
    br=$(git for-each-ref --format='%(refname:strip=3)' --points-at "$head_sha" "refs/remotes/$ns/" | head -1)
    git checkout -q -B "${br:-main}" "$head_sha" && echo "cloned" || echo "error checkout failed"
    continue
  fi
  case "$mode" in
    none) echo "notmerged merge: none" ;;
    ff)
      if git merge -q --ff-only "$head_sha" >"$X/m.out" 2>&1; then echo "merged fast-forward"
      else echo "notmerged not a fast-forward or local changes in the way: $(head -c 300 "$X/m.out" | tr '\n' ' ')"; fi ;;
    merge)
      if git merge -q --no-edit -m "Merge work of $ns" "$head_sha" >"$X/m.out" 2>&1; then echo "merged"
      else git merge --abort >/dev/null 2>&1; echo "notmerged conflicts: $(head -c 300 "$X/m.out" | tr '\n' ' ')"; fi ;;
  esac
done
"#;

/// `git checkout -b` in the root repository (`$1` = branch name).
const BRANCH: &str = r#"
cd "$W" && [ -d .git ] || { echo "norepo"; exit 0; }
git checkout -q -b "$1" 2>&1 && echo "branched $1"
"#;

/// What an import did for one repository.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ImportReport {
    pub repo: String,
    pub lines: Vec<String>,
}

impl ImportReport {
    pub fn diverged(&self) -> Vec<&str> {
        self.lines.iter().filter_map(|l| l.strip_prefix("diverged ")).collect()
    }
}

impl GitRunner {
    pub fn new(kind: RuntimeKind, podman: String, rootless: bool, cfg: ConfigStore) -> GitRunner {
        GitRunner { kind, podman, rootless, cfg }
    }

    /// Who merge commits made on the agents' behalf are by.
    pub fn identity(&self) -> GitConfig {
        self.cfg.get().git()
    }

    /// Run `script` with the workspace at `$W` and `scratch` at `$X`.
    async fn run(&self, workspace: &Path, scratch: &Path, script: &str, args: &[String], identity: Option<&GitConfig>) -> Result<String> {
        let mut env: Vec<(String, String)> = vec![
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ];
        // Highest-precedence config: no hooks, no fsmonitor, any owner.
        let cfgs = [("safe.directory", "*"), ("core.hooksPath", "/dev/null"), ("core.fsmonitor", "false"), ("gc.auto", "0")];
        env.push(("GIT_CONFIG_COUNT".into(), cfgs.len().to_string()));
        for (i, (k, v)) in cfgs.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{i}"), k.to_string()));
            env.push((format!("GIT_CONFIG_VALUE_{i}"), v.to_string()));
        }
        let id = identity.cloned().unwrap_or_default();
        let name = id.name.unwrap_or_else(|| "moocher".into());
        let email = id.email.unwrap_or_else(|| "moocher@localhost".into());
        for (k, v) in [("GIT_AUTHOR_NAME", &name), ("GIT_COMMITTER_NAME", &name), ("GIT_AUTHOR_EMAIL", &email), ("GIT_COMMITTER_EMAIL", &email)] {
            env.push((k.into(), v.clone()));
        }
        let mut cmd = match self.kind {
            RuntimeKind::Process => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(script).arg("sh").args(args);
                c.env("W", workspace).env("X", scratch).env("HOME", scratch);
                for (k, v) in &env {
                    c.env(k, v);
                }
                c
            }
            RuntimeKind::Podman => {
                if !self.rootless {
                    crate::runtime::chown_r(scratch, CONTAINER_UID)?;
                }
                let image = self.cfg.get().node.runtime.image.clone();
                let mut c = Command::new(&self.podman);
                c.args(["run", "--rm", "--network=none", "--cap-drop=all", "--security-opt=no-new-privileges", "--read-only", "--pids-limit=256", "--memory=1g"]);
                c.args(["--tmpfs", "/tmp:rw,size=64m"]);
                if self.rootless {
                    c.arg(format!("--userns=keep-id:uid={CONTAINER_UID},gid={CONTAINER_UID}"));
                } else {
                    c.arg(format!("--user={CONTAINER_UID}:{CONTAINER_UID}"));
                }
                c.arg("-v").arg(format!("{}:/workspace", workspace.display()));
                c.arg("-v").arg(format!("{}:/x", scratch.display()));
                for (k, v) in env.iter().chain([("W".to_string(), "/workspace".to_string()), ("X".into(), "/x".into()), ("HOME".into(), "/tmp".into())].iter()) {
                    c.arg("-e").arg(format!("{k}={v}"));
                }
                c.args(["--entrypoint", "/bin/sh", &image, "-c", script, "sh"]).args(args);
                c
            }
        };
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(TIMEOUT, cmd.output()).await.map_err(|_| anyhow::anyhow!("git timed out"))?.context("running git")?;
        if !out.status.success() {
            bail!("git: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Bundle the repositories `repos` of `workspace` into `scratch`. Returns
    /// (repository, bundle file) for each one that has refs.
    pub async fn bundle(&self, workspace: &Path, scratch: &Path, repos: &[String]) -> Result<Vec<(String, PathBuf)>> {
        if repos.is_empty() {
            return Ok(vec![]);
        }
        std::fs::create_dir_all(scratch)?;
        let out = self.run(workspace, scratch, BUNDLE, repos, None).await?;
        let mut v = vec![];
        for line in out.lines() {
            let mut it = line.splitn(3, ' ');
            match (it.next(), it.next().and_then(|n| n.parse::<usize>().ok())) {
                (Some("ok"), Some(n)) if n >= 1 && n <= repos.len() => v.push((repos[n - 1].clone(), scratch.join(format!("{n}.bundle")))),
                (Some("fail"), Some(n)) if n >= 1 && n <= repos.len() => {
                    tracing::warn!(repo = %repos[n - 1], why = it.next().unwrap_or(""), "git bundle failed");
                }
                _ => {}
            }
        }
        Ok(v)
    }

    /// Import bundles into `workspace` (see `IMPORT` for the modes).
    pub async fn import(&self, workspace: &Path, scratch: &Path, ns: &str, mode: &str, bundles: &[(String, PathBuf)], identity: &GitConfig) -> Result<Vec<ImportReport>> {
        if bundles.is_empty() {
            return Ok(vec![]);
        }
        if ns.is_empty() || !ns.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("bad remote name {ns:?}");
        }
        std::fs::create_dir_all(scratch)?;
        let mut args = vec![ns.to_string(), mode.to_string()];
        for (i, (repo, file)) in bundles.iter().enumerate() {
            let n = i + 1;
            std::fs::copy(file, scratch.join(format!("{n}.bundle")))?;
            args.push(repo.clone());
            args.push(n.to_string());
        }
        let out = self.run(workspace, scratch, IMPORT, &args, Some(identity)).await?;
        let mut reports: Vec<ImportReport> = vec![];
        for line in out.lines() {
            if let Some(r) = line.strip_prefix("repo ") {
                reports.push(ImportReport { repo: r.to_string(), lines: vec![] });
            } else if let Some(cur) = reports.last_mut() {
                cur.lines.push(line.to_string());
            }
        }
        Ok(reports)
    }

    /// Check out a new branch in the workspace's root repository.
    pub async fn branch(&self, workspace: &Path, scratch: &Path, name: &str) -> Result<String> {
        if name.is_empty() || name.starts_with('-') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c)) {
            bail!("bad branch name {name:?}");
        }
        std::fs::create_dir_all(scratch)?;
        Ok(self.run(workspace, scratch, BRANCH, &[name.to_string()], None).await?.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repos_without_descending_into_dependencies() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for d in [".git/refs/heads", "lib/.git/refs", "node_modules/x/.git/refs", "a/b/c/d/.git/refs"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [".git/HEAD", "lib/.git/HEAD", "node_modules/x/.git/HEAD", "a/b/c/d/.git/HEAD"] {
            std::fs::write(root.join(f), "ref: refs/heads/main\n").unwrap();
        }
        assert_eq!(find_repos(root), vec!["".to_string(), "lib".to_string()]);
        let before = refs_digest(root, "");
        std::fs::write(root.join(".git/refs/heads/main"), "0123\n").unwrap();
        assert_ne!(before, refs_digest(root, ""));
    }
}
