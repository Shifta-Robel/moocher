//! `disk_enforce: hard` (node.yaml): the kernel enforces `container.disk`.
//!
//! The default is `soft`: the node measures and interrupts at 100% (manager).
//! `hard` adds a filesystem quota over the instance's OpenCode home and its
//! isolated workspace together, so a write past the limit fails with ENOSPC
//! (EDQUOT) instead of being noticed seconds later:
//!
//! * **XFS** (mounted with `prjquota`): one project id per instance, set on
//!   both directories (`xfs_quota project -s`), with a `bhard` block limit;
//! * **btrfs**: both directories are subvolumes, put under one level-1
//!   qgroup that carries the limit (`quota enable` once per filesystem).
//!
//! Shared workspaces belong to several sessions and stay soft. Quotas need
//! root (CAP_SYS_ADMIN); the node checks everything at start and refuses to
//! run in `hard` mode on anything else, rather than silently enforcing less.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fs {
    Xfs { mount: PathBuf, prjquota: bool },
    Btrfs { mount: PathBuf },
    Other { mount: PathBuf, fstype: String },
}

/// One step of a plan. Btrfs subvolume ids are only known once created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Run(Vec<String>),
    /// `btrfs qgroup assign 0/<id of dir> <qgroup> <mount>`.
    BtrfsAssign { dir: PathBuf, qgroup: String, mount: PathBuf },
    /// Create a subvolume unless the directory exists already.
    BtrfsSubvol(PathBuf),
}

/// The filesystem holding `path`, from `/proc/self/mountinfo` text.
pub fn detect_in(mountinfo: &str, path: &Path) -> Option<Fs> {
    let mut best: Option<(PathBuf, String, String)> = None;
    for line in mountinfo.lines() {
        // id parent maj:min root mountpoint opts [optional...] - fstype source superopts
        let (pre, post) = line.split_once(" - ")?;
        let f: Vec<&str> = pre.split(' ').collect();
        let g: Vec<&str> = post.split(' ').collect();
        let (Some(mp), Some(fstype)) = (f.get(4), g.first()) else { continue };
        let mp = PathBuf::from(unescape(mp));
        if path.starts_with(&mp) && best.as_ref().is_none_or(|(b, _, _)| mp.as_os_str().len() >= b.as_os_str().len()) {
            best = Some((mp, fstype.to_string(), g.get(2).copied().unwrap_or("").to_string()));
        }
    }
    let (mount, fstype, superopts) = best?;
    Some(match fstype.as_str() {
        "xfs" => Fs::Xfs { mount, prjquota: superopts.split(',').any(|o| matches!(o, "prjquota" | "pquota" | "prjjquota")) },
        "btrfs" => Fs::Btrfs { mount },
        _ => Fs::Other { mount, fstype },
    })
}

fn unescape(s: &str) -> String {
    // mountinfo escapes space, tab, newline and backslash as \ooo.
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub fn detect(path: &Path) -> Result<Fs> {
    let path = std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let mi = std::fs::read_to_string("/proc/self/mountinfo").context("reading /proc/self/mountinfo")?;
    detect_in(&mi, &path).ok_or_else(|| anyhow::anyhow!("no mount found for {}", path.display()))
}

/// Why `hard` cannot work here, if it cannot.
pub fn check(fs: &Fs, euid: u32) -> Result<()> {
    match fs {
        Fs::Xfs { mount, prjquota: false } => bail!("disk_enforce: hard needs XFS mounted with prjquota ({} is not); add it to the mount options in /etc/fstab and remount", mount.display()),
        Fs::Other { mount, fstype } => bail!("disk_enforce: hard needs XFS (prjquota) or btrfs under the data dir; {} is {fstype}", mount.display()),
        _ if euid != 0 => bail!("disk_enforce: hard needs the node to run as root (filesystem quotas need CAP_SYS_ADMIN)"),
        _ => Ok(()),
    }
}

/// A stable quota id for an instance (XFS project id / btrfs qgroup 1/<id>).
pub fn quota_id(instance: &str) -> u32 {
    // FNV-1a, kept clear of small ids an admin may use by hand.
    let mut h: u32 = 0x811c9dc5;
    for b in instance.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    1_000_000 + h % 1_000_000_000
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Commands that put `dirs` under one limit of `bytes`.
pub fn plan_apply(fs: &Fs, dirs: &[PathBuf], id: u32, bytes: u64) -> Vec<Step> {
    let mut p = vec![];
    match fs {
        Fs::Xfs { mount, .. } => {
            let m = mount.display().to_string();
            for d in dirs {
                p.push(Step::Run(s(&["xfs_quota", "-x", "-c", &format!("project -s -p {} {id}", d.display()), &m])));
            }
            p.push(Step::Run(s(&["xfs_quota", "-x", "-c", &format!("limit -p bhard={bytes} {id}"), &m])));
        }
        Fs::Btrfs { mount } => {
            let m = mount.display().to_string();
            let q = format!("1/{id}");
            p.push(Step::Run(s(&["btrfs", "quota", "enable", &m])));
            for d in dirs {
                p.push(Step::BtrfsSubvol(d.clone()));
            }
            p.push(Step::Run(s(&["btrfs", "qgroup", "create", &q, &m])));
            for d in dirs {
                p.push(Step::BtrfsAssign { dir: d.clone(), qgroup: q.clone(), mount: mount.clone() });
            }
            p.push(Step::Run(s(&["btrfs", "qgroup", "limit", &bytes.to_string(), &q, &m])));
        }
        Fs::Other { .. } => {}
    }
    p
}

/// Commands that lift the limit again (before the directories are removed).
pub fn plan_clear(fs: &Fs, dirs: &[PathBuf], id: u32) -> Vec<Step> {
    match fs {
        Fs::Xfs { mount, .. } => {
            let m = mount.display().to_string();
            let mut p = vec![Step::Run(s(&["xfs_quota", "-x", "-c", &format!("limit -p bhard=0 {id}"), &m]))];
            for d in dirs {
                p.push(Step::Run(s(&["xfs_quota", "-x", "-c", &format!("project -C -p {} {id}", d.display()), &m])));
            }
            p
        }
        // The subvolumes go with the directories; the level-1 group stays unless destroyed.
        Fs::Btrfs { mount } => vec![Step::Run(s(&["btrfs", "qgroup", "destroy", &format!("1/{id}"), &mount.display().to_string()]))],
        Fs::Other { .. } => vec![],
    }
}

async fn run(cmd: &[String]) -> Result<String> {
    let out = tokio::process::Command::new(&cmd[0]).args(&cmd[1..]).output().await.with_context(|| format!("running {}", cmd[0]))?;
    if !out.status.success() {
        bail!("{} failed: {}", cmd.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Put `dirs` (created if missing) under one hard limit.
pub async fn apply(fs: &Fs, dirs: &[PathBuf], id: u32, bytes: u64) -> Result<()> {
    if matches!(fs, Fs::Xfs { .. }) {
        for d in dirs {
            std::fs::create_dir_all(d)?;
        }
    }
    execute(&plan_apply(fs, dirs, id, bytes)).await
}

pub async fn execute(plan: &[Step]) -> Result<()> {
    for step in plan {
        match step {
            Step::Run(c) => {
                run(c).await?;
            }
            Step::BtrfsSubvol(d) => {
                if !d.exists() {
                    if let Some(parent) = d.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    run(&s(&["btrfs", "subvolume", "create", &d.display().to_string()])).await?;
                }
            }
            Step::BtrfsAssign { dir, qgroup, mount } => {
                let id = run(&s(&["btrfs", "inspect-internal", "rootid", &dir.display().to_string()])).await?;
                let id = id.trim();
                if id.parse::<u64>().is_err() || id == "5" {
                    bail!("{} is not a btrfs subvolume (it existed before disk_enforce: hard)", dir.display());
                }
                run(&s(&["btrfs", "qgroup", "assign", &format!("0/{id}"), qgroup, &mount.display().to_string()])).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MI: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 8:17 / /srv/moo rw,noatime shared:20 - xfs /dev/sdb1 rw,attr2,inode64,logbufs=8,prjquota
41 22 8:33 / /srv/plain\\040xfs rw - xfs /dev/sdc1 rw,attr2,noquota
42 22 0:45 /@data /var/lib/moo rw shared:30 - btrfs /dev/sdd rw,space_cache=v2,subvolid=256,subvol=/@data
";

    #[test]
    fn detects_the_filesystem_under_a_path() {
        assert_eq!(detect_in(MI, Path::new("/srv/moo/data/instances")), Some(Fs::Xfs { mount: "/srv/moo".into(), prjquota: true }));
        assert_eq!(detect_in(MI, Path::new("/srv/plain xfs/d")), Some(Fs::Xfs { mount: "/srv/plain xfs".into(), prjquota: false }));
        assert_eq!(detect_in(MI, Path::new("/var/lib/moo/w")), Some(Fs::Btrfs { mount: "/var/lib/moo".into() }));
        assert!(matches!(detect_in(MI, Path::new("/home/x")), Some(Fs::Other { fstype, .. }) if fstype == "ext4"));
        assert!(check(&detect_in(MI, Path::new("/srv/plain xfs/d")).unwrap(), 0).unwrap_err().to_string().contains("prjquota"));
        assert!(check(&detect_in(MI, Path::new("/home")).unwrap(), 0).unwrap_err().to_string().contains("ext4"));
        assert!(check(&detect_in(MI, Path::new("/srv/moo")).unwrap(), 1000).unwrap_err().to_string().contains("root"));
        assert!(check(&detect_in(MI, Path::new("/srv/moo")).unwrap(), 0).is_ok());
    }

    #[test]
    fn plans() {
        let dirs = vec![PathBuf::from("/srv/moo/data/instances/s-1/home"), PathBuf::from("/srv/moo/data/workspaces/s-1")];
        let id = quota_id("s-1");
        assert_eq!(id, quota_id("s-1"));
        assert_ne!(id, quota_id("s-2"));
        assert!(id >= 1_000_000);
        let xfs = Fs::Xfs { mount: "/srv/moo".into(), prjquota: true };
        let p = plan_apply(&xfs, &dirs, id, 5 << 30);
        assert_eq!(
            p,
            vec![
                Step::Run(s(&["xfs_quota", "-x", "-c", &format!("project -s -p /srv/moo/data/instances/s-1/home {id}"), "/srv/moo"])),
                Step::Run(s(&["xfs_quota", "-x", "-c", &format!("project -s -p /srv/moo/data/workspaces/s-1 {id}"), "/srv/moo"])),
                Step::Run(s(&["xfs_quota", "-x", "-c", &format!("limit -p bhard=5368709120 {id}"), "/srv/moo"])),
            ]
        );
        assert_eq!(plan_clear(&xfs, &dirs, id)[0], Step::Run(s(&["xfs_quota", "-x", "-c", &format!("limit -p bhard=0 {id}"), "/srv/moo"])));
        let b = Fs::Btrfs { mount: "/var/lib/moo".into() };
        let p = plan_apply(&b, &dirs, id, 1 << 30);
        assert_eq!(p[0], Step::Run(s(&["btrfs", "quota", "enable", "/var/lib/moo"])));
        assert_eq!(p[1], Step::BtrfsSubvol(dirs[0].clone()));
        assert_eq!(p[3], Step::Run(s(&["btrfs", "qgroup", "create", &format!("1/{id}"), "/var/lib/moo"])));
        assert_eq!(p[5], Step::BtrfsAssign { dir: dirs[1].clone(), qgroup: format!("1/{id}"), mount: "/var/lib/moo".into() });
        assert_eq!(p[6], Step::Run(s(&["btrfs", "qgroup", "limit", "1073741824", &format!("1/{id}"), "/var/lib/moo"])));
        assert!(plan_apply(&Fs::Other { mount: "/".into(), fstype: "ext4".into() }, &dirs, id, 1).is_empty());
    }
}
