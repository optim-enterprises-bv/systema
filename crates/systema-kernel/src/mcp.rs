//! MCP (Model Context Protocol) client over stdio, behind the capability broker.
//!
//! The operator declares servers in `systema.toml` under `[mcp.<name>]`, including which
//! legs of the lethal trifecta the server touches. Agents opt in per server via
//! `capabilities.mcp`. Every `tools/call` goes through [`crate::capability::Broker::check`].
//!
//! Servers are started lazily, shared by all agents, with a scrubbed environment (only
//! the variables the operator lists). They run as the user, unsandboxed: the broker governs
//! *which calls are made*, not what a server does internally, so install only servers you
//! trust (the same rule Hermes and Claude Code apply).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};

use crate::manifest::GrantMode;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
pub const RESULT_CAP: usize = 64 * 1024;

fn yes() -> bool {
    true
}

/// `[mcp.<name>]` in systema.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Program to start (a name resolved from the system PATH, or an absolute path).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The *only* environment variables the server gets, besides PATH/HOME/LANG.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// If set, only these tools are exposed.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Tools that always need operator approval.
    #[serde(default)]
    pub ask_tools: Vec<String>,
    /// Default mode for this server's tools.
    #[serde(default = "default_mode")]
    pub mode: GrantMode,
    /// Trifecta legs a call adds. Defaults are all true: unclassified servers need approval.
    #[serde(default = "yes")]
    pub private: bool,
    #[serde(default = "yes")]
    pub outbound: bool,
    #[serde(default = "yes")]
    pub untrusted: bool,
}

fn default_mode() -> GrantMode {
    GrantMode::Allow
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Option<Value>,
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct McpClient {
    pub name: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Mutex<Child>,
    pub tools: Vec<McpTool>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl McpClient {
    pub async fn start(name: &str, cfg: &McpServerConfig, cwd: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let program = if cfg.command.contains('/') {
            std::path::PathBuf::from(&cfg.command)
        } else {
            crate::capability::resolve_program(&cfg.command)
                .with_context(|| format!("mcp server {name}: `{}` not found in the system PATH", cfg.command))?
        };
        std::fs::create_dir_all(cwd)?;
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&cfg.args)
            .env_clear()
            .env("PATH", crate::capability::SAFE_PATH.join(":"))
            .env("HOME", cwd)
            .env("LANG", "C.UTF-8")
            .envs(&cfg.env)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting mcp server {name}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let pending: Pending = Arc::default();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (p, a, n) = (pending.clone(), alive.clone(), name.to_string());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    debug!(server = %n, "non-JSON line on stdout");
                    continue;
                };
                let id = msg.get("id").and_then(Value::as_u64);
                let is_response = msg.get("result").is_some() || msg.get("error").is_some();
                match (id, is_response) {
                    (Some(id), true) => {
                        if let Some(tx) = p.lock().unwrap().remove(&id) {
                            let r = match msg.get("error") {
                                Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                                None => Ok(msg["result"].clone()),
                            };
                            let _ = tx.send(r);
                        }
                    }
                    // Server->client requests (sampling, roots, elicitation) are not supported;
                    // they are ignored here and the server sees a timeout on its side.
                    _ => debug!(server = %n, method = ?msg.get("method"), "ignored server message"),
                }
            }
            a.store(false, Ordering::SeqCst);
            // Fail everything still waiting.
            for (_, tx) in p.lock().unwrap().drain() {
                let _ = tx.send(Err("mcp server exited".into()));
            }
        });
        let n2 = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(server = %n2, "stderr: {line}");
            }
        });

        let mut client = Self {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
            tools: Vec::new(),
            alive,
        };

        let init = tokio::time::timeout(
            INIT_TIMEOUT,
            client.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "systema", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("mcp server {name}: initialize timed out"))??;
        let version = init["protocolVersion"].as_str().unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&version) {
            bail!("mcp server {name}: unsupported protocol version {version:?}");
        }
        client.notify("notifications/initialized", json!({})).await?;

        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = tokio::time::timeout(INIT_TIMEOUT, client.request("tools/list", params))
                .await
                .map_err(|_| anyhow::anyhow!("mcp server {name}: tools/list timed out"))??;
            let batch: Vec<McpTool> = serde_json::from_value(page["tools"].clone()).unwrap_or_default();
            tools.extend(batch);
            cursor = page["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        if let Some(allow) = &cfg.tools {
            tools.retain(|t| allow.contains(&t.name));
        }
        info!(server = %name, version, tools = tools.len(), "mcp server ready");
        client.tools = tools;
        Ok(Arc::new(client))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn send(&self, msg: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        rx.await.map_err(|_| anyhow::anyhow!("mcp server {} went away", self.name))?.map_err(|e| anyhow::anyhow!(e))
    }

    /// `tools/call`, rendered as text for the model.
    pub async fn call(&self, tool: &str, arguments: Value) -> anyhow::Result<String> {
        let r = tokio::time::timeout(CALL_TIMEOUT, self.request("tools/call", json!({ "name": tool, "arguments": arguments })))
            .await
            .map_err(|_| anyhow::anyhow!("mcp call timed out after {} s", CALL_TIMEOUT.as_secs()))??;
        let mut out = String::new();
        for item in r["content"].as_array().into_iter().flatten() {
            match item["type"].as_str() {
                Some("text") => out.push_str(item["text"].as_str().unwrap_or("")),
                Some(other) => out.push_str(&format!("[{other} content omitted]")),
                None => {}
            }
            out.push('\n');
        }
        if out.trim().is_empty()
            && let Some(s) = r.get("structuredContent")
        {
            out = s.to_string();
        }
        if out.len() > RESULT_CAP {
            out.truncate(out.floor_char_boundary(RESULT_CAP));
            out.push_str("\n[truncated]");
        }
        if r["isError"].as_bool() == Some(true) {
            return Ok(format!("tool reported an error: {}", out.trim()));
        }
        Ok(out.trim_end().to_string())
    }
}

/// Starts servers on first use and restarts ones that died.
#[derive(Default)]
pub struct McpManager {
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
}

impl McpManager {
    pub async fn get(
        &self,
        name: &str,
        cfg: &McpServerConfig,
        cwd: &std::path::Path,
    ) -> anyhow::Result<Arc<McpClient>> {
        let mut map = self.clients.lock().await;
        if let Some(c) = map.get(name).filter(|c| c.is_alive()) {
            return Ok(c.clone());
        }
        let c = McpClient::start(name, cfg, cwd).await.inspect_err(|e| warn!("{e:#}"))?;
        map.insert(name.to_string(), c.clone());
        Ok(c)
    }

    /// Drop a cached client, if any. `kill_on_drop` stops its process once the last `Arc`
    /// reference goes away, so a config change (`Control::McpAdd`) or removal
    /// (`Control::McpRemove`) takes effect immediately rather than waiting for the next failed
    /// call to notice the server is gone or different.
    pub async fn remove(&self, name: &str) {
        self.clients.lock().await.remove(name);
    }
}

/// OpenAI function names allow `[a-zA-Z0-9_-]{1,64}`.
pub fn function_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();
    let mut n = format!("mcp__{}__{}", clean(server), clean(tool));
    n.truncate(64);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Option<McpServerConfig> {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/mcp_notes_server.py");
        crate::capability::resolve_program("python3")?;
        Some(McpServerConfig {
            command: "python3".into(),
            args: vec!["-I".into(), script.into()],
            env: BTreeMap::new(),
            tools: None,
            ask_tools: vec![],
            mode: GrantMode::Allow,
            private: false,
            outbound: false,
            untrusted: true,
        })
    }

    #[tokio::test]
    async fn talks_to_a_stdio_server() {
        let Some(cfg) = fixture() else { return };
        let dir = std::env::temp_dir().join(format!("systema-mcp-{}", systema_proto::random_id()));
        let c = McpClient::start("notes", &cfg, &dir).await.unwrap();
        let names: Vec<_> = c.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"add_note") && names.contains(&"list_notes"), "{names:?}");
        assert!(c.call("add_note", json!({ "text": "buy milk" })).await.unwrap().contains("saved"));
        assert!(c.call("list_notes", json!({})).await.unwrap().contains("buy milk"));
        assert!(c.call("no_such_tool", json!({})).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn allowlist_filters_tools() {
        let Some(mut cfg) = fixture() else { return };
        cfg.tools = Some(vec!["list_notes".into()]);
        let dir = std::env::temp_dir().join(format!("systema-mcp-{}", systema_proto::random_id()));
        let c = McpClient::start("notes", &cfg, &dir).await.unwrap();
        assert_eq!(c.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["list_notes"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn function_names_are_openai_safe() {
        assert_eq!(function_name("my.server", "do thing"), "mcp__my_server__do_thing");
        assert!(function_name(&"x".repeat(80), "y").len() <= 64);
    }
}
