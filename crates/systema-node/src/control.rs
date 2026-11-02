//! Local control socket (`$SYSTEMA_HOME/control.sock`, mode 0600): how the `systema` CLI
//! talks to a running node. One JSON request line in, one JSON response line out.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Control {
    /// Discovered peers and their agent cards.
    Peers,
    /// Send a request to `agent` on `peer`, invoking `skill`.
    Send { peer: String, agent: String, skill: String, text: String },
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
}

pub type ControlReply = Result<serde_json::Value, String>;
pub type ControlTx = mpsc::Sender<(Control, oneshot::Sender<ControlReply>)>;

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("control.sock")
}

/// Bind the socket and forward each request into the node loop.
pub fn serve(home: &Path, tx: ControlTx) -> anyhow::Result<()> {
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
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = handle(stream, tx).await;
            });
        }
    });
    Ok(())
}

async fn handle(stream: UnixStream, tx: ControlTx) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let reply: ControlReply = match serde_json::from_str::<Control>(&line) {
        Ok(cmd) => {
            let (otx, orx) = oneshot::channel();
            tx.send((cmd, otx)).await.map_err(|_| anyhow::anyhow!("node stopped"))?;
            orx.await.unwrap_or_else(|_| Err("node dropped the request".into()))
        }
        Err(e) => Err(format!("bad control request: {e}")),
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    w.write_all(&out).await?;
    Ok(())
}

/// Client side: send one command to the running node.
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
