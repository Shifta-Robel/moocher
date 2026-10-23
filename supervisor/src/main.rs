//! ocl-supervisor — the hub's control plane. See docs/ARCHITECTURE.md.

mod api;
mod db;
mod link;
mod nodes;
mod orch;
mod pki;
mod placement;
mod quotas;
mod resources;
mod state;
mod templates;
mod tlslink;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ocl_protocol::config::{load_yaml, SupervisorConfig};
use ocl_protocol::proto::node_link_server::NodeLinkServer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "ocl-supervisor", version, about = "opencode-cluster supervisor")]
struct Cli {
    /// Config directory (supervisor.yaml, workflows/).
    #[arg(long, env = "OCL_CONFIG_DIR", global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database and an admin API token (printed once and saved to <config>/cli-token).
    Init,
    /// Run the supervisor.
    Run,
    /// Manage API tokens offline (works while the supervisor is stopped).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Verify the audit log hash chain.
    AuditVerify,
}

#[derive(Subcommand)]
enum TokenCmd {
    Create {
        name: String,
        #[arg(long, default_value = "operate")]
        scope: String,
    },
    List,
    Revoke {
        name: String,
    },
}

fn default_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".opencode-cluster")).unwrap_or_else(|| PathBuf::from("/etc/opencode-cluster"))
}

pub fn default_link_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("ocl")).unwrap_or_else(|| PathBuf::from("/run/ocl")).join("supervisor.sock")
}

fn load_cfg(dir: &Path) -> Result<SupervisorConfig> {
    let p = dir.join("supervisor.yaml");
    if p.exists() {
        load_yaml(&p)
    } else {
        Ok(SupervisorConfig::default())
    }
}

fn open_db(dir: &Path, cfg: &SupervisorConfig) -> Result<db::Db> {
    let data = cfg.data_dir.clone().unwrap_or_else(|| dir.join("supervisor"));
    std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))?;
    }
    db::Db::open(&data.join("supervisor.db"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("OCL_LOG").unwrap_or_else(|_| "info".into()))
        .with_current_span(false)
        .init();
    let dir = cli.config_dir.unwrap_or_else(default_dir);
    let cfg = load_cfg(&dir)?;
    match cli.cmd {
        Cmd::Init => {
            let db = open_db(&dir, &cfg)?;
            if !db.tokens()?.is_empty() {
                anyhow::bail!("already initialised (tokens exist); use `ocl-supervisor token create` for more");
            }
            let t = db.create_token("admin", db::Scope::Admin)?;
            db.audit("system", "init", "-", &serde_json::json!({}), true)?;
            let path = dir.join("cli-token");
            std::fs::write(&path, format!("{t}\n"))?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("admin token (also saved to {}):\n{t}", path.display());
            Ok(())
        }
        Cmd::Token { cmd } => {
            let db = open_db(&dir, &cfg)?;
            match cmd {
                TokenCmd::Create { name, scope } => {
                    let s = db::Scope::parse(&scope).context("scope must be read, operate or admin")?;
                    let t = db.create_token(&name, s)?;
                    db.audit("cli", "token.create", &name, &serde_json::json!({"scope": scope}), true)?;
                    println!("{t}");
                }
                TokenCmd::List => {
                    for (n, s, _) in db.tokens()? {
                        println!("{n}\t{s}");
                    }
                }
                TokenCmd::Revoke { name } => {
                    let ok = db.revoke_token(&name)?;
                    db.audit("cli", "token.revoke", &name, &serde_json::json!({}), ok)?;
                    println!("{}", if ok { "revoked" } else { "no such token" });
                }
            }
            Ok(())
        }
        Cmd::AuditVerify => {
            let db = open_db(&dir, &cfg)?;
            let n = db.audit_verify()?;
            println!("audit chain OK ({n} entries)");
            Ok(())
        }
        Cmd::Run => run(dir, cfg).await,
    }
}

async fn run(dir: PathBuf, cfg: SupervisorConfig) -> Result<()> {
    let db = open_db(&dir, &cfg)?;
    if db.tokens()?.is_empty() {
        anyhow::bail!("no API tokens exist; run `ocl-supervisor init` first");
    }
    // Every node is "disconnected" until it attaches again.
    for mut n in db.nodes() {
        if n.connected {
            n.connected = false;
            db.put_node(&n)?;
        }
    }
    let sock = cfg.link_socket.clone().unwrap_or_else(default_link_socket);
    let api_listen = cfg.api_listen.clone();
    let data = cfg.data_dir.clone().unwrap_or_else(|| dir.join("supervisor"));
    let sup = state::Sup::new(db, cfg, dir.clone());
    sup.seed_allowlist();
    for s in sup.db.sessions(false) {
        if s.report.state.is_live() {
            // Until its node re-attaches we cannot know; the hello reconciles it.
            let mut s = s;
            s.report.state = ocl_protocol::model::SessionState::Unreachable;
            sup.db.put_session(&s)?;
        }
    }

    // Node link on a unix socket (same machine). Phase 2 adds mTLS over TCP.
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&sock);
    let uds = tokio::net::UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let link = tonic::transport::Server::builder()
        .add_service(NodeLinkServer::new(link::LinkService { sup: sup.clone() }))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds));

    // Remote nodes: mTLS on TCP.
    let remote = match sup.cfg.link_listen.clone() {
        None => None,
        Some(listen) => {
            let ca = pki::Ca::open(&data.join("pki"))?;
            let names = if sup.cfg.public_names.is_empty() { pki::default_names() } else { sup.cfg.public_names.clone() };
            let (chain, key) = ca.link_cert(&names)?;
            let ca_der = ocl_protocol::tls::certs_from_pem(&ca.cert_pem)?;
            let tls = ocl_protocol::tls::link_server(ocl_protocol::tls::certs_from_pem(&chain)?, ocl_protocol::tls::key_from_pem(&key)?, &ca_der)?;
            tracing::info!(listen = %listen, names = ?names, ca_fingerprint = %ca.fingerprint(), "remote node link (mTLS) enabled");
            let _ = sup.ca.set(ca);
            let incoming = tlslink::incoming(&listen, tls).await?;
            Some(
                tonic::transport::Server::builder()
                    .http2_keepalive_interval(Some(std::time::Duration::from_secs(20)))
                    .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
                    .add_service(NodeLinkServer::new(link::LinkService { sup: sup.clone() }))
                    .serve_with_incoming(incoming),
            )
        }
    };
    let remote = async move {
        match remote {
            Some(f) => f.await,
            None => std::future::pending().await,
        }
    };

    let listener = tokio::net::TcpListener::bind(&api_listen).await.with_context(|| format!("binding {api_listen}"))?;
    if !api_listen.starts_with("127.") && !api_listen.starts_with("localhost") && !api_listen.starts_with("[::1]") {
        tracing::warn!(api_listen, "API is listening beyond loopback without TLS; put a TLS reverse proxy in front (built-in TLS: Phase 4)");
    }
    let api = axum::serve(listener, api::router(sup.clone()));

    {
        let sup = sup.clone();
        tokio::spawn(async move {
            let mut idle_since = None;
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                iv.tick().await;
                sup.schedule(&mut idle_since).await;
            }
        });
    }
    tokio::spawn(sup.clone().watch_library());
    sup.audit(&state::Actor::System, "supervisor.start", "-", serde_json::json!({"version": ocl_protocol::VERSION}), true);
    tracing::info!(api = %api_listen, link = %sock.display(), "supervisor running");
    let sup2: Arc<state::Sup> = sup.clone();
    tokio::select! {
        r = link => r.context("node link server")?,
        r = remote => r.context("remote node link server")?,
        r = api => r.context("api server")?,
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm() => {}
    }
    sup2.audit(&state::Actor::System, "supervisor.stop", "-", serde_json::json!({}), true);
    Ok(())
}

async fn sigterm() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    }
}
