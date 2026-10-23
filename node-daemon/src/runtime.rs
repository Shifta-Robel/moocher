//! Where OpenCode runs. `podman` is the production runtime; `process` runs the
//! same bridge + opencode pair directly on the host (dev/CI only, no isolation).

use anyhow::{bail, Context, Result};
use ocl_protocol::config::{ContainerSpec, Egress, RuntimeKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

/// UID of the `agent` user inside the image.
pub const CONTAINER_UID: u32 = 1000;
/// Loopback port inside the container where the node gateway is reachable.
pub const GATEWAY_PORT: u16 = 7777;
const OPENCODE_PORT: u16 = 4096;

#[derive(Debug, Clone)]
pub struct InstancePaths {
    /// Host dir with node.sock / oc.sock (mounted at /run/ocl).
    pub run: PathBuf,
    /// Generated opencode config (mounted read-only at /etc/ocl).
    pub etc: PathBuf,
    /// OpenCode state, persisted so sessions survive container restarts.
    pub home: PathBuf,
    pub workspace: PathBuf,
}

impl InstancePaths {
    pub fn node_sock(&self) -> PathBuf {
        self.run.join("node.sock")
    }
    pub fn oc_sock(&self) -> PathBuf {
        self.run.join("oc.sock")
    }
}

pub struct LaunchSpec<'a> {
    pub node: &'a str,
    pub id: &'a str,
    pub agent: &'a str,
    pub image: &'a str,
    pub container: &'a ContainerSpec,
    pub paths: &'a InstancePaths,
    pub password: &'a str,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub id: String,
    pub running: bool,
}

pub struct Runtime {
    pub kind: RuntimeKind,
    podman: String,
    rootless: bool,
    opencode_bin: String,
    bridge_bin: PathBuf,
    /// process runtime: id -> (child pid, gateway port)
    procs: Mutex<HashMap<String, (u32, u16)>>,
    /// podman: id -> cgroup v2 directory (looked up once)
    cgroups: Mutex<HashMap<String, PathBuf>>,
}

pub fn container_name(id: &str) -> String {
    format!("ocl-{id}")
}

impl Runtime {
    pub fn new(kind: RuntimeKind, podman: &str, opencode_bin: Option<String>) -> Result<Runtime> {
        let rootless = !is_root();
        let bridge_bin = std::env::var_os("OCL_BRIDGE").map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ocl-bridge"))).unwrap_or_else(|| "ocl-bridge".into())
        });
        if kind == RuntimeKind::Process {
            tracing::warn!("runtime=process: sessions run WITHOUT container isolation (dev/test only)");
        }
        Ok(Runtime {
            kind,
            podman: podman.into(),
            rootless,
            opencode_bin: opencode_bin.unwrap_or_else(|| "opencode".into()),
            bridge_bin,
            procs: Mutex::new(HashMap::new()),
            cgroups: Mutex::new(HashMap::new()),
        })
    }

    /// Path of the workspace as seen by OpenCode.
    pub fn workspace_in_runtime(&self, paths: &InstancePaths) -> String {
        match self.kind {
            RuntimeKind::Podman => "/workspace".into(),
            RuntimeKind::Process => paths.workspace.display().to_string(),
        }
    }

    /// Gateway base URL as seen from inside the runtime. For `process` a free
    /// port is chosen per instance (call before writing the opencode config).
    pub fn gateway_port(&self, id: &str) -> Result<u16> {
        match self.kind {
            RuntimeKind::Podman => Ok(GATEWAY_PORT),
            RuntimeKind::Process => {
                if let Some((_, p)) = self.procs.lock().unwrap().get(id) {
                    return Ok(*p);
                }
                let p = free_port()?;
                self.procs.lock().unwrap().insert(id.to_string(), (0, p));
                Ok(p)
            }
        }
    }

    /// Prepare host dirs with ownership the container user can use.
    pub fn prepare_dirs(&self, paths: &InstancePaths) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in [&paths.run, &paths.etc, &paths.home, &paths.workspace] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        std::fs::set_permissions(&paths.run, std::fs::Permissions::from_mode(0o700))?;
        if self.kind == RuntimeKind::Podman && !self.rootless {
            // Rootful podman: container uid 1000 is host uid 1000.
            for d in [&paths.run, &paths.home, &paths.workspace] {
                chown_r(d, CONTAINER_UID)?;
            }
        }
        Ok(())
    }

    pub async fn launch(&self, s: &LaunchSpec<'_>) -> Result<()> {
        match self.kind {
            RuntimeKind::Podman => self.launch_podman(s).await,
            RuntimeKind::Process => self.launch_process(s),
        }
    }

    async fn launch_podman(&self, s: &LaunchSpec<'_>) -> Result<()> {
        let name = container_name(s.id);
        let _ = self.podman(&["rm", "-f", &name]).await;
        let mut a: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone()];
        for (k, v) in [("ocl.node", s.node), ("ocl.session", s.id), ("ocl.agent", s.agent)] {
            a.push("--label".into());
            a.push(format!("{k}={v}"));
        }
        if s.container.egress == Egress::None {
            a.push("--network=none".into());
        }
        a.extend(
            [
                "--cap-drop=all",
                "--security-opt=no-new-privileges",
                "--read-only",
                "--init",
            ]
            .map(String::from),
        );
        a.push("--tmpfs".into());
        a.push(format!("/tmp:rw,size={}", s.container.tmp));
        a.push(format!("--memory={}", s.container.memory));
        a.push(format!("--cpus={}", s.container.cpus));
        a.push(format!("--pids-limit={}", s.container.pids));
        if self.rootless {
            a.push(format!("--userns=keep-id:uid={CONTAINER_UID},gid={CONTAINER_UID}"));
        }
        for (k, v) in opencode_env(s.password, "/etc/ocl") {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        let p = s.paths;
        for (host, ctr, ro) in [(&p.run, "/run/ocl", false), (&p.etc, "/etc/ocl", true), (&p.home, "/home/agent", false), (&p.workspace, "/workspace", false)] {
            a.push("-v".into());
            a.push(format!("{}:{}{}", host.display(), ctr, if ro { ":ro" } else { "" }));
        }
        a.push(s.image.into());
        a.extend([
            "--fwd".into(),
            format!("tcp:127.0.0.1:{GATEWAY_PORT}=unix:/run/ocl/node.sock"),
            "--fwd".into(),
            format!("unix:/run/ocl/oc.sock=tcp:127.0.0.1:{OPENCODE_PORT}"),
            "--".into(),
            "opencode".into(),
            "serve".into(),
            "--port".into(),
            OPENCODE_PORT.to_string(),
            "--hostname".into(),
            "127.0.0.1".into(),
        ]);
        let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        self.podman(&refs).await.context("podman run")?;
        Ok(())
    }

    fn launch_process(&self, s: &LaunchSpec<'_>) -> Result<()> {
        let gw = self.gateway_port(s.id)?;
        let oc = free_port()?;
        let p = s.paths;
        let log = std::fs::File::create(p.home.join("opencode.log"))?;
        let mut cmd = std::process::Command::new(&self.bridge_bin);
        cmd.arg("--fwd")
            .arg(format!("tcp:127.0.0.1:{gw}=unix:{}", p.node_sock().display()))
            .arg("--fwd")
            .arg(format!("unix:{}=tcp:127.0.0.1:{oc}", p.oc_sock().display()))
            .arg("--")
            .arg(&self.opencode_bin)
            .args(["serve", "--port", &oc.to_string(), "--hostname", "127.0.0.1"])
            .current_dir(&p.workspace)
            .env("HOME", &p.home)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        for (k, v) in opencode_env(s.password, &p.etc.display().to_string()) {
            cmd.env(k, v);
        }
        let child = cmd.spawn().with_context(|| format!("starting {}", self.bridge_bin.display()))?;
        self.procs.lock().unwrap().insert(s.id.to_string(), (child.id(), gw));
        // Reap in the background so it never becomes a zombie.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        Ok(())
    }

    /// Where to read an instance's resource usage from.
    pub async fn stats_source(&self, id: &str) -> crate::stats::Source {
        match self.kind {
            RuntimeKind::Process => match self.procs.lock().unwrap().get(id) {
                Some((pid, _)) if *pid != 0 => crate::stats::Source::Pid(*pid),
                _ => crate::stats::Source::None,
            },
            RuntimeKind::Podman => {
                if let Some(d) = self.cgroups.lock().unwrap().get(id) {
                    return crate::stats::Source::Cgroup(d.clone());
                }
                let Ok(out) = self.podman(&["inspect", "--format", "{{.State.CgroupPath}}", &container_name(id)]).await else {
                    return crate::stats::Source::None;
                };
                let rel = out.trim().trim_start_matches('/');
                if rel.is_empty() {
                    return crate::stats::Source::None;
                }
                let dir = Path::new("/sys/fs/cgroup").join(rel);
                self.cgroups.lock().unwrap().insert(id.to_string(), dir.clone());
                crate::stats::Source::Cgroup(dir)
            }
        }
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        self.cgroups.lock().unwrap().remove(id);
        match self.kind {
            RuntimeKind::Podman => {
                self.podman(&["stop", "-t", "10", &container_name(id)]).await?;
            }
            RuntimeKind::Process => {
                if let Some((pid, _)) = self.procs.lock().unwrap().remove(id) {
                    if pid != 0 {
                        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        if self.kind == RuntimeKind::Podman {
            let _ = self.podman(&["rm", "-f", &container_name(id)]).await;
        }
        Ok(())
    }

    pub async fn list(&self, node: &str) -> Result<Vec<Found>> {
        if self.kind == RuntimeKind::Process {
            return Ok(self.procs.lock().unwrap().keys().map(|id| Found { id: id.clone(), running: true }).collect());
        }
        let out = self.podman(&["ps", "-a", "--filter", &format!("label=ocl.node={node}"), "--format", "json"]).await?;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or(serde_json::Value::Array(vec![]));
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(Found {
                            id: c["Labels"]["ocl.session"].as_str()?.to_string(),
                            running: c["State"].as_str() == Some("running"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn start_existing(&self, id: &str) -> Result<()> {
        if self.kind == RuntimeKind::Podman {
            self.podman(&["start", &container_name(id)]).await?;
        }
        Ok(())
    }

    /// Run a shell command in the session's workspace. Returns (exit code, combined output tail).
    pub async fn exec(&self, id: &str, workspace: &Path, command: &str, timeout: Duration) -> Result<(i32, String)> {
        let mut cmd = match self.kind {
            RuntimeKind::Podman => {
                let mut c = Command::new(&self.podman);
                c.args(["exec", "-w", "/workspace", &container_name(id), "sh", "-c", command]);
                c
            }
            RuntimeKind::Process => {
                let mut c = Command::new("sh");
                c.args(["-c", command]).current_dir(workspace);
                c
            }
        };
        cmd.kill_on_drop(true);
        let out = tokio::time::timeout(timeout, cmd.output()).await.map_err(|_| anyhow::anyhow!("check timed out after {timeout:?}"))??;
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let mut tail: String = text.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        let code = out.status.code().unwrap_or(-1);
        // With no container, checks run with the host's tools; a missing tool
        // looks like a failed check (e.g. pytest not installed on the host).
        if self.kind == RuntimeKind::Process && (code == 127 || tail.contains("No module named")) {
            tail.push_str("\n[opencode-cluster] runtime=process runs checks on the host: install the tool there, or use the podman runtime (the agent image has it).");
        }
        Ok((code, tail))
    }

    async fn podman(&self, args: &[&str]) -> Result<String> {
        let out = tokio::time::timeout(Duration::from_secs(120), Command::new(&self.podman).args(args).output())
            .await
            .map_err(|_| anyhow::anyhow!("podman {} timed out", args.first().unwrap_or(&"")))?
            .with_context(|| format!("running {}", self.podman))?;
        if !out.status.success() {
            bail!("podman {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

fn opencode_env(password: &str, config_dir: &str) -> Vec<(&'static str, String)> {
    vec![
        ("OPENCODE_SERVER_PASSWORD", password.to_string()),
        // OPENCODE_CONFIG_DIR is read at boot; OPENCODE_CONFIG loads lazily and
        // makes the first prompt fail with ModelUnavailable (found in the Phase 1 spike).
        ("OPENCODE_CONFIG_DIR", config_dir.to_string()),
        // The agent must not be able to rewrite its own permissions/providers
        // by writing an opencode.json into the workspace.
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".into()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1".into()),
        ("OPENCODE_DISABLE_CLAUDE_CODE", "1".into()),
    ]
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn is_root() -> bool {
    std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0).unwrap_or(false)
}

fn chown_r(path: &Path, uid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(uid))?;
    if path.is_dir() {
        for e in std::fs::read_dir(path)? {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            chown_r(&e.path(), uid)?;
        }
    }
    Ok(())
}
