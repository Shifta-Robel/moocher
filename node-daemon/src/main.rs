//! opencode-node — one per machine. See docs/ARCHITECTURE.md §2.

mod cfgstore;
mod gateway;
mod link;
mod manager;
mod oc;
mod runtime;
mod secrets;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ocl_protocol::link::NodeEvent;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "opencode-node", version, about = "opencode-cluster node daemon")]
struct Cli {
    /// Config directory (node.yaml, agents/, providers.yaml, routes.yaml).
    #[arg(long, env = "OCL_CONFIG_DIR", global = true)]
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
}

fn default_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".opencode-cluster")).unwrap_or_else(|| PathBuf::from("/etc/opencode-cluster"))
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
    match cli.cmd {
        Cmd::Schema { out } => {
            std::fs::create_dir_all(&out)?;
            for (name, schema) in ocl_protocol::config::schemas() {
                std::fs::write(out.join(format!("{name}.schema.json")), serde_json::to_vec_pretty(&schema)?)?;
            }
            println!("wrote schemas to {}", out.display());
            Ok(())
        }
        Cmd::Check => check(&dir),
        Cmd::Run => run(dir).await,
    }
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
    let node_name = snap.node.name.clone();
    let data_dir = snap.node.runtime.data_dir.clone().unwrap_or_else(|| dir.join("data"));
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(store::Store::open(&data_dir.join("node.db"))?);
    let cfg = cfgstore::ConfigStore::new(snap);
    let link = link::LinkHandle::new(store.clone());
    let mgr = manager::Manager::new(cfg.clone(), store, rt, link.clone())?;
    tracing::info!(node = %node_name, supervisor = %supervisor, data = %mgr.data_root().display(), "starting");

    mgr.reconcile().await?;

    // Hot reload -> tell the supervisor which agents exist now.
    let mut reloads = cfg.watch()?;
    {
        let link = link.clone();
        tokio::spawn(async move {
            while let Some(s) = reloads.recv().await {
                link.emit(NodeEvent::Agents { agents: s.agent_infos() });
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
    let link_task = tokio::spawn(link.run(supervisor, hello, handler));

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
