//! Local control socket (`$SYSTEMA_HOME/control.sock`, mode 0600): how the CLI and the TUI
//! drive a running node. Most commands are one JSON request line in, one JSON response line
//! out. [`Control::Watch`] instead streams kernel [`Event`]s as JSON lines until the client
//! disconnects.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use systema_kernel::Answer;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Control {
    /// Discovered peers and their agent cards.
    Peers,
    /// Send a request to `agent` on `peer`, invoking `skill`.
    Send { peer: String, agent: String, skill: String, text: String },
    /// Look up agents offering `skill` across the swarm (DHT providers + direct card fetch).
    Find { skill: String },
    /// Requests from unknown peers waiting for approval.
    Inbox,
    /// Approve an inbox item: grant its peer its skill.
    Approve { id: String, per_hour: u32, hours: Option<u64> },
    /// Drop an inbox item without granting anything.
    Reject { id: String },
    /// Block a peer: revoke its grants and drop its inbox items.
    Block { peer: String },
    /// Current grants and blocklist.
    Grants,

    // --- agent control plane ---
    /// Start a turn: ask a local agent in the operator's session.
    Ask { agent: String, text: String },
    /// Orchestrate: pick the best agent for a request (returns `{agent}`; does not run it).
    Route { text: String },
    /// Inject operator guidance into a running session (or start one if idle).
    Steer { session: String, text: String },
    /// Cancel a running turn.
    Cancel { session: String },
    /// Snapshot of running/known sessions.
    Sessions,
    /// Answer a pending tool approval.
    Resolve { id: String, answer: Answer, remember: bool },
    /// Tool approvals currently waiting for the operator.
    Approvals,
    /// Remembered "always allow" rules.
    Rules,
    /// Forget a remembered rule.
    Forget { agent: String, key: String },
    /// Loaded agents and their capabilities/skills.
    Agents,
    /// Node vitals for status panels: peer id, addresses, counts.
    Status,
    /// Stream kernel events as JSON lines until the client disconnects.
    Watch,
}

pub type ControlReply = Result<serde_json::Value, String>;
pub type ControlTx = mpsc::Sender<(Control, oneshot::Sender<ControlReply>)>;

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("control.sock")
}

/// Bind the socket. Commands go to the node loop via `tx`; `events` feeds `Watch` streams.
pub fn serve(home: &Path, tx: ControlTx, events: broadcast::Sender<systema_kernel::Event>) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = socket_path(home);
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another node is already running ({})", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let (tx, events) = (tx.clone(), events.clone());
            tokio::spawn(async move {
                let _ = handle(stream, tx, events).await;
            });
        }
    });
    Ok(())
}

async fn handle(stream: UnixStream, tx: ControlTx, events: broadcast::Sender<systema_kernel::Event>) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let cmd = match serde_json::from_str::<Control>(&line) {
        Ok(c) => c,
        Err(e) => {
            let reply: ControlReply = Err(format!("bad control request: {e}"));
            let mut out = serde_json::to_vec(&reply)?;
            out.push(b'\n');
            w.write_all(&out).await?;
            return Ok(());
        }
    };
    if matches!(cmd, Control::Watch) {
        let mut rx = events.subscribe();
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let mut buf = serde_json::to_vec(&ev)?;
                    buf.push(b'\n');
                    if w.write_all(&buf).await.is_err() {
                        break; // client went away
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        return Ok(());
    }
    let reply: ControlReply = {
        let (otx, orx) = oneshot::channel();
        match tx.send((cmd, otx)).await {
            Ok(()) => orx.await.unwrap_or_else(|_| Err("node dropped the request".into())),
            Err(_) => Err("node stopped".into()),
        }
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    w.write_all(&out).await?;
    Ok(())
}

/// Client side: send one command to the running node and read its single reply.
pub async fn request(home: &Path, cmd: &Control) -> anyhow::Result<serde_json::Value> {
    let path = socket_path(home);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("no running node at {} (start one with `systema node`)", path.display()))?;
    let (r, mut w) = stream.into_split();
    let mut out = serde_json::to_vec(cmd)?;
    out.push(b'\n');
    w.write_all(&out).await?;
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let reply: ControlReply = serde_json::from_str(&line).context("bad reply from node")?;
    reply.map_err(|e| anyhow::anyhow!(e))
}

/// Client side: open an event stream. Returns a line reader of JSON [`Event`]s.
pub async fn watch(home: &Path) -> anyhow::Result<Lines<BufReader<OwnedReadHalf>>> {
    let path = socket_path(home);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("no running node at {} (start one with `systema node`)", path.display()))?;
    let (r, mut w) = stream.into_split();
    w.write_all(b"{\"cmd\":\"watch\"}\n").await?;
    w.flush().await?;
    // Dropping the write half only shuts down our write direction (SHUT_WR); the read half
    // stays open, so the node keeps streaming events to us until we drop the reader.
    drop(w);
    Ok(BufReader::new(r).lines())
}
