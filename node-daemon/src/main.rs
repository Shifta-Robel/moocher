//! moocher-node — one per machine. See docs/ARCHITECTURE.md §2.

mod attach;
mod cas;
mod cfgstore;
mod disk_quota;
mod egress;
mod egress_proxy;
mod gateway;
mod gitops;
mod link;
mod manager;
mod oneshot;
mod pace;
mod pool;
mod obs;
mod oc;
mod pki;
mod quota;
mod units;
mod relay;
mod runtime;
mod secrets;
mod stats;
mod store;
mod usage;
mod wsops;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use moocher_protocol::link::NodeEvent;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "moocher-node", version, about = "moocher node daemon")]
struct Cli {
    /// Config directory (node.yaml, agents/, providers.yaml, routes.yaml).
    #[arg(long, env = "MOO_CONFIG_DIR", global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Run,
    /// Validate the config directory and report per-agent problems.
    Check,
    /// Write JSON Schemas for all config files (for editor validation).
    Schema {
        #[arg(long, default_value = "schemas")]
        out: PathBuf,
    },
    /// Enroll this machine with a hub using a one-time join token
    /// (from `moo node invite <name>` or the web UI).
    Join {
        /// The hub's link address, e.g. https://hub.example:7443
        url: String,
        #[arg(long, env = "MOO_JOIN_TOKEN", hide_env_values = true)]
        token: String,
    },
}

fn default_dir() -> PathBuf {
    moocher_protocol::config::default_config_dir()
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
    match cli.cmd {
        Cmd::Schema { out } => {
            std::fs::create_dir_all(&out)?;
            for (name, schema) in moocher_protocol::config::schemas() {
                std::fs::write(out.join(format!("{name}.schema.json")), serde_json::to_vec_pretty(&schema)?)?;
            }
            println!("wrote schemas to {}", out.display());
            Ok(())
        }
        Cmd::Check => check(&dir),
        Cmd::Join { url, token } => join(&dir, &url, &token).await,
        Cmd::Run => run(dir).await,
    }
}

fn pki_dir(dir: &std::path::Path, node: Option<&moocher_protocol::config::NodeConfig>) -> PathBuf {
    node.and_then(|n| n.pki_dir.clone()).unwrap_or_else(|| dir.join("pki"))
}

async fn join(dir: &std::path::Path, url: &str, token: &str) -> Result<()> {
    let node_yaml = dir.join("node.yaml");
    let existing: Option<moocher_protocol::config::NodeConfig> = if node_yaml.exists() { Some(moocher_protocol::config::load_yaml(&node_yaml)?) } else { None };
    let pki = pki::Pki::new(pki_dir(dir, existing.as_ref()));
    let e = pki::join(&pki, url, token).await?;
    println!("enrolled as node {:?} (role {}), certificate valid until {}", e.node, e.role, fmt_ts(e.expires_at));
    match existing {
        None => {
            std::fs::create_dir_all(dir)?;
            let body = if e.role == "relay" {
                format!("name: {}\nsupervisor: {url}\nrole: relay\n", e.node)
            } else {
                format!("name: {}\nsupervisor: {url}\nmax_sessions: 4\nruntime: {{ kind: podman }}\n", e.node)
            };
            std::fs::write(&node_yaml, body)?;
            println!("wrote {}", node_yaml.display());
        }
        Some(n) => {
            if n.name != e.node {
                println!("WARNING: {} names this node {:?} but it enrolled as {:?}; set `name: {}` before starting.", node_yaml.display(), n.name, e.node, e.node);
            }
            if n.supervisor != url {
                println!("WARNING: {} has `supervisor: {}`; set it to `{url}`.", node_yaml.display(), n.supervisor);
            }
        }
    }
    if e.status == "pending" {
        println!("The node is pending approval. On the hub: moo node approve {}", e.node);
    }
    println!("Start it with: moocher-node --config-dir {} run   (or enable the systemd unit)", dir.display());
    Ok(())
}

fn fmt_ts(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs / 86_400;
    // Civil date from days since epoch (Howard Hinnant's algorithm), UTC.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} UTC")
}

fn check(dir: &std::path::Path) -> Result<()> {
    let snap = cfgstore::load(dir)?;
    println!("node {} -> supervisor {}", snap.node.name, snap.node.supervisor);
    let mut bad = 0;
    for (f, e) in &snap.file_errors {
        println!("  ✗ {f}: {e}");
        bad += 1;
    }
    for a in snap.agents.values() {
        let ok = a.errors.is_empty() && a.secret_ok;
        println!(
            "  {} agent {:<16} {}/{}{}",
            if ok { "✓" } else { "✗" },
            a.cfg.name,
            a.cfg.provider.id,
            a.cfg.provider.model,
            if a.secret_ok { String::new() } else { "  [secret does not resolve]".into() }
        );
        for e in &a.errors {
            println!("      - {e}");
        }
        if !ok {
            bad += 1;
        }
    }
    if bad > 0 {
        anyhow::bail!("{bad} problem(s)");
    }
    Ok(())
}

async fn run(dir: PathBuf) -> Result<()> {
    let snap = cfgstore::load(&dir).with_context(|| format!("loading {}", dir.display()))?;
    for (f, e) in &snap.file_errors {
        tracing::warn!(file = %f, error = %e, "config file ignored");
    }
    let rt = runtime::Runtime::new(snap.node.runtime.kind, &snap.node.runtime.podman, snap.node.runtime.opencode_bin.clone())?;
    let supervisor = snap.node.supervisor.clone();
    let relay_only = snap.node.role == moocher_protocol::config::NodeRole::Relay;
    let pki = Arc::new(pki::Pki::new(pki_dir(&dir, Some(&snap.node))));
    if supervisor.starts_with("https://") {
        pki.client_config().context("loading the node certificate")?;
    }
    let node_name = snap.node.name.clone();
    let data_dir = snap.node.runtime.data_dir.clone().unwrap_or_else(|| dir.join("data"));
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(store::Store::open(&data_dir.join("node.db"))?);
    let cfg = cfgstore::ConfigStore::new(snap);
    let link = link::LinkHandle::new(store.clone());
    let dialer = relay::Dialer::start(node_name.clone(), link.clone()).await?;
    let egress = Arc::new(egress::Egress::new(cfg.clone(), Some(dialer), link.clone()));
    let quotas = Arc::new(quota::Quotas::new(&node_name, store.clone()));
    let cache_max = moocher_protocol::model::parse_size(&cfg.get().node.artifact_cache).unwrap_or(2 << 30);
    let cas = Arc::new(cas::Cas::new(data_dir.join("cas"), link.clone(), &node_name, cache_max)?);
    let files = Arc::new(wsops::WsOps::new(cas, store.clone(), data_dir.clone(), rt.chown_target(), rt.git_runner(cfg.clone())));
    let obs = obs::Observer::new(&node_name, store.clone());
    let units = Arc::new(units::Units::new(&node_name, store.clone()));
    let pool = Arc::new(pool::Pool::default());
    let deps = gateway::GatewayDeps { pacer: Default::default(), pool: pool.clone(), cfg: cfg.clone(), link: link.clone(), egress: egress.clone(), quotas: quotas.clone(), units: units.clone(), files, obs: obs.clone() };
    let mgr = manager::Manager::new(cfg.clone(), store, rt, link.clone(), deps)?;
    tracing::info!(node = %node_name, supervisor = %supervisor, data = %mgr.data_root().display(), "starting");

    if relay_only {
        tracing::info!("role=relay: this node runs no agents and only relays egress traffic");
    } else {
        mgr.reconcile().await?;
    }

    // Hot reload -> tell the supervisor which agents exist now; routes may have changed.
    let reprobe = Arc::new(tokio::sync::Notify::new());
    let mut reloads = cfg.watch()?;
    {
        let (link, quotas, reprobe) = (link.clone(), quotas.clone(), reprobe.clone());
        tokio::spawn(async move {
            while let Some(s) = reloads.recv().await {
                let agents = if s.node.role == moocher_protocol::config::NodeRole::Relay { vec![] } else { s.agent_infos() };
                link.emit(NodeEvent::Agents { agents });
                quotas.touch();
                reprobe.notify_one();
            }
        });
    }
    // Egress IPs: at start, every 30 min, on config change and when the hub sees a new address.
    if !relay_only {
        let (egress, link, reprobe) = (egress.clone(), link.clone(), reprobe.clone());
        tokio::spawn(async move {
            loop {
                egress.probe().await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1800)) => {}
                    _ = reprobe.notified() => {}
                    _ = link.ip_changed() => {}
                }
            }
        });
    }
    // Resource samples every 10 s (best effort; the hub keeps the history).
    {
        let (mgr, link) = (mgr.clone(), link.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                iv.tick().await;
                let (host, instances) = mgr.collect_stats().await;
                link.send_live(&moocher_protocol::link::NodeMsg::Stats { host, instances }).await;
            }
        });
    }
    // Provider latency per route: report changes once a minute.
    {
        let (egress, link) = (egress.clone(), link.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                iv.tick().await;
                if let Some(stats) = egress.take_stats() {
                    link.emit(NodeEvent::RouteStats { stats });
                }
            }
        });
    }
    // What keyproxy saw of providers: once a minute when something changed.
    {
        let (obs, link) = (obs.clone(), link.clone());
        let every = std::env::var("MOO_OBS_INTERVAL_S").ok().and_then(|v| v.parse().ok()).unwrap_or(60u64).max(1);
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(every));
            loop {
                iv.tick().await;
                if let Some((rows, events)) = obs.take() {
                    link.emit(NodeEvent::Observations { rows, events });
                }
            }
        });
    }
    // Quota buckets: persist and report changes (at most every 3 s).
    {
        let (quotas, units, cfg, link) = (quotas.clone(), units.clone(), cfg.clone(), link.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                iv.tick().await;
                pool.report(&link, &cfg.get());
                let (a, b) = (quotas.flush(), units.flush());
                if a || b {
                    let s = cfg.get();
                    let limits = quota::limits_by_bucket(&s.providers);
                    let specs = s.providers.iter().map(|(k, p)| (k.clone(), quota::spec_for(p).units)).collect();
                    let mut buckets = quotas.snapshot(&limits, &s.overrides.quotas);
                    buckets.extend(units.snapshot(&specs));
                    link.emit(NodeEvent::Quotas { buckets });
                }
            }
        });
    }
    {
        let mgr = mgr.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                iv.tick().await;
                mgr.tick().await;
            }
        });
    }

    let hello: link::HelloFn = {
        let mgr = mgr.clone();
        Arc::new(move || {
            let mgr = mgr.clone();
            Box::pin(async move { mgr.hello().await })
        })
    };
    let handler: link::CommandHandler = {
        let mgr = mgr.clone();
        Arc::new(move |cmd| {
            let mgr = mgr.clone();
            Box::pin(async move { mgr.handle(cmd).await })
        })
    };
    let link_task = tokio::spawn(link.run(supervisor, pki, hello, handler));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm() => {}
        _ = link_task => {}
    }
    // Sessions keep running in their containers; a restart re-attaches.
    tracing::info!("shutting down (containers keep running)");
    Ok(())
}

async fn sigterm() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    }
}
