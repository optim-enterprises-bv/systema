use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use systema_hub::{Hub, HubConfig, ensure_identity};
use tokio::sync::watch;

/// Public bootstrap + relay for the Systema swarm. No agents, no kernel.
#[derive(Parser, Debug)]
#[command(name = "systema-hub", version)]
struct Cli {
    /// State directory: identity.key (created if missing) and an optional hub.toml.
    #[arg(long, env = "SYSTEMA_HUB_HOME", default_value = "/var/lib/systema-hub")]
    home: PathBuf,
    /// Listen address (repeatable); replaces the config file's `listen`.
    #[arg(long)]
    listen: Vec<String>,
    /// Public address to advertise (repeatable); replaces the config file's `external_addrs`.
    #[arg(long)]
    external: Vec<String>,
    /// Another hub to peer with (repeatable); replaces the config file's `bootstrap`.
    #[arg(long)]
    bootstrap: Vec<String>,
    /// Do not act as a circuit relay.
    #[arg(long)]
    no_relay: bool,
    /// Print the peer id (creating the identity if needed) and exit.
    #[arg(long)]
    print_peer_id: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,libp2p=warn".into()))
        .init();
    let cli = Cli::parse();
    let kp = ensure_identity(&cli.home)?;
    let peer = kp.public().to_peer_id();
    if cli.print_peer_id {
        println!("{peer}");
        return Ok(());
    }
    let mut cfg = HubConfig::load(&cli.home)?;
    if !cli.listen.is_empty() {
        cfg.listen = cli.listen;
    }
    if !cli.external.is_empty() {
        cfg.external_addrs = cli.external;
    }
    if !cli.bootstrap.is_empty() {
        cfg.bootstrap = cli.bootstrap;
    }
    if cli.no_relay {
        cfg.relay = false;
    }
    tracing::info!(%peer, relay = cfg.relay, "systema-hub starting");
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = stop_tx.send(true);
    });
    Hub::new(kp, cfg)?.run(stop_rx, None).await.context("hub stopped")?;
    tracing::info!("systema-hub stopped");
    Ok(())
}
