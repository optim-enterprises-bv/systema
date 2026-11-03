use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use systema_kernel::{CancellationToken, Config, Kernel, TrustState, config::home_dir};
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
    /// Ask the OS: the orchestrator routes your request to the best agent and runs it.
    Do { prompt: Vec<String> },
    /// Review the audit log and draft improvement proposals for you to apply (nothing is changed).
    Propose,
    /// Show the task board: recent tasks agents delegated to each other, with status.
    Tasks,
    /// List scheduled jobs (from schedules.toml); with a name, run that job now.
    Schedule { name: Option<String> },
    /// Search the shared knowledge base for notes/docs agents have saved.
    Kb { query: Vec<String> },
    /// Add a note/document to the shared knowledge base.
    KbAdd {
        #[arg(long)]
        title: String,
        text: Vec<String>,
    },
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
    /// Ask a local agent through the running node (streams via `systema watch`/`tui`).
    Live { agent: String, prompt: Vec<String> },
    /// Inject operator guidance into a running session (or start one).
    Steer { session: String, text: Vec<String> },
    /// Cancel a running turn.
    CancelTurn { session: String },
    /// List running sessions.
    Sessions,
    /// List tool approvals waiting for an answer.
    Approvals,
    /// Answer a pending tool approval.
    Resolve {
        id: String,
        /// Allow and remember.
        #[arg(long)]
        always: bool,
        /// Deny instead of allowing.
        #[arg(long)]
        deny: bool,
    },
    /// List remembered "always allow" rules.
    Rules,
    /// Stream live kernel events (ctrl-c to stop).
    Watch,
    /// Launch the cockpit UI (monitor, steer and approve agents).
    Tui,
}

const EXAMPLE_AGENTS: &[(&str, &str)] = &[
    ("operator.toml", include_str!("../../../agents/operator.toml")),
    ("coder.toml", include_str!("../../../agents/coder.toml")),
    ("sysadmin.toml", include_str!("../../../agents/sysadmin.toml")),
    ("researcher.toml", include_str!("../../../agents/researcher.toml")),
    ("improver.toml", include_str!("../../../agents/improver.toml")),
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
            let answer = k
                .ask_with_fallback(&agent, &agent, &prompt.join(" "), None, TrustState::operator(), &TerminalApprover, &CancellationToken::new())
                .await?;
            println!("{answer}");
        }
        Cmd::Do { prompt } => {
            let k = Kernel::new(Config::load(&home)?)?;
            let prompt = prompt.join(" ");
            let agent = k.route(&prompt).await?;
            eprintln!("[routed to {agent}]");
            // ask_with_fallback hands off to operator if the routed specialist lacks access.
            let answer = k
                .ask_with_fallback(&agent, &agent, &prompt, None, TrustState::operator(), &TerminalApprover, &CancellationToken::new())
                .await?;
            println!("{answer}");
        }
        Cmd::Propose => {
            let k = Kernel::new(Config::load(&home)?)?;
            eprintln!("[reviewing the audit log — this uses the deep model and may take a minute]");
            let path = k.propose_improvements(&TerminalApprover).await?;
            let doc = std::fs::read_to_string(&path)?;
            println!("{doc}\n[saved to {}]", path.display());
        }
        Cmd::Tasks => {
            let tasks = systema_kernel::tasks::list(&home, 50);
            println!("{}", systema_kernel::tasks::render(&tasks));
        }
        Cmd::Schedule { name } => {
            let jobs = systema_kernel::schedule::load(&home);
            match name {
                None => {
                    let state = systema_kernel::schedule::load_state(&home);
                    if jobs.is_empty() {
                        println!("no scheduled jobs (define them in {}/schedules.toml)", home.display());
                    }
                    for j in &jobs {
                        let armed = if state.contains_key(&j.name) { "armed" } else { "new" };
                        println!("{:<18} {:<24} {:<8} [{}] — {}", j.name, j.describe_trigger(), if j.enabled { "enabled" } else { "disabled" }, armed, j.prompt.chars().take(50).collect::<String>());
                    }
                }
                Some(n) => match jobs.iter().find(|j| j.name == n) {
                    Some(j) => {
                        control(&home, Control::Ask { agent: j.agent.clone(), text: j.prompt.clone() }).await?;
                        println!("started `{}` on {} — follow with `systema watch` or `systema tui`", j.name, j.agent);
                    }
                    None => anyhow::bail!("no scheduled job named `{n}`"),
                },
            }
        }
        Cmd::Kb { query } => {
            let hits = systema_kernel::kb::search(&home, &query.join(" "), systema_kernel::kb::DEFAULT_K);
            println!("{}", systema_kernel::kb::render(&hits));
        }
        Cmd::KbAdd { title, text } => {
            let n = systema_kernel::kb::add(&home, &title, "cli", "operator", false, false, &text.join(" "))?;
            println!("added {n} chunk(s) to the knowledge base under `{title}`");
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
                    println!("{}  {:<10} skills={:?}{}  {}", p["peer"].as_str().unwrap_or("?"), a["name"].as_str().unwrap_or("?"), skills, caps_str(a), a["description"].as_str().unwrap_or(""));
                }
            }
        }
        Cmd::Find { skill } => {
            let v = control(&home, Control::Find { skill }).await?;
            let results = v["results"].as_array().cloned().unwrap_or_default();
            println!("{} provider(s) in the DHT for `{}`, {} with verified cards", v["providers"], v["skill"].as_str().unwrap_or(""), results.len());
            for r in results {
                for a in r["agents"].as_array().into_iter().flatten() {
                    println!("{}  {:<10}{}  {}", r["peer"].as_str().unwrap_or("?"), a["name"].as_str().unwrap_or("?"), caps_str(a), a["description"].as_str().unwrap_or(""));
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
        Cmd::Live { agent, prompt } => {
            control(&home, Control::Ask { agent: agent.clone(), text: prompt.join(" ") }).await?;
            println!("started turn for {agent}; follow it with `systema watch` or `systema tui`");
        }
        Cmd::Steer { session, text } => {
            println!("{}", control(&home, Control::Steer { session, text: text.join(" ") }).await?);
        }
        Cmd::CancelTurn { session } => println!("{}", control(&home, Control::Cancel { session }).await?),
        Cmd::Sessions => println!("{}", serde_json::to_string_pretty(&control(&home, Control::Sessions).await?)?),
        Cmd::Approvals => println!("{}", serde_json::to_string_pretty(&control(&home, Control::Approvals).await?)?),
        Cmd::Resolve { id, always, deny } => {
            let answer = if deny {
                systema_kernel::Answer::Deny
            } else if always {
                systema_kernel::Answer::Always
            } else {
                systema_kernel::Answer::Once
            };
            println!("{}", control(&home, Control::Resolve { id, answer, remember: always }).await?);
        }
        Cmd::Rules => println!("{}", serde_json::to_string_pretty(&control(&home, Control::Rules).await?)?),
        Cmd::Watch => {
            let mut lines = systema_node::control::watch(&home).await?;
            eprintln!("[watching {} — ctrl-c to stop]", home.display());
            while let Some(line) = lines.next_line().await? {
                println!("{line}");
            }
        }
        Cmd::Tui => {
            use std::os::unix::process::CommandExt as _;
            // The cockpit is a sibling binary; run it with our resolved home.
            let exe = std::env::current_exe()?;
            let tui = exe.with_file_name("systema-tui");
            let bin = if tui.exists() { tui } else { PathBuf::from("systema-tui") };
            let err = std::process::Command::new(&bin).arg(&home).exec();
            anyhow::bail!("could not launch {}: {err}", bin.display());
        }
    }
    Ok(())
}

/// Asks on the terminal; refuses when there is no terminal to ask on.
struct TerminalApprover;

impl systema_kernel::Approver for TerminalApprover {
    fn approve(&self, agent: &str, request: &systema_kernel::ToolRequest, reason: &str, rememberable: bool) -> systema_kernel::Answer {
        use std::io::{BufRead as _, IsTerminal as _, Write as _};
        use systema_kernel::Answer;
        if !std::io::stdin().is_terminal() {
            eprintln!("[systema] {agent} wants `{}`: needs approval but stdin is not a terminal; denied", request.summary());
            return Answer::Deny;
        }
        let choices = if rememberable { "[y]es once / [a]lways / [N]o" } else { "[y]es once / [N]o" };
        let mut err = std::io::stderr();
        let _ = write!(err, "\n[systema] {agent} wants to: {}\n          why asking: {reason}\n          {choices} ", request.summary());
        let _ = err.flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        match line.trim() {
            "a" | "A" | "always" if rememberable => Answer::Always,
            "y" | "Y" | "yes" => Answer::Once,
            _ => Answer::Deny,
        }
    }
}

/// Render a discovered agent card's advertised node capabilities, e.g. "  [ctx 262144 · RTX 4090 · qwen3.8-27b]".
fn caps_str(a: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(ctx) = a["max_context"].as_u64() {
        parts.push(format!("ctx {ctx}"));
    }
    if let Some(hw) = a["hardware"].as_str().filter(|s| !s.is_empty()) {
        parts.push(hw.to_string());
    }
    let models: Vec<&str> = a["models"].as_array().into_iter().flatten().filter_map(|m| m.as_str()).collect();
    if !models.is_empty() {
        parts.push(models.join(","));
    }
    if parts.is_empty() { String::new() } else { format!("  [{}]", parts.join(" · ")) }
}

async fn control(home: &Path, cmd: Control) -> anyhow::Result<serde_json::Value> {
    systema_node::control::request(home, &cmd).await
}
