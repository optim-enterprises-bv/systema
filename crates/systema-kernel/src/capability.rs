//! The capability broker's policy half: given an agent's manifest, the session's trust
//! state and a tool request, decide Allow / Ask / Deny and resolve the exact target.
//!
//! Order of checks:
//! 1. resolve the target (canonical path, program from a fixed PATH, parsed URL);
//! 2. hard denials (Systema home, credential stores, /proc …) regardless of manifest;
//! 3. the manifest is the ceiling: anything it doesn't cover is denied;
//! 4. the Rule of Two: a request that would leave the session holding private data,
//!    untrusted content and an outbound channel at once is escalated to Ask.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::{AgentManifest, GrantMode};
use crate::mcp::McpServerConfig;

/// Programs are only ever resolved from these directories, never from a path the model supplies.
pub const SAFE_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/sbin", "/sbin"];

/// Which legs of the lethal trifecta a session holds (or a request would add).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustState {
    /// Untrusted content is in the context (a remote peer's message, fetched data).
    pub untrusted: bool,
    /// Private data is in the context (reads outside the workspace, program output).
    pub private: bool,
    /// The session can push data outward (fetches, programs, replies to a remote peer).
    pub outbound: bool,
}

impl TrustState {
    /// A session driven only by the operator.
    pub fn operator() -> Self {
        Self::default()
    }

    /// A session answering a remote peer: its text is untrusted and its reply leaves the node.
    pub fn remote() -> Self {
        Self { untrusted: true, private: false, outbound: true }
    }

    pub fn with(self, adds: TrustState) -> Self {
        Self {
            untrusted: self.untrusted || adds.untrusted,
            private: self.private || adds.private,
            outbound: self.outbound || adds.outbound,
        }
    }

    pub fn is_trifecta(&self) -> bool {
        self.untrusted && self.private && self.outbound
    }
}

const PRIVATE: TrustState = TrustState { untrusted: false, private: true, outbound: false };
const NONE: TrustState = TrustState { untrusted: false, private: false, outbound: false };
const RUN: TrustState = TrustState { untrusted: false, private: true, outbound: true };
const FETCH: TrustState = TrustState { untrusted: true, private: false, outbound: true };
/// Reading untrusted shared content (e.g. a board with public/tainted posts): untrusted only.
const UNTRUSTED: TrustState = TrustState { untrusted: true, private: false, outbound: false };

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRequest {
    ReadFile { path: String },
    ListDir { path: String },
    WriteFile { path: String, content: String },
    /// In-place edit: replace the single occurrence of `old` with `new` in an existing file.
    Patch { path: String, old: String, new: String },
    /// Recursively search a directory for a substring in file names and text contents.
    SearchFiles { path: String, query: String },
    Run { program: String, args: Vec<String> },
    /// A full shell command (`bash -lc`), for trusted local agents only. Real operator shell.
    Shell { command: String },
    Fetch { url: String },
    /// The agent's own persistent key/value memory (stored in its workspace).
    Memory { action: String, key: String, value: String },
    /// Post to the shared message board.
    BoardPost { topic: String, text: String },
    /// Read recent posts from the shared message board (optionally one topic).
    BoardRead { topic: Option<String> },
    /// Hand a sub-task to another agent and wait for its result.
    DelegateTask { agent: String, task: String },
    /// Read the task board (recent delegated tasks and their status).
    TaskList,
    /// A `tools/call` on an operator-configured MCP server. `arguments` is the model's JSON.
    Mcp { server: String, tool: String, arguments: String },
}

impl ToolRequest {
    pub fn kind(&self) -> &'static str {
        match self {
            ToolRequest::ReadFile { .. } => "read_file",
            ToolRequest::ListDir { .. } => "list_dir",
            ToolRequest::WriteFile { .. } => "write_file",
            ToolRequest::Patch { .. } => "patch",
            ToolRequest::SearchFiles { .. } => "search_files",
            ToolRequest::Run { .. } => "run",
            ToolRequest::Shell { .. } => "shell",
            ToolRequest::Fetch { .. } => "fetch_url",
            ToolRequest::Memory { .. } => "memory",
            ToolRequest::BoardPost { .. } => "board_post",
            ToolRequest::BoardRead { .. } => "board_read",
            ToolRequest::DelegateTask { .. } => "delegate_task",
            ToolRequest::TaskList => "task_list",
            ToolRequest::Mcp { .. } => "mcp",
        }
    }

    /// One line for prompts and the audit log.
    pub fn summary(&self) -> String {
        match self {
            ToolRequest::ReadFile { path } => format!("read_file {path}"),
            ToolRequest::ListDir { path } => format!("list_dir {path}"),
            ToolRequest::WriteFile { path, content } => format!("write_file {path} ({} bytes)", content.len()),
            ToolRequest::Patch { path, .. } => format!("patch {path}"),
            ToolRequest::SearchFiles { path, query } => format!("search_files {path} for {:?}", query.chars().take(60).collect::<String>()),
            ToolRequest::Run { program, args } => format!("run {program} {}", args.join(" ")),
            ToolRequest::Shell { command } => format!("shell {}", command.replace('\n', " ")),
            ToolRequest::Fetch { url } => format!("fetch_url {url}"),
            ToolRequest::Memory { action, key, .. } => format!("memory {action} {key}").trim_end().to_string(),
            ToolRequest::BoardPost { topic, .. } => format!("board_post [{topic}]"),
            ToolRequest::BoardRead { topic } => match topic {
                Some(t) => format!("board_read [{t}]"),
                None => "board_read".into(),
            },
            ToolRequest::DelegateTask { agent, task } => format!("delegate_task → {agent}: {}", task.chars().take(80).collect::<String>()),
            ToolRequest::TaskList => "task_list".into(),
            ToolRequest::Mcp { server, tool, arguments } => {
                let a: String = arguments.chars().take(120).collect();
                format!("mcp {server}.{tool} {a}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Needs the operator. `rememberable` is false for Rule-of-Two escalations.
    Ask { reason: String, rememberable: bool },
    Deny(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Path(PathBuf),
    Program(PathBuf),
    Url(String),
    Shell(String),
    Mcp,
    /// The agent's own memory store, resolved inside its workspace at execute time.
    Memory,
    /// The shared message board (handled in the kernel, not the per-agent sandbox).
    Board,
    /// A delegated sub-task or the task board (handled in the kernel).
    Delegate,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub decision: Decision,
    /// Legs the session gains if the request executes.
    pub legs: TrustState,
    pub target: Target,
}

impl Checked {
    fn deny(reason: impl Into<String>) -> Self {
        Self { decision: Decision::Deny(reason.into()), legs: NONE, target: Target::None }
    }
}

pub struct Broker {
    systema_home: PathBuf,
    user_home: PathBuf,
    protected: Vec<PathBuf>,
    mcp: std::collections::BTreeMap<String, McpServerConfig>,
}

/// Paths under the user's home that hold credentials or session state.
const PROTECTED_IN_HOME: &[&str] = &[
    ".ssh", ".gnupg", ".password-store", ".local/share/keyrings", ".netrc", ".git-credentials",
    ".aws", ".azure", ".kube", ".docker", ".config/gcloud", ".config/gh", ".config/hub",
    ".cargo/credentials", ".cargo/credentials.toml", ".npmrc", ".pypirc", ".mozilla",
    ".config/google-chrome", ".config/chromium", ".config/BraveSoftware", ".hermes", ".claude",
    ".claude.json", ".config/systema",
];

const PROTECTED_SYSTEM: &[&str] =
    &["/proc", "/sys/firmware", "/etc/shadow", "/etc/gshadow", "/etc/sudoers", "/etc/sudoers.d", "/root", "/run/user"];

impl Broker {
    pub fn new(
        systema_home: impl Into<PathBuf>,
        user_home: impl Into<PathBuf>,
        mcp: std::collections::BTreeMap<String, McpServerConfig>,
    ) -> Self {
        let systema_home = canonical_or_normal(&systema_home.into());
        let user_home = canonical_or_normal(&user_home.into());
        let mut protected: Vec<PathBuf> = PROTECTED_IN_HOME.iter().map(|p| canonical_or_normal(&user_home.join(p))).collect();
        protected.extend(PROTECTED_SYSTEM.iter().map(|p| canonical_or_normal(Path::new(p))));
        protected.push(systema_home.clone());
        Self { systema_home, user_home, protected, mcp }
    }

    pub fn mcp_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.mcp.get(name)
    }

    /// Working directory and HOME of an MCP server process.
    pub fn mcp_dir(&self, server: &str) -> PathBuf {
        self.systema_home.join("mcp").join(server)
    }

    pub fn workspace(&self, agent: &str) -> PathBuf {
        self.systema_home.join("workspaces").join(agent)
    }

    /// `~` → user home; relative → the agent's workspace; then lexically normalized.
    pub fn expand(&self, agent: &str, p: &str) -> PathBuf {
        let raw = if p == "~" {
            self.user_home.clone()
        } else if let Some(rest) = p.strip_prefix("~/") {
            self.user_home.join(rest)
        } else if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.workspace(agent).join(p)
        };
        normalize(&raw)
    }

    fn is_protected(&self, agent: &str, p: &Path) -> bool {
        if p.starts_with(self.workspace(agent)) {
            return false;
        }
        self.protected.iter().any(|x| p.starts_with(x))
    }

    fn under_any(&self, agent: &str, p: &Path, roots: &[String]) -> bool {
        roots.iter().any(|r| p.starts_with(canonical_or_normal(&self.expand(agent, r))))
    }

    /// Resolve a path that must exist (read/list): canonical, symlinks followed.
    fn resolve_existing(&self, agent: &str, p: &str) -> Result<PathBuf, String> {
        let expanded = self.expand(agent, p);
        expanded.canonicalize().map_err(|e| format!("{}: {e}", expanded.display()))
    }

    /// Resolve a write target: canonical parent + file name; the target itself may not be a symlink.
    fn resolve_for_write(&self, agent: &str, p: &str) -> Result<PathBuf, String> {
        let expanded = self.expand(agent, p);
        let name = expanded.file_name().ok_or("write target has no file name")?.to_owned();
        let parent = expanded.parent().ok_or("write target has no parent")?;
        let ws = self.workspace(agent);
        let parent = if parent.starts_with(&ws) {
            // Inside the workspace, missing directories are created on write.
            canonical_prefix(parent)
        } else {
            parent.canonicalize().map_err(|e| format!("{}: {e}", parent.display()))?
        };
        let target = parent.join(name);
        if target.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("refusing to write through a symlink".into());
        }
        Ok(target)
    }

    pub fn check(&self, m: &AgentManifest, trust: &TrustState, req: &ToolRequest) -> Checked {
        let agent = m.name.as_str();
        let caps = &m.capabilities;
        let ws = canonical_prefix(&self.workspace(agent));

        let (mode, legs, target) = match req {
            ToolRequest::ReadFile { path } | ToolRequest::ListDir { path } => {
                let p = match self.resolve_existing(agent, path) {
                    Ok(p) => p,
                    Err(e) => return Checked::deny(e),
                };
                if self.is_protected(agent, &p) {
                    return Checked::deny(protected_msg(&p));
                }
                if p.starts_with(&ws) {
                    (GrantMode::Allow, NONE, Target::Path(p))
                } else if self.under_any(agent, &p, &caps.fs_read) || self.under_any(agent, &p, &caps.fs_write) {
                    (GrantMode::Allow, PRIVATE, Target::Path(p))
                } else {
                    return Checked::deny(format!("{} is outside this agent's fs_read", p.display()));
                }
            }
            ToolRequest::WriteFile { path, .. } | ToolRequest::Patch { path, .. } => {
                let p = match self.resolve_for_write(agent, path) {
                    Ok(p) => p,
                    Err(e) => return Checked::deny(e),
                };
                if self.is_protected(agent, &p) {
                    return Checked::deny(protected_msg(&p));
                }
                if p.starts_with(&ws) {
                    (GrantMode::Allow, NONE, Target::Path(p))
                } else if self.under_any(agent, &p, &caps.fs_write) {
                    let mode = if trust.untrusted { GrantMode::Ask } else { GrantMode::Allow };
                    (mode, NONE, Target::Path(p))
                } else {
                    return Checked::deny(format!("{} is outside this agent's fs_write", p.display()));
                }
            }
            ToolRequest::SearchFiles { path, .. } => {
                let p = match self.resolve_existing(agent, path) {
                    Ok(p) => p,
                    Err(e) => return Checked::deny(e),
                };
                if self.is_protected(agent, &p) {
                    return Checked::deny(protected_msg(&p));
                }
                if p.starts_with(&ws) {
                    (GrantMode::Allow, NONE, Target::Path(p))
                } else if self.under_any(agent, &p, &caps.fs_read) || self.under_any(agent, &p, &caps.fs_write) {
                    (GrantMode::Allow, PRIVATE, Target::Path(p))
                } else {
                    return Checked::deny(format!("{} is outside this agent's fs_read", p.display()));
                }
            }
            ToolRequest::Memory { action, .. } => {
                // The agent's own scratchpad, inside its workspace: always allowed. Reading a
                // value back may reintroduce private data, so `get` carries the private leg.
                let legs = if action == "get" { PRIVATE } else { NONE };
                (GrantMode::Allow, legs, Target::Memory)
            }
            ToolRequest::BoardPost { .. } => {
                if !caps.board {
                    return Checked::deny("this agent has no board capability");
                }
                (GrantMode::Allow, NONE, Target::Board)
            }
            ToolRequest::BoardRead { topic } => {
                if !caps.board {
                    return Checked::deny("this agent has no board capability");
                }
                // The board is shared read/write state. It may hold the operator's own
                // coordination (private), so a read is at least `private` — which means a
                // swarm/remote session (already untrusted + outbound) that reads it completes
                // the trifecta and is gated, blocking a leak. And if any returned post is from
                // a public/swarm author or a tainted session, the read also brings in untrusted
                // content, so the Rule of Two guards against an injected instruction.
                let posts = crate::board::read(&self.systema_home, topic.as_deref(), crate::board::READ_LIMIT);
                let legs = if crate::board::is_tainted(&posts) { PRIVATE.with(UNTRUSTED) } else { PRIVATE };
                (GrantMode::Allow, legs, Target::Board)
            }
            ToolRequest::DelegateTask { .. } => {
                if !caps.delegate {
                    return Checked::deny("this agent has no delegate capability");
                }
                // Delegating from an untrusted session is the escalation vector (a remote peer
                // causing a more-capable agent to run), so it is refused. Only a clean,
                // operator-driven session may delegate. The sub-task's final taint is merged
                // back into this session by the kernel after it runs.
                if trust.untrusted {
                    return Checked::deny("cannot delegate from a session holding untrusted content");
                }
                (GrantMode::Allow, NONE, Target::Delegate)
            }
            ToolRequest::TaskList => {
                if !caps.delegate {
                    return Checked::deny("this agent has no delegate capability");
                }
                // Task results may contain private data pulled by the assignee.
                (GrantMode::Allow, PRIVATE, Target::Delegate)
            }
            ToolRequest::Run { program, args } => {
                if program.contains('/') {
                    return Checked::deny("give a program name, not a path; programs resolve from the system PATH only");
                }
                let Some(resolved) = resolve_program(program) else {
                    return Checked::deny(format!("{program}: not found in the system PATH"));
                };
                let grant = caps.exec.iter().find(|g| g.program == *program && args.starts_with(&g.args_prefix));
                let mode = match (grant, caps.shell) {
                    (Some(g), _) => g.mode,
                    (None, true) => GrantMode::Ask,
                    (None, false) => {
                        return Checked::deny(format!("no exec grant covers `{program} {}`", args.join(" ")));
                    }
                };
                (mode, RUN, Target::Program(resolved))
            }
            ToolRequest::Shell { command } => {
                if !caps.shell {
                    return Checked::deny("this agent has no shell capability");
                }
                // A real shell can read anything the user can (including credential stores),
                // so it is never handed to a swarm-facing agent. Until public agents run in a
                // microVM, shell is operator-only.
                if caps.public {
                    return Checked::deny("shell is not available to swarm-facing (public) agents");
                }
                (GrantMode::Allow, RUN, Target::Shell(command.clone()))
            }
            ToolRequest::Fetch { url } => {
                if !caps.network {
                    return Checked::deny("this agent has no network capability");
                }
                if let Err(e) = check_url_shape(url) {
                    return Checked::deny(e);
                }
                (GrantMode::Allow, FETCH, Target::Url(url.clone()))
            }
            ToolRequest::Mcp { server, tool, .. } => {
                if !caps.mcp.contains(server) {
                    return Checked::deny(format!("this agent may not use mcp server `{server}`"));
                }
                let Some(cfg) = self.mcp.get(server) else {
                    return Checked::deny(format!("mcp server `{server}` is not configured"));
                };
                if cfg.tools.as_ref().is_some_and(|allow| !allow.contains(tool)) {
                    return Checked::deny(format!("tool `{tool}` is not enabled on mcp server `{server}`"));
                }
                let legs = TrustState { untrusted: cfg.untrusted, private: cfg.private, outbound: cfg.outbound };
                let mode = if cfg.ask_tools.contains(tool) { GrantMode::Ask } else { cfg.mode };
                (mode, legs, Target::Mcp)
            }
        };

        let decision = if trust.with(legs).is_trifecta() {
            // Rule-of-Two escalation: never rememberable — an "always allow" here would
            // permanently reopen the exfiltration path.
            Decision::Ask {
                reason: format!(
                    "`{}` would give this session private data, untrusted content and an outbound channel at once \
                     (lethal trifecta); session so far: untrusted={} private={} outbound={}",
                    req.summary(),
                    trust.untrusted,
                    trust.private,
                    trust.outbound
                ),
                rememberable: false,
            }
        } else {
            match mode {
                GrantMode::Allow => Decision::Allow,
                GrantMode::Ask => Decision::Ask {
                    reason: format!("manifest requires approval for `{}`", req.summary()),
                    rememberable: true,
                },
            }
        };
        Checked { decision, legs, target }
    }
}

fn protected_msg(p: &Path) -> String {
    format!(
        "{} is protected by a hard rule (credentials, keys, Systema's own state or kernel interfaces). \
         No grant or approval can open it. Do not try other paths or tools to reach it, and do not suggest \
         workarounds such as copying, moving or pasting its contents",
        p.display()
    )
}

pub fn resolve_program(name: &str) -> Option<PathBuf> {
    SAFE_PATH.iter().map(|d| Path::new(d).join(name)).find(|p| p.is_file()).and_then(|p| p.canonicalize().ok())
}

fn check_url_shape(url: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err("only http and https URLs".into());
    }
    match u.host() {
        None => Err("url has no host".into()),
        Some(url::Host::Ipv4(ip)) if !is_public_ip(ip.into()) => Err(format!("{ip} is not a public address")),
        Some(url::Host::Ipv6(ip)) if !is_public_ip(ip.into()) => Err(format!("{ip} is not a public address")),
        Some(url::Host::Domain(d)) if d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") => {
            Err(format!("{d} is a local name"))
        }
        _ => Ok(()),
    }
}

/// Public unicast only: no loopback, private, link-local, CGNAT, multicast, documentation, benchmarking.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Lexical normalization: drop `.`, resolve `..` without touching the filesystem.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing prefix, then append the rest (normalized).
fn canonical_prefix(p: &Path) -> PathBuf {
    let p = normalize(p);
    let mut existing = p.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return p,
        }
    }
}

fn canonical_or_normal(p: &Path) -> PathBuf {
    canonical_prefix(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Capabilities, ExecGrant, ModelTier, Priority};

    struct Env {
        root: PathBuf,
        broker: Broker,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn env() -> Env {
        let root = std::env::temp_dir().join(format!("systema-cap-{}", systema_proto::random_id()));
        let home = root.join("home");
        for d in ["src/proj", ".ssh", ".systema/workspaces/coder", "elsewhere"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::write(home.join("src/proj/main.rs"), "fn main() {}").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "SECRET").unwrap();
        std::fs::write(home.join(".systema/identity.key"), "KEY").unwrap();
        std::fs::write(home.join("elsewhere/x"), "x").unwrap();
        std::os::unix::fs::symlink(home.join(".ssh/id_ed25519"), home.join("src/proj/innocent.txt")).unwrap();
        let mut mcp = std::collections::BTreeMap::new();
        let server = |untrusted, private, outbound| McpServerConfig {
            command: "true".into(),
            args: vec![],
            env: Default::default(),
            tools: Some(vec!["search".into(), "delete".into()]),
            ask_tools: vec!["delete".into()],
            mode: GrantMode::Allow,
            private,
            outbound,
            untrusted,
        };
        mcp.insert("web".into(), server(true, false, true));
        mcp.insert("unclassified".into(), server(true, true, true));
        let broker = Broker::new(home.join(".systema"), &home, mcp);
        Env { root, broker }
    }

    fn manifest() -> AgentManifest {
        AgentManifest {
            name: "coder".into(),
            description: String::new(),
            model: ModelTier::Fast,
            priority: Priority::Normal,
            system_prompt: String::new(),
            skills: vec![],
            capabilities: Capabilities {
                fs_read: vec!["~/src".into()],
                fs_write: vec!["~/src/proj".into()],
                network: true,
                shell: false,
                exec: vec![
                    ExecGrant { program: "ls".into(), args_prefix: vec![], mode: GrantMode::Allow },
                    ExecGrant { program: "git".into(), args_prefix: vec!["status".into()], mode: GrantMode::Allow },
                    ExecGrant { program: "rm".into(), args_prefix: vec![], mode: GrantMode::Ask },
                ],
                mcp: vec!["web".into(), "unclassified".into()],
                board: true,
                delegate: false,
                public: true,
            },
        }
    }

    fn read(p: &str) -> ToolRequest {
        ToolRequest::ReadFile { path: p.into() }
    }

    #[test]
    fn operator_reads_granted_tree() {
        let e = env();
        let c = e.broker.check(&manifest(), &TrustState::operator(), &read("~/src/proj/main.rs"));
        assert_eq!(c.decision, Decision::Allow);
        assert!(c.legs.private);
    }

    #[test]
    fn credentials_and_own_policy_are_protected() {
        let e = env();
        let m = manifest();
        let op = TrustState::operator();
        for p in ["~/.ssh/id_ed25519", "~/.systema/identity.key", "/proc/self/environ"] {
            assert!(matches!(e.broker.check(&m, &op, &read(p)).decision, Decision::Deny(_)), "{p}");
        }
    }

    #[test]
    fn traversal_and_symlinks_cannot_escape() {
        let e = env();
        let m = manifest();
        let op = TrustState::operator();
        assert!(matches!(e.broker.check(&m, &op, &read("~/src/../.ssh/id_ed25519")).decision, Decision::Deny(_)));
        // Symlink inside a granted tree pointing at a key: canonicalized, then protected.
        assert!(matches!(e.broker.check(&m, &op, &read("~/src/proj/innocent.txt")).decision, Decision::Deny(_)));
        assert!(matches!(e.broker.check(&m, &op, &read("~/elsewhere/x")).decision, Decision::Deny(_)));
    }

    #[test]
    fn workspace_is_free_and_not_private() {
        let e = env();
        let c = e.broker.check(
            &manifest(),
            &TrustState::remote(),
            &ToolRequest::WriteFile { path: "notes/todo.md".into(), content: "x".into() },
        );
        assert_eq!(c.decision, Decision::Allow);
        assert_eq!(c.legs, TrustState::default());
    }

    #[test]
    fn remote_session_cannot_read_private_without_operator() {
        let e = env();
        let c = e.broker.check(&manifest(), &TrustState::remote(), &read("~/src/proj/main.rs"));
        assert!(matches!(c.decision, Decision::Ask { reason: ref r, rememberable: false } if r.contains("lethal trifecta")));
    }

    #[test]
    fn fetch_after_private_read_asks() {
        let e = env();
        let after_read = TrustState::operator().with(PRIVATE);
        let c = e.broker.check(&manifest(), &after_read, &ToolRequest::Fetch { url: "https://example.com/?q=secret".into() });
        assert!(matches!(c.decision, Decision::Ask { .. }));
        let fresh = e.broker.check(&manifest(), &TrustState::operator(), &ToolRequest::Fetch { url: "https://example.com/".into() });
        assert_eq!(fresh.decision, Decision::Allow);
        assert!(fresh.legs.untrusted);
    }

    #[test]
    fn fetch_refuses_local_targets() {
        let e = env();
        let op = TrustState::operator();
        for u in ["http://127.0.0.1:1234/v1/models", "http://localhost/", "http://192.168.1.1/", "http://[::1]/", "file:///etc/passwd", "http://100.64.0.1/"] {
            let c = e.broker.check(&manifest(), &op, &ToolRequest::Fetch { url: u.into() });
            assert!(matches!(c.decision, Decision::Deny(_)), "{u}");
        }
    }

    #[test]
    fn exec_grants_match_program_and_prefix() {
        let e = env();
        let m = manifest();
        let op = TrustState::operator();
        let run = |p: &str, a: &[&str]| ToolRequest::Run { program: p.into(), args: a.iter().map(|s| s.to_string()).collect() };
        assert_eq!(e.broker.check(&m, &op, &run("git", &["status", "-s"])).decision, Decision::Allow);
        assert!(matches!(e.broker.check(&m, &op, &run("git", &["push"])).decision, Decision::Deny(_)));
        assert!(matches!(e.broker.check(&m, &op, &run("rm", &["-rf", "x"])).decision, Decision::Ask { .. }));
        assert!(matches!(e.broker.check(&m, &op, &run("/tmp/evil/ls", &[])).decision, Decision::Deny(_)));
        assert!(matches!(e.broker.check(&m, &op, &run("curl", &[])).decision, Decision::Deny(_)));
    }

    #[test]
    fn untrusted_session_writes_outside_workspace_need_approval() {
        let e = env();
        let tainted = TrustState { untrusted: true, private: false, outbound: false };
        let c = e.broker.check(&manifest(), &tainted, &ToolRequest::WriteFile { path: "~/src/proj/new.rs".into(), content: "x".into() });
        assert!(matches!(c.decision, Decision::Ask { .. }));
    }

    fn mcp(server: &str, tool: &str) -> ToolRequest {
        ToolRequest::Mcp { server: server.into(), tool: tool.into(), arguments: "{}".into() }
    }

    #[test]
    fn delegation_needs_capability_and_a_clean_session() {
        let e = env();
        let mut m = manifest();
        let del = ToolRequest::DelegateTask { agent: "sysadmin".into(), task: "check disks".into() };
        // Without the capability: denied.
        assert!(matches!(e.broker.check(&m, &TrustState::operator(), &del).decision, Decision::Deny(_)));
        // With it, from a clean session: allowed.
        m.capabilities.delegate = true;
        assert_eq!(e.broker.check(&m, &TrustState::operator(), &del).decision, Decision::Allow);
        // But never from an untrusted session (that would be a privilege-escalation path).
        assert!(matches!(e.broker.check(&m, &TrustState::remote(), &del).decision, Decision::Deny(_)));
    }

    #[test]
    fn board_posts_are_untrusted_to_readers_and_gate_remote_leaks() {
        let e = env();
        let m = manifest(); // board = true, public = true
        // Posting is allowed and adds no legs.
        let post = ToolRequest::BoardPost { topic: "general".into(), text: "hi".into() };
        let c = e.broker.check(&m, &TrustState::operator(), &post);
        assert_eq!(c.decision, Decision::Allow);
        assert_eq!(c.legs, TrustState::default());
        // A read of an empty/operator board carries only the private leg.
        let read = ToolRequest::BoardRead { topic: None };
        let c = e.broker.check(&m, &TrustState::operator(), &read);
        assert!(c.legs.private && !c.legs.untrusted);
        // A remote (untrusted + outbound) session reading the board completes the trifecta
        // → escalated, so a swarm agent cannot read the operator's board and reply out.
        let c = e.broker.check(&m, &TrustState::remote(), &read);
        assert!(matches!(c.decision, Decision::Ask { .. }));
        // An agent without the board capability is denied.
        let mut no_board = manifest();
        no_board.capabilities.board = false;
        assert!(matches!(e.broker.check(&no_board, &TrustState::operator(), &read).decision, Decision::Deny(_)));
    }

    #[test]
    fn mcp_calls_follow_manifest_allowlist_and_legs() {
        let e = env();
        let m = manifest();
        let op = TrustState::operator();
        let c = e.broker.check(&m, &op, &mcp("web", "search"));
        assert_eq!(c.decision, Decision::Allow);
        assert!(c.legs.untrusted && c.legs.outbound && !c.legs.private);
        assert!(matches!(e.broker.check(&m, &op, &mcp("web", "delete")).decision, Decision::Ask { .. }));
        assert!(matches!(e.broker.check(&m, &op, &mcp("web", "exec")).decision, Decision::Deny(_)));
        assert!(matches!(e.broker.check(&m, &op, &mcp("github", "search")).decision, Decision::Deny(_)));
        // Unclassified servers default to all three legs: always the operator's call.
        assert!(matches!(e.broker.check(&m, &op, &mcp("unclassified", "search")).decision, Decision::Ask { .. }));
        // A web search after reading private files would complete the trifecta.
        let after_read = op.with(PRIVATE);
        assert!(matches!(e.broker.check(&m, &after_read, &mcp("web", "search")).decision, Decision::Ask { .. }));
    }
}
