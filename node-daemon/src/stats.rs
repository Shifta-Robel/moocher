//! Resource usage of the host and of each instance (PHASE1_5 §12).
//!
//! * Podman: the container's cgroup v2 files (`memory.current`, `memory.max`,
//!   `pids.current`, `cpu.stat`), located once via `podman inspect`.
//! * Process runtime: the instance's process tree from /proc.
//! * Disk: a size walk of the workspace and OpenCode home, at most once a
//!   minute per instance (bounded, so a huge tree cannot stall the node).

use moocher_protocol::model::{now_ms, HostStats, InstanceStats, Ts};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

const DISK_EVERY_MS: Ts = 60_000;
const DISK_MAX_ENTRIES: usize = 300_000;

#[derive(Default)]
pub struct Collector {
    /// id -> (cpu time in µs, when measured)
    prev_cpu: Mutex<HashMap<String, (u64, Instant)>>,
    /// id -> (bytes, when measured)
    disk: Mutex<HashMap<String, (u64, Ts)>>,
}

/// Where the numbers for one instance come from.
pub enum Source {
    Cgroup(PathBuf),
    Pid(u32),
    None,
}

pub fn host(node: &str, data_dir: &Path) -> HostStats {
    let mut mem_total = 0;
    let mut mem_available = 0;
    if let Ok(m) = std::fs::read_to_string("/proc/meminfo") {
        for l in m.lines() {
            let kb = || l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if l.starts_with("MemTotal:") {
                mem_total = kb();
            } else if l.starts_with("MemAvailable:") {
                mem_available = kb();
            }
        }
    }
    let load1 = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let (disk_total, disk_free) = statvfs(data_dir).unwrap_or((0, 0));
    HostStats {
        node: node.into(),
        cpus: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        load1,
        mem_total,
        mem_available,
        disk_total,
        disk_free,
        ts: now_ms(),
    }
}

fn statvfs(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some((s.f_blocks as u64 * f, s.f_bavail as u64 * f))
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// (cpu µs, memory bytes, memory limit, pids) from a cgroup v2 directory.
fn cgroup(dir: &Path) -> Option<(u64, u64, Option<u64>, u64)> {
    let usec = std::fs::read_to_string(dir.join("cpu.stat")).ok()?.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())?;
    let mem = read_u64(&dir.join("memory.current"))?;
    let limit = read_u64(&dir.join("memory.max")); // "max" -> None
    let pids = read_u64(&dir.join("pids.current")).unwrap_or(0);
    Some((usec, mem, limit, pids))
}

/// The process and its descendants: (cpu µs, rss bytes, pids).
fn process_tree(root: u32) -> Option<(u64, u64, u64)> {
    let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut stat: HashMap<u32, (u64, u64)> = HashMap::new(); // pid -> (cpu ticks, rss pages)
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(s) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // Fields after the ")" of the command name: state ppid ... utime(12) stime(13) ... rss(22)
        let Some(rest) = s.rsplit_once(')').map(|(_, r)| r) else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 22 {
            continue;
        }
        let ppid: u32 = f[1].parse().unwrap_or(0);
        let cpu = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
        let rss: u64 = f[21].parse().unwrap_or(0);
        children.entry(ppid).or_default().push(pid);
        stat.insert(pid, (cpu, rss));
    }
    stat.get(&root)?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let (mut cpu, mut rss, mut n) = (0u64, 0u64, 0u64);
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some((c, r)) = stat.get(&p) {
            cpu += c;
            rss += r * page;
            n += 1;
        }
        stack.extend(children.get(&p).into_iter().flatten());
    }
    Some((cpu * 1_000_000 / tick, rss, n))
}

/// Total size of the files under `dirs` (bounded walk, no symlinks followed).
fn du(dirs: &[&Path]) -> u64 {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > DISK_MAX_ENTRIES {
                return total;
            }
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

impl Collector {
    pub fn instance(&self, id: &str, node: &str, src: &Source, disk_dirs: &[&Path], mem_limit_cfg: Option<u64>, disk_limit: Option<u64>) -> InstanceStats {
        let now = Instant::now();
        let measured = match src {
            Source::Cgroup(dir) => cgroup(dir).map(|(u, m, l, p)| (u, m, l.or(mem_limit_cfg), p)),
            Source::Pid(pid) => process_tree(*pid).map(|(u, m, p)| (u, m, mem_limit_cfg, p)),
            Source::None => None,
        };
        let mut st = InstanceStats {
            session: id.into(),
            node: node.into(),
            cpu_pct: None,
            mem_bytes: None,
            mem_limit: mem_limit_cfg,
            pids: None,
            disk_bytes: None,
            disk_limit,
            ts: now_ms(),
        };
        if let Some((usec, mem, limit, pids)) = measured {
            let mut prev = self.prev_cpu.lock().unwrap();
            if let Some((pu, pt)) = prev.get(id) {
                let wall = now.duration_since(*pt).as_micros() as f64;
                if wall > 0.0 && usec >= *pu {
                    st.cpu_pct = Some(((usec - pu) as f64 / wall * 1000.0).round() / 10.0);
                }
            }
            prev.insert(id.to_string(), (usec, now));
            st.mem_bytes = Some(mem);
            st.mem_limit = limit;
            st.pids = Some(pids);
        }
        let due = self.disk.lock().unwrap().get(id).is_none_or(|(_, at)| now_ms() - at >= DISK_EVERY_MS);
        if due {
            let bytes = du(disk_dirs);
            self.disk.lock().unwrap().insert(id.to_string(), (bytes, now_ms()));
        }
        st.disk_bytes = self.disk.lock().unwrap().get(id).map(|(b, _)| *b);
        st
    }

    pub fn forget(&self, id: &str) {
        self.prev_cpu.lock().unwrap().remove(id);
        self.disk.lock().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_and_disk() {
        let c = Collector::default();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 4096]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 1000]).unwrap();
        let src = Source::Pid(std::process::id());
        let s1 = c.instance("x", "n", &src, &[dir.path()], Some(1 << 30), Some(10_000));
        assert!(s1.mem_bytes.unwrap() > 0 && s1.pids.unwrap() >= 1);
        assert_eq!(s1.disk_bytes, Some(5096));
        // Burn a little CPU, then the second sample has a percentage.
        let t = Instant::now();
        while t.elapsed().as_millis() < 50 {
            std::hint::black_box(0u64.wrapping_add(1));
        }
        let s2 = c.instance("x", "n", &src, &[dir.path()], None, None);
        assert!(s2.cpu_pct.is_some());
        let h = host("n", dir.path());
        assert!(h.mem_total > 0 && h.disk_total > 0 && h.cpus >= 1);
    }

    #[test]
    fn cgroup_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpu.stat"), "usage_usec 1500000\nuser_usec 1000000\n").unwrap();
        std::fs::write(dir.path().join("memory.current"), "52428800\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        std::fs::write(dir.path().join("pids.current"), "7\n").unwrap();
        assert_eq!(cgroup(dir.path()), Some((1_500_000, 52_428_800, None, 7)));
    }
}
