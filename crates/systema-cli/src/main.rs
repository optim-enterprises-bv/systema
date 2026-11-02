use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use systema_kernel::{Config, Kernel, config::home_dir};
use systema_node::control::Control;
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
    /// Run the swarm node (discovery, card announcements, inbound requests) until ctrl-c.
    Node,
    /// List agents discovered on other nodes (needs a running node).
    Peers,
    /// Find agents offering a skill anywhere in the swarm (DHT lookup, then signed card fetch).
    Find { skill: String },
    /// Ask an agent on another node to use one of its skills.
    Send {
        /// Peer id of the remote node (see `systema peers`).
        peer: String,
        /// Agent name on that node.
        agent: String,
        #[arg(long)]
        skill: String,
        text: Vec<String>,
    },
    /// Requests from unknown peers waiting for your approval.
    Inbox,
    /// Approve an inbox item: grant that peer that skill.
    Approve {
        id: String,
        /// Calls allowed per hour.
        #[arg(long, default_value_t = 10)]
        per_hour: u32,
        /// Grant lifetime in hours (default: until revoked).
        #[arg(long)]
        hours: Option<u64>,
    },
    /// Drop an inbox item without granting anything.
    Reject { id: String },
    /// Block a peer: revoke its grants, drop its requests, disconnect it.
    Block { peer: String },
    /// Show grants and the blocklist.
    Grants,
}

const EXAMPLE_AGENTS: &[(&str, &str)] = &[
    ("coder.toml", include_str!("../../../agents/coder.toml")),
    ("sysadmin.toml", include_str!("../../../agents/sysadmin.toml")),
    ("researcher.toml", include_str!("../../../agents/researcher.toml")),
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
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(home.join("agents"))?;
    // Holds the identity key, control socket, grants and audit log: owner only.
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
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
            println!("{}", k.ask(&agent, &prompt.join(" "), &TerminalApprover).await?);
        }
        Cmd::Node => {
            let kp = load_identity(&home)?;
            let cfg = Config::load(&home)?;
            let k = std::sync::Arc::new(Kernel::new(cfg.clone())?);
            let mut node = systema_node::Node::new(kp, cfg.node, k.clone(), home.clone())?;
            let pid = node.peer_id().to_string();
            node.set_local_cards(k.public_cards(&pid, &[]));
            node.run().await?;
        }
        Cmd::Peers => {
            let v = control(&home, Control::Peers).await?;
            for p in v.as_array().into_iter().flatten() {
                for a in p["agents"].as_array().into_iter().flatten() {
                    let skills: Vec<_> = a["skills"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_str()).collect();
                    println!("{}  {:<10} skills={:?}  {}", p["peer"].as_str().unwrap_or("?"), a["name"].as_str().unwrap_or("?"), skills, a["description"].as_str().unwrap_or(""));
                }
            }
        }
        Cmd::Find { skill } => {
            let v = control(&home, Control::Find { skill }).await?;
            let results = v["results"].as_array().cloned().unwrap_or_default();
            println!("{} provider(s) in the DHT for `{}`, {} with verified cards", v["providers"], v["skill"].as_str().unwrap_or(""), results.len());
            for r in results {
                for a in r["agents"].as_array().into_iter().flatten() {
                    println!("{}  {:<10} {}", r["peer"].as_str().unwrap_or("?"), a["name"].as_str().unwrap_or("?"), a["description"].as_str().unwrap_or(""));
                    for t in a["transports"].as_array().into_iter().flatten() {
                        println!("    via {}", t["address"].as_str().unwrap_or("?"));
                    }
                }
            }
            for e in v["errors"].as_array().into_iter().flatten() {
                eprintln!("  note: {}", e.as_str().unwrap_or(""));
            }
        }
        Cmd::Send { peer, agent, skill, text } => {
            let v = control(&home, Control::Send { peer, agent, skill, text: text.join(" ") }).await?;
            match v["status"].as_str() {
                Some("ok") => println!("{}", v["text"].as_str().unwrap_or("")),
                Some("pending") => println!("pending: held in the remote inbox as {} ({})", v["inbox_id"].as_str().unwrap_or("?"), v["reason"].as_str().unwrap_or("")),
                Some("denied") => anyhow::bail!("denied: {}", v["reason"].as_str().unwrap_or("")),
                _ => anyhow::bail!("error: {}", v["reason"].as_str().unwrap_or(&v.to_string())),
            }
        }
        Cmd::Inbox => {
            let v = control(&home, Control::Inbox).await?;
            let items = v.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("inbox empty");
            }
            for i in items {
                let text = i["text"].as_str().unwrap_or("");
                let short: String = text.chars().take(120).collect();
                println!("{}  peer={}  {}/{}\n    {}{}", i["id"].as_str().unwrap_or("?"), i["peer"].as_str().unwrap_or("?"), i["agent"].as_str().unwrap_or("?"), i["skill"].as_str().unwrap_or("?"), short.replace('\n', " "), if text.len() > short.len() { "…" } else { "" });
            }
        }
        Cmd::Approve { id, per_hour, hours } => println!("{}", serde_json::to_string_pretty(&control(&home, Control::Approve { id, per_hour, hours }).await?)?),
        Cmd::Reject { id } => println!("{}", control(&home, Control::Reject { id }).await?),
        Cmd::Block { peer } => println!("{}", control(&home, Control::Block { peer }).await?),
        Cmd::Grants => println!("{}", serde_json::to_string_pretty(&control(&home, Control::Grants).await?)?),
    }
    Ok(())
}

/// Asks on the terminal; refuses when there is no terminal to ask on.
struct TerminalApprover;

impl systema_kernel::Approver for TerminalApprover {
    fn approve(&self, agent: &str, request: &systema_kernel::ToolRequest, reason: &str) -> bool {
        use std::io::{BufRead as _, IsTerminal as _, Write as _};
        if !std::io::stdin().is_terminal() {
            eprintln!("[systema] {agent} wants `{}`: needs approval but stdin is not a terminal; denied", request.summary());
            return false;
        }
        let mut err = std::io::stderr();
        let _ = write!(err, "\n[systema] {agent} wants to: {}\n          why asking: {reason}\n          allow once? [y/N] ", request.summary());
        let _ = err.flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

async fn control(home: &Path, cmd: Control) -> anyhow::Result<serde_json::Value> {
    systema_node::control::request(home, &cmd).await
}
