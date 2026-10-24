//! Directory trees as artifacts: manifests, tar.zst packing and safe file
//! access inside a workspace.
//!
//! Agents control their workspace, and the node reads and writes it from the
//! host side. So nothing here follows a symlink: walks skip links, reads and
//! writes open with `O_NOFOLLOW` and then check through `/proc/self/fd` that
//! the opened file really is inside the workspace. Archives only ever contain
//! regular files and directories with plain relative paths.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// One file of a tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// BLAKE3, hex.
    pub hash: String,
    pub size: u64,
    #[serde(default)]
    pub exec: bool,
}

/// Relative path (with `/`) -> file.
pub type Manifest = BTreeMap<String, FileEntry>;

/// Hash of a manifest: equal trees have equal digests.
pub fn manifest_digest(m: &Manifest) -> String {
    let mut h = blake3::Hasher::new();
    for (p, e) in m {
        h.update(p.as_bytes());
        h.update(&[0]);
        h.update(e.hash.as_bytes());
        h.update(&[e.exec as u8]);
    }
    h.finalize().to_hex().to_string()
}

pub fn hash_reader(mut r: impl Read) -> std::io::Result<(String, u64)> {
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finalize().to_hex().to_string(), n))
}

pub fn hash_file(p: &Path) -> std::io::Result<(String, u64)> {
    hash_reader(File::open(p)?)
}

/// A relative path made only of normal components (no `..`, no root), with `/`.
pub fn clean_rel(p: &str) -> Result<String> {
    let mut parts = vec![];
    for c in Path::new(p).components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| anyhow!("path is not UTF-8"))?.to_string()),
            Component::CurDir => {}
            _ => bail!("path {p:?} must stay inside the workspace (no `..` or absolute parts)"),
        }
    }
    if parts.is_empty() {
        return Ok(String::new());
    }
    Ok(parts.join("/"))
}

fn fd_path(f: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).ok()
}

fn check_inside(f: &File, root: &Path) -> Result<()> {
    match fd_path(f) {
        Some(p) if p.starts_with(root) => Ok(()),
        Some(p) => bail!("refusing {}: it resolves outside the workspace", p.display()),
        // No /proc (unusual): the O_NOFOLLOW open and the component checks still apply.
        None => Ok(()),
    }
}

/// Open `root/rel` for reading without following links out of `root`
/// (`root` must be canonical).
pub fn open_inside(root: &Path, rel: &str) -> Result<File> {
    let rel = clean_rel(rel)?;
    let p = root.join(&rel);
    let canon = std::fs::canonicalize(&p).with_context(|| format!("{rel}: not found"))?;
    if !canon.starts_with(root) {
        bail!("{rel} resolves outside the workspace");
    }
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&canon).with_context(|| format!("opening {rel}"))?;
    check_inside(&f, root)?;
    Ok(f)
}

/// Create (or truncate) `root/rel` for writing: missing parent directories
/// are created, and no component may be a symlink.
pub fn create_inside(root: &Path, rel: &str, exec: bool) -> Result<File> {
    let rel = clean_rel(rel)?;
    if rel.is_empty() {
        bail!("empty path");
    }
    let mut cur = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for d in &parts[..parts.len() - 1] {
        cur.push(d);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => bail!("{} is a symlink; refusing to write through it", cur.display()),
            Ok(m) if !m.is_dir() => bail!("{} exists and is not a directory", cur.display()),
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&cur).with_context(|| format!("creating {}", cur.display()))?,
        }
    }
    cur.push(parts[parts.len() - 1]);
    if let Ok(m) = std::fs::symlink_metadata(&cur) {
        if m.file_type().is_symlink() || m.is_dir() {
            bail!("{} is a symlink or directory; refusing to overwrite it", cur.display());
        }
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(if exec { 0o755 } else { 0o644 })
        .custom_flags(libc::O_NOFOLLOW)
        .open(&cur)
        .with_context(|| format!("creating {rel}"))?;
    if let Err(e) = check_inside(&f, root) {
        // A directory was swapped for a link between the checks and the open.
        if let Some(p) = fd_path(&f) {
            let _ = std::fs::remove_file(p);
        }
        return Err(e);
    }
    Ok(f)
}

/// Remove `root/rel` if it is a regular file inside `root`.
pub fn remove_inside(root: &Path, rel: &str) -> Result<()> {
    let rel = clean_rel(rel)?;
    let p = root.join(&rel);
    let parent = p.parent().ok_or_else(|| anyhow!("bad path"))?;
    let canon_parent = std::fs::canonicalize(parent)?;
    if !canon_parent.starts_with(root) {
        bail!("{rel} resolves outside the workspace");
    }
    let m = std::fs::symlink_metadata(&p)?;
    if m.is_file() || m.file_type().is_symlink() {
        std::fs::remove_file(&p)?;
    }
    Ok(())
}

/// Files under `root/sub` (no links followed; links and special files skipped).
/// Returns the manifest (paths relative to `root`) and what was skipped.
pub fn manifest(root: &Path, sub: &str, skip: &dyn Fn(&str) -> bool) -> Result<(Manifest, Vec<String>)> {
    let sub = clean_rel(sub)?;
    let start = if sub.is_empty() { root.to_path_buf() } else { root.join(&sub) };
    let mut m = Manifest::new();
    let mut skipped = vec![];
    if !start.exists() {
        return Ok((m, skipped));
    }
    for e in walkdir::WalkDir::new(&start).follow_links(false).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        if skip(&rel) {
            continue;
        }
        let ft = e.file_type();
        if ft.is_dir() {
            continue;
        }
        if !ft.is_file() {
            skipped.push(rel);
            continue;
        }
        let f = open_inside(root, &rel)?;
        let exec = f.metadata()?.permissions().mode() & 0o111 != 0;
        let (hash, size) = hash_reader(f)?;
        m.insert(rel, FileEntry { hash, size, exec });
    }
    Ok((m, skipped))
}

/// Pack the files of `m` (relative to `root`) into a tar.zst at `out`.
pub fn pack(root: &Path, m: &Manifest, out: &Path) -> Result<()> {
    let f = File::create(out)?;
    let enc = zstd::stream::write::Encoder::new(f, 3)?;
    let mut tar = tar::Builder::new(enc);
    for (rel, e) in m {
        let file = open_inside(root, rel)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(e.size);
        h.set_mode(if e.exec { 0o755 } else { 0o644 });
        h.set_mtime(file.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        tar.append_data(&mut h, rel, file.take(e.size))?;
    }
    let enc = tar.into_inner()?;
    enc.finish()?.sync_all()?;
    Ok(())
}

/// Extract a tar.zst into the empty directory `dest`. Only regular files and
/// directories with clean relative paths are accepted; at most `max_bytes` of
/// content (guards against decompression bombs).
pub fn unpack(archive: &Path, dest: &Path, max_bytes: u64) -> Result<Manifest> {
    std::fs::create_dir_all(dest)?;
    let dest = std::fs::canonicalize(dest)?;
    let dec = zstd::stream::read::Decoder::new(File::open(archive)?)?;
    let mut ar = tar::Archive::new(dec);
    let mut m = Manifest::new();
    let mut total = 0u64;
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let rel = clean_rel(&path)?;
        if rel.is_empty() {
            continue;
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(dest.join(&rel))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let exec = entry.header().mode().unwrap_or(0o644) & 0o111 != 0;
                let mut out = create_inside(&dest, &rel, exec)?;
                let mut h = blake3::Hasher::new();
                let mut buf = vec![0u8; 64 * 1024];
                let mut size = 0u64;
                loop {
                    let k = entry.read(&mut buf)?;
                    if k == 0 {
                        break;
                    }
                    total += k as u64;
                    if total > max_bytes {
                        bail!("archive expands to more than {max_bytes} bytes");
                    }
                    h.update(&buf[..k]);
                    out.write_all(&buf[..k])?;
                    size += k as u64;
                }
                m.insert(rel, FileEntry { hash: h.finalize().to_hex().to_string(), size, exec });
            }
            _ => {} // links, devices: never extracted
        }
    }
    Ok(m)
}

/// Copy `src` (outside the workspace, trusted) to `root/rel` (inside).
pub fn copy_into(src: &Path, root: &Path, rel: &str, exec: bool) -> Result<u64> {
    let mut from = File::open(src)?;
    let mut to = create_inside(root, rel, exec)?;
    Ok(std::io::copy(&mut from, &mut to)?)
}

/// What a three-way merge did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    /// Changed on both sides: the local version is kept and the incoming one
    /// is written next to it as `<path>.theirs`.
    pub conflicts: Vec<String>,
}

/// Merge the tree in `incoming_dir` (manifest `incoming`) into `root`, file
/// by file, against `base` (the last state both sides agreed on):
/// * unchanged here since base -> take theirs (add, update or delete);
/// * unchanged there since base -> keep ours;
/// * changed on both sides -> keep ours, write theirs as `<path>.theirs`.
pub fn merge3(root: &Path, incoming_dir: &Path, base: &Manifest, local: &Manifest, incoming: &Manifest) -> Result<MergeReport> {
    let mut r = MergeReport::default();
    let mut paths: Vec<&String> = base.keys().chain(local.keys()).chain(incoming.keys()).collect();
    paths.sort();
    paths.dedup();
    let same = |a: Option<&FileEntry>, b: Option<&FileEntry>| match (a, b) {
        (Some(a), Some(b)) => a.hash == b.hash && a.exec == b.exec,
        (None, None) => true,
        _ => false,
    };
    for p in paths {
        let (b, l, i) = (base.get(p), local.get(p), incoming.get(p));
        if same(l, i) {
            continue;
        }
        if same(l, b) {
            match i {
                Some(e) => {
                    copy_into(&incoming_dir.join(p), root, p, e.exec)?;
                    if l.is_some() {
                        r.updated.push(p.clone());
                    } else {
                        r.added.push(p.clone());
                    }
                }
                None => {
                    remove_inside(root, p)?;
                    r.deleted.push(p.clone());
                }
            }
        } else if same(i, b) {
            // Only we changed it.
        } else {
            if let Some(e) = i {
                copy_into(&incoming_dir.join(p), root, &format!("{p}.theirs"), e.exec)?;
            }
            r.conflicts.push(p.clone());
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn pack_unpack_roundtrip_skips_links() {
        let t = tempfile::tempdir().unwrap();
        let src = std::fs::canonicalize(t.path()).unwrap().join("src");
        std::fs::create_dir(&src).unwrap();
        w(&src, "a.txt", "hello");
        w(&src, "d/b.py", "print(1)");
        std::os::unix::fs::symlink("/etc/passwd", src.join("evil")).unwrap();
        let (m, skipped) = manifest(&src, "", &|_| false).unwrap();
        assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec!["a.txt", "d/b.py"]);
        assert_eq!(skipped, vec!["evil"]);
        let ar = t.path().join("x.tar.zst");
        pack(&src, &m, &ar).unwrap();
        let out = t.path().join("out");
        let m2 = unpack(&ar, &out, 1 << 20).unwrap();
        assert_eq!(m, m2);
        assert_eq!(std::fs::read_to_string(out.join("d/b.py")).unwrap(), "print(1)");
        assert!(unpack(&ar, &t.path().join("small"), 4).is_err(), "size cap");
    }

    #[test]
    fn no_escape_through_links() {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("s")).unwrap();
        assert!(open_inside(&root, "link/secret").is_err());
        assert!(open_inside(&root, "s").is_err());
        assert!(open_inside(&root, "../outside/secret").is_err());
        assert!(create_inside(&root, "link/new", false).is_err());
        assert!(create_inside(&root, "s", false).is_err());
        assert!(!outside.join("new").exists());
        assert!(create_inside(&root, "sub/ok.txt", false).is_ok());
    }

    #[test]
    fn three_way_merge() {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        let ours = root.join("ours");
        let theirs = root.join("theirs");
        for d in [&ours, &theirs] {
            w(d, "same.txt", "base");
            w(d, "ours-only.txt", "base");
            w(d, "theirs-only.txt", "base");
            w(d, "both.txt", "base");
            w(d, "gone.txt", "base");
        }
        let (base, _) = manifest(&ours, "", &|_| false).unwrap();
        w(&ours, "ours-only.txt", "ours");
        w(&ours, "both.txt", "ours");
        w(&theirs, "theirs-only.txt", "theirs");
        w(&theirs, "both.txt", "theirs");
        w(&theirs, "new.txt", "new");
        std::fs::remove_file(theirs.join("gone.txt")).unwrap();
        let (local, _) = manifest(&ours, "", &|_| false).unwrap();
        let (inc, _) = manifest(&theirs, "", &|_| false).unwrap();
        let r = merge3(&ours, &theirs, &base, &local, &inc).unwrap();
        assert_eq!(r.added, vec!["new.txt"]);
        assert_eq!(r.updated, vec!["theirs-only.txt"]);
        assert_eq!(r.deleted, vec!["gone.txt"]);
        assert_eq!(r.conflicts, vec!["both.txt"]);
        let rd = |p: &str| std::fs::read_to_string(ours.join(p)).unwrap();
        assert_eq!(rd("ours-only.txt"), "ours");
        assert_eq!(rd("theirs-only.txt"), "theirs");
        assert_eq!(rd("both.txt"), "ours");
        assert_eq!(rd("both.txt.theirs"), "theirs");
        assert!(!ours.join("gone.txt").exists());
    }
}
