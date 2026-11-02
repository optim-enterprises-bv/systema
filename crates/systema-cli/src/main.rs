use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use systema_kernel::{Config, Kernel, config::home_dir};
use systema_proto::{Signed, libp2p_identity::Keypair};

/// Systema: a local agentic OS whose agents find and message each other across the internet.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// State directory (default $SYSTEMA_HOME or ~/.systema).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the state dir, node identity, default config and example agents.
    Init,
    /// Print this node's peer id.
    Id,
    /// Check titan-engine is reachable and list agents.
    Status,
    /// List installed agents.
    Agents,
    /// Print the signed agent cards this node would publish.
    Cards,
    /// Send one prompt to a local agent through the kernel scheduler.
    Ask { agent: String, prompt: Vec<String> },
    /// Run the swarm node (discovery + card announcements) until ctrl-c.
    Node,
}

const EXAMPLE_AGENTS: &[(&str, &str)] = &[
    ("coder.toml", include_str!("../../../agents/coder.toml")),
    ("sysadmin.toml", include_str!("../../../agents/sysadmin.toml")),
];

fn identity_path(home: &Path) -> PathBuf {
    home.join("identity.key")
}

fn load_identity(home: &Path) -> anyhow::Result<Keypair> {
    let p = identity_path(home);
    let bytes = std::fs::read(&p).with_context(|| format!("no identity at {} (run `systema init`)", p.display()))?;
    Keypair::from_protobuf_encoding(&bytes).context("corrupt identity key")
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

fn init(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home.join("agents"))?;
    let id = identity_path(home);
    if id.exists() {
        println!("identity   kept     {}", id.display());
    } else {
        let kp = Keypair::generate_ed25519();
        write_private(&id, &kp.to_protobuf_encoding()?)?;
        println!("identity   created  {} ({})", id.display(), kp.public().to_peer_id());
    }
    let cfg = home.join("systema.toml");
    if cfg.exists() {
        println!("config     kept     {}", cfg.display());
    } else {
        std::fs::write(&cfg, toml::to_string_pretty(&Config::default())?)?;
        println!("config     created  {}", cfg.display());
    }
    for (name, body) in EXAMPLE_AGENTS {
        let p = home.join("agents").join(name);
        if !p.exists() {
            std::fs::write(&p, body)?;
            println!("agent      created  {}", p.display());
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,libp2p=warn".into()))
        .init();
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(home_dir);

    match cli.cmd {
        Cmd::Init => init(&home)?,
        Cmd::Id => println!("{}", load_identity(&home)?.public().to_peer_id()),
        Cmd::Status => {
            let k = Kernel::new(Config::load(&home)?)?;
            match k.inference.health().await {
                Ok(()) => println!("inference  ok       {}", k.config.inference.endpoint),
                Err(e) => println!("inference  DOWN     {} ({e})", k.config.inference.endpoint),
            }
            println!("agents     {}", k.agents().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        Cmd::Agents => {
            let k = Kernel::new(Config::load(&home)?)?;
            for a in k.agents() {
                let skills: Vec<_> = a.skills.iter().map(|s| s.id.as_str()).collect();
                println!("{:<12} {:?}/{:?} public={} skills={:?}  {}", a.name, a.model, a.priority, a.capabilities.public, skills, a.description);
            }
        }
        Cmd::Cards => {
            let kp = load_identity(&home)?;
            let k = Kernel::new(Config::load(&home)?)?;
            for card in k.public_cards(&kp.public().to_peer_id().to_string(), &[]) {
                println!("{}", serde_json::to_string_pretty(&Signed::sign(&kp, card)?)?);
            }
        }
        Cmd::Ask { agent, prompt } => {
            let k = Kernel::new(Config::load(&home)?)?;
            println!("{}", k.ask(&agent, &prompt.join(" ")).await?);
        }
        Cmd::Node => {
            let kp = load_identity(&home)?;
            let cfg = Config::load(&home)?;
            let k = Kernel::new(cfg.clone())?;
            let mut node = systema_node::Node::new(kp, cfg.node)?;
            let pid = node.peer_id().to_string();
            node.set_local_cards(k.public_cards(&pid, &[]));
            node.run().await?;
        }
    }
    Ok(())
}
