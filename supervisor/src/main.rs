//! moocher-hub — the hub's control plane. See docs/ARCHITECTURE.md.

mod api;
mod artifacts;
mod attach;
mod auth;
mod db;
mod link;
mod nodes;
mod notify;
mod orch;
mod pki;
mod placement;
mod questions;
mod quotas;
mod recovery;
mod resources;
mod state;
mod templates;
mod retention;
mod tlslink;
mod web;
mod workspaces;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use moocher_protocol::config::{load_yaml, SupervisorConfig};
use moocher_protocol::proto::blobs_server::BlobsServer;
use moocher_protocol::proto::node_link_server::NodeLinkServer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "moocher-hub", version, about = "moocher supervisor")]
struct Cli {
    /// Config directory (supervisor.yaml, workflows/).
    #[arg(long, env = "MOO_CONFIG_DIR", global = true)]
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
    /// Print an event export (`events-YYYYMMDD.jsonl.zst`) as JSON lines, like `zstdcat`.
    EventsCat { file: PathBuf },
    /// Web UI logins (work while the supervisor is stopped).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Create a user or set its password (read from stdin, or prompted).
    Set {
        name: String,
        #[arg(long, default_value = "admin")]
        scope: String,
    },
    List,
    Rm {
        name: String,
    },
    /// Turn TOTP on (prints the secret and an otpauth:// URI for your app) or off.
    Totp {
        name: String,
        #[arg(long)]
        off: bool,
    },
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
    moocher_protocol::config::default_config_dir()
}

pub fn default_link_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("moo")).unwrap_or_else(|| PathBuf::from("/run/moo")).join("supervisor.sock")
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
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("MOO_LOG").unwrap_or_else(|_| "info".into()))
        .with_current_span(false)
        .init();
    let dir = cli.config_dir.unwrap_or_else(default_dir);
    let cfg = load_cfg(&dir)?;
    match cli.cmd {
        Cmd::Init => {
            let db = open_db(&dir, &cfg)?;
            if !db.tokens()?.is_empty() {
                anyhow::bail!("already initialised (tokens exist); use `moocher-hub token create` for more");
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
        Cmd::User { cmd } => {
            let db = open_db(&dir, &cfg)?;
            match cmd {
                UserCmd::Set { name, scope } => {
                    if !moocher_protocol::config::valid_name(&name) {
                        anyhow::bail!("user names are [a-z0-9-]{{1,48}}");
                    }
                    let s = db::Scope::parse(&scope).context("scope must be read, operate or admin")?;
                    let pw = read_password()?;
                    db.put_user(&name, &auth::hash_password(&pw)?, s)?;
                    db.delete_user_sessions(&name)?;
                    db.audit("cli", "user.set", &name, &serde_json::json!({"scope": scope}), true)?;
                    println!("user {name} ({scope}) saved; log in at the dashboard");
                }
                UserCmd::List => {
                    for (n, s, t, l) in db.users() {
                        println!("{n}\t{s}\t{}{}", if t { "totp" } else { "password" }, if l > moocher_protocol::model::now_ms() { "\tLOCKED" } else { "" });
                    }
                }
                UserCmd::Rm { name } => {
                    let ok = db.delete_user(&name)?;
                    db.audit("cli", "user.delete", &name, &serde_json::json!({}), ok)?;
                    println!("{}", if ok { "removed" } else { "no such user" });
                }
                UserCmd::Totp { name, off } => {
                    if off {
                        db.set_totp(&name, None)?;
                        db.audit("cli", "user.totp", &name, &serde_json::json!({"enabled": false}), true)?;
                        println!("TOTP off for {name}");
                    } else {
                        let secret = auth::base32(&rand::random::<[u8; 20]>());
                        if !db.set_totp(&name, Some(&secret))? {
                            anyhow::bail!("no such user {name}");
                        }
                        db.audit("cli", "user.totp", &name, &serde_json::json!({"enabled": true}), true)?;
                        println!("TOTP on for {name}. Add it to your authenticator app:\n  secret: {secret}\n  {}", auth::otpauth_uri(&name, &secret));
                    }
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
        Cmd::EventsCat { file } => {
            let f = std::fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
            // A file is several zstd frames; the decoder reads them as one stream.
            std::io::copy(&mut zstd::stream::read::Decoder::new(f)?, &mut std::io::stdout().lock())?;
            Ok(())
        }
        Cmd::Run => run(dir, cfg).await,
    }
}

/// One line from stdin, or a hidden prompt on a terminal.
fn read_password() -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    let read = |prompt: &str| -> Result<String> {
        if tty {
            eprint!("{prompt}");
            std::io::stderr().flush()?;
            // Hide what is typed.
            let _ = std::process::Command::new("stty").arg("-echo").stdin(std::process::Stdio::inherit()).status();
        }
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        if tty {
            let _ = std::process::Command::new("stty").arg("echo").stdin(std::process::Stdio::inherit()).status();
            eprintln!();
        }
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    };
    let pw = read("password: ")?;
    if tty && read("again: ")? != pw {
        anyhow::bail!("the passwords differ");
    }
    Ok(pw)
}

async fn run(dir: PathBuf, cfg: SupervisorConfig) -> Result<()> {
    let db = open_db(&dir, &cfg)?;
    if db.tokens()?.is_empty() {
        anyhow::bail!("no API tokens exist; run `moocher-hub init` first");
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
            s.report.state = moocher_protocol::model::SessionState::Unreachable;
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
        .add_service(BlobsServer::new(artifacts::BlobService { sup: sup.clone() }))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds));

    // Remote nodes: mTLS on TCP.
    let remote = match sup.cfg.link_listen.clone() {
        None => None,
        Some(listen) => {
            let ca = pki::Ca::open(&data.join("pki"))?;
            let names = if sup.cfg.public_names.is_empty() { pki::default_names() } else { sup.cfg.public_names.clone() };
            let (chain, key) = ca.link_cert(&names)?;
            let ca_der = moocher_protocol::tls::certs_from_pem(&ca.cert_pem)?;
            let tls = moocher_protocol::tls::link_server(moocher_protocol::tls::certs_from_pem(&chain)?, moocher_protocol::tls::key_from_pem(&key)?, &ca_der)?;
            tracing::info!(listen = %listen, names = ?names, ca_fingerprint = %ca.fingerprint(), "remote node link (mTLS) enabled");
            let _ = sup.ca.set(ca);
            let incoming = tlslink::incoming(&listen, tls).await?;
            Some(
                tonic::transport::Server::builder()
                    .http2_keepalive_interval(Some(std::time::Duration::from_secs(20)))
                    .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
                    .add_service(NodeLinkServer::new(link::LinkService { sup: sup.clone() }))
                    .add_service(BlobsServer::new(artifacts::BlobService { sup: sup.clone() }))
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
    let loopback = api_listen.starts_with("127.") || api_listen.starts_with("localhost") || api_listen.starts_with("[::1]");
    let tls = match sup.cfg.tls.clone() {
        Some(t) => Some(web::start_tls(&sup, &t, &data).await?),
        None => {
            if !loopback && sup.cfg.trusted_proxies.is_empty() {
                tracing::warn!(api_listen, "the dashboard/API is served over plain HTTP beyond loopback; configure `tls:` (files or ACME) or a TLS proxy (`trusted_proxies:`)");
            }
            None
        }
    };
    let api_tls = tls.clone().map(|st| web::server_config(st, &[b"h2", b"http/1.1"])).transpose()?;
    if let Some(attach_listen) = sup.cfg.attach_listen.clone() {
        let l = tokio::net::TcpListener::bind(&attach_listen).await.with_context(|| format!("binding attach_listen {attach_listen}"))?;
        let attach_tls = tls.clone().map(|st| web::server_config(st, &[b"http/1.1"])).transpose()?;
        tracing::info!(listen = %attach_listen, tls = attach_tls.is_some(), "Open in OpenCode listener");
        tokio::spawn(attach::serve(sup.clone(), l, attach_tls));
    }
    if sup.db.users().is_empty() {
        tracing::warn!("no web UI user yet: create one with `moocher-hub user set <name>`");
    }
    let api = web::serve(listener, api::router(sup.clone()), api_tls);

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
    tokio::spawn(sup.clone().housekeeping_loop());
    sup.audit(&state::Actor::System, "supervisor.start", "-", serde_json::json!({"version": moocher_protocol::VERSION}), true);
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
