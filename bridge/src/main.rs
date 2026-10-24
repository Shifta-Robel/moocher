//! ocl-bridge — the only cluster code that runs inside an agent container.
//!
//! Agent containers run with `--network=none`, so the container's loopback is
//! the only network it has. The bridge relays between that loopback and Unix
//! sockets bind-mounted from the host (`/run/ocl`), in both directions, and
//! then runs the given command (normally `opencode serve`) as its child:
//!
//! ```text
//! ocl-bridge \
//!   --fwd tcp:127.0.0.1:7777=unix:/run/ocl/node.sock \   # agent -> node gateway
//!   --fwd unix:/run/ocl/oc.sock=tcp:127.0.0.1:4096 \     # node -> opencode serve
//!   -- opencode serve --port 4096 --hostname 127.0.0.1
//! ```
//!
//! The bridge exits with the child's exit code and forwards SIGTERM/SIGINT.
//! It has no knowledge of HTTP; it is a byte pipe.

use std::process::ExitCode;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

#[derive(Clone, Debug)]
enum Addr {
    Tcp(String),
    Unix(String),
}

impl Addr {
    fn parse(s: &str) -> Result<Addr, String> {
        if let Some(rest) = s.strip_prefix("tcp:") {
            Ok(Addr::Tcp(rest.to_string()))
        } else if let Some(rest) = s.strip_prefix("unix:") {
            Ok(Addr::Unix(rest.to_string()))
        } else {
            Err(format!("address must start with tcp: or unix: — got {s:?}"))
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn dial(addr: &Addr) -> std::io::Result<Box<dyn Stream>> {
    Ok(match addr {
        Addr::Tcp(a) => Box::new(TcpStream::connect(a).await?),
        Addr::Unix(p) => Box::new(UnixStream::connect(p).await?),
    })
}

async fn pipe(mut inbound: Box<dyn Stream>, target: Addr) {
    // The target may not be up yet (opencode starts after the bridge); retry briefly.
    let mut outbound = None;
    for _ in 0..50 {
        match dial(&target).await {
            Ok(s) => {
                outbound = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    let Some(mut outbound) = outbound else {
        eprintln!("ocl-bridge: target {target:?} unreachable");
        return;
    };
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
}

async fn serve(listen: Addr, target: Addr) -> std::io::Result<()> {
    match listen {
        Addr::Tcp(a) => {
            let l = TcpListener::bind(&a).await?;
            loop {
                let (s, _) = l.accept().await?;
                tokio::spawn(pipe(Box::new(s), target.clone()));
            }
        }
        Addr::Unix(p) => {
            let _ = std::fs::remove_file(&p);
            let l = UnixListener::bind(&p)?;
            // The host-side directory (0700) is the access boundary; the socket
            // itself must be connectable by the host user under userns mapping.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666))?;
            loop {
                let (s, _) = l.accept().await?;
                tokio::spawn(pipe(Box::new(s), target.clone()));
            }
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: ocl-bridge [--fwd LISTEN=TARGET]... -- COMMAND [ARGS]...\n  LISTEN/TARGET: tcp:HOST:PORT | unix:PATH");
    ExitCode::from(2)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fwds = Vec::new();
    let mut cmd: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fwd" if i + 1 < args.len() => {
                let Some((l, t)) = args[i + 1].split_once('=') else { return usage() };
                match (Addr::parse(l), Addr::parse(t)) {
                    (Ok(l), Ok(t)) => fwds.push((l, t)),
                    (Err(e), _) | (_, Err(e)) => {
                        eprintln!("ocl-bridge: {e}");
                        return usage();
                    }
                }
                i += 2;
            }
            "--" => {
                cmd = args[i + 1..].to_vec();
                break;
            }
            "-h" | "--help" => return usage(),
            other => {
                eprintln!("ocl-bridge: unexpected argument {other:?}");
                return usage();
            }
        }
    }

    for (l, t) in fwds {
        tokio::spawn(async move {
            if let Err(e) = serve(l.clone(), t).await {
                eprintln!("ocl-bridge: listener {l:?} failed: {e}");
                std::process::exit(1);
            }
        });
    }

    if cmd.is_empty() {
        // Relay-only mode (used by tests).
        let _ = tokio::signal::ctrl_c().await;
        return ExitCode::SUCCESS;
    }

    let mut child = match tokio::process::Command::new(&cmd[0]).args(&cmd[1..]).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ocl-bridge: cannot start {:?}: {e}", cmd[0]);
            return ExitCode::from(127);
        }
    };
    let pid = child.id().map(|p| p as i32);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
    loop {
        tokio::select! {
            status = child.wait() => {
                // Killed by a signal: 128 + signal, as shells and Podman report it
                // (an OOM kill shows as 137).
                use std::os::unix::process::ExitStatusExt;
                let code = status.ok().map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1))).unwrap_or(1);
                return ExitCode::from(code.clamp(0, 255) as u8);
            }
            _ = term.recv() => forward(pid, "TERM"),
            _ = int.recv() => forward(pid, "INT"),
        }
    }
}

fn forward(pid: Option<i32>, sig: &str) {
    // Avoid a libc dependency: `kill` is in every base image we support.
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill").arg(format!("-{sig}")).arg(pid.to_string()).status();
    }
}
