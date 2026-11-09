//! The Engine tab's data: a background task that talks to the engine through the SDK and hands
//! snapshots to the UI over a channel, so drawing never waits on the network.
//!
//! Functions used (all existing engine/worker functions): `engine::workers::list`,
//! `session::list`, `session::messages-tail`, `harness::send`, and any function the user calls by id.

use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // version/runtime are kept for the worker detail view
pub struct WorkerRow {
    pub name: String,
    pub status: String,
    pub version: String,
    pub runtime: String,
    pub functions: u64,
    pub active: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub messages: u64,
    pub updated: u64,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct MsgRow {
    pub role: String,
    pub text: String,
}

pub enum Cmd {
    /// The session whose transcript to keep fresh (None: no transcript).
    Select(Option<String>),
    /// Send a prompt, to `session` or (None) a new session.
    Send { session: Option<String>, text: String },
    /// Call a function by id with a JSON payload.
    Call { function: String, payload: Value },
    /// Poll now.
    Refresh,
}

pub enum Msg {
    Online(bool),
    Workers(Vec<WorkerRow>),
    Sessions(Vec<SessionRow>),
    Messages { session: String, rows: Vec<MsgRow> },
    /// A session was created or targeted by a send: the UI selects it.
    Opened(String),
    Toast(String),
    CallResult { function: String, text: String },
}

pub fn engine_url() -> String {
    std::env::var("SYSTEMA_ENGINE_URL")
        .or_else(|_| std::env::var("SYSTEMA_URL"))
        .unwrap_or_else(|_| "ws://127.0.0.1:49134".to_string())
}

/// Spawn the poller. Returns the command sender and the message receiver.
pub fn spawn(url: String) -> (mpsc::UnboundedSender<Cmd>, mpsc::UnboundedReceiver<Msg>) {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Msg>();
    tokio::spawn(async move {
        let mut client = Wire { url, ws: None };
        let mut selected: Option<String> = None;
        let mut last_model: Option<(String, String)> = None; // (provider, model) of the newest session
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = tick.tick() => poll(&mut client, &msg_tx, selected.as_deref(), &mut last_model).await,
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Cmd::Select(s) => { selected = s; poll(&mut client, &msg_tx, selected.as_deref(), &mut last_model).await }
                        Cmd::Refresh => poll(&mut client, &msg_tx, selected.as_deref(), &mut last_model).await,
                        Cmd::Send { session, text } => {
                            let mut payload = json!({ "message": text });
                            if let Some(id) = &session {
                                payload["session_id"] = json!(id);
                            } else if let Some((provider, model)) = &last_model {
                                payload["provider"] = json!(provider);
                                payload["model"] = json!(model);
                            }
                            match call(&mut client, "harness::send", payload).await {
                                Ok(v) => {
                                    if let Some(id) = v["session_id"].as_str() {
                                        selected = Some(id.to_string());
                                        let _ = msg_tx.send(Msg::Opened(id.to_string()));
                                    }
                                    let _ = msg_tx.send(Msg::Toast("sent".into()));
                                    poll(&mut client, &msg_tx, selected.as_deref(), &mut last_model).await;
                                }
                                Err(e) => { let _ = msg_tx.send(Msg::Toast(format!("send failed: {e}"))); }
                            }
                        }
                        Cmd::Call { function, payload } => {
                            let text = match call(&mut client, &function, payload).await {
                                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
                                Err(e) => format!("error: {e}"),
                            };
                            let _ = msg_tx.send(Msg::CallResult { function, text });
                        }
                    }
                }
            }
        }
    });
    (cmd_tx, msg_rx)
}

/// One WebSocket to the engine, used one request at a time. It connects on demand and drops the
/// socket on any error, so the next call reconnects (an engine restart heals itself).
struct Wire {
    url: String,
    ws: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

async fn call(client: &mut Wire, function: &str, payload: Value) -> Result<Value, String> {
    let result = tokio::time::timeout(Duration::from_secs(6), client.invoke(function, payload)).await;
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            client.ws = None;
            Err(e)
        }
        Err(_) => {
            client.ws = None;
            Err("timed out waiting for the engine".into())
        }
    }
}

impl Wire {
    async fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, String> {
        if self.ws.is_none() {
            let (ws, _) = tokio::time::timeout(Duration::from_secs(2), connect_async(self.url.as_str()))
                .await
                .map_err(|_| "connect timed out".to_string())?
                .map_err(|e| e.to_string())?;
            self.ws = Some(ws);
        }
        let ws = self.ws.as_mut().ok_or("not connected")?;
        let id = uuid::Uuid::new_v4().to_string();
        let msg = json!({ "type": "invokefunction", "invocation_id": id, "function_id": function, "data": payload });
        ws.send(Ws::Text(msg.to_string().into())).await.map_err(|e| e.to_string())?;
        // Other frames (pings, notices) may arrive first: read until our answer.
        loop {
            match ws.next().await {
                Some(Ok(Ws::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                    if v["type"] == "invocationresult" && v["invocation_id"] == id.as_str() {
                        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                            let msg = err["message"].as_str().unwrap_or("error");
                            return Err(match err["code"].as_str() {
                                Some(code) => format!("{code}: {msg}"),
                                None => msg.to_string(),
                            });
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Some(Ok(Ws::Close(_))) | None => return Err("engine closed the connection".into()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.to_string()),
            }
        }
    }
}

async fn poll(client: &mut Wire, tx: &mpsc::UnboundedSender<Msg>, selected: Option<&str>, last_model: &mut Option<(String, String)>) {
    let workers = match call(client, "engine::workers::list", json!({})).await {
        Ok(v) => v,
        Err(_) => {
            let _ = tx.send(Msg::Online(false));
            return;
        }
    };
    let _ = tx.send(Msg::Online(true));
    let mut rows: Vec<WorkerRow> = workers["workers"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|w| WorkerRow {
                    name: w["name"].as_str().unwrap_or("?").to_string(),
                    status: w["status"].as_str().unwrap_or("").to_string(),
                    version: w["version"].as_str().unwrap_or("").to_string(),
                    runtime: w["runtime"].as_str().unwrap_or("").to_string(),
                    functions: w["function_count"].as_u64().unwrap_or(0),
                    active: w["active_invocations"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let _ = tx.send(Msg::Workers(rows));

    if let Ok(v) = call(client, "session::list", json!({ "limit": 40 })).await {
        let sessions: Vec<SessionRow> = v["sessions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| SessionRow {
                        id: s["session_id"].as_str().unwrap_or("").to_string(),
                        title: ["title", "description"]
                            .iter()
                            .filter_map(|k| s[*k].as_str().or_else(|| s["metadata"][*k].as_str()))
                            .find(|t| !t.is_empty())
                            .unwrap_or("(untitled)")
                            .to_string(),
                        messages: s["message_count"].as_u64().unwrap_or(0),
                        updated: s["updated_at"].as_u64().or_else(|| s["created_at"].as_u64()).unwrap_or(0),
                        model: s["metadata"]["model"].as_str().unwrap_or("").to_string(),
                    })
                    .filter(|s| !s.id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if last_model.is_none() {
            if let Some(m) = sessions.iter().find(|s| s.model.contains("::")) {
                if let Some((p, model)) = m.model.split_once("::") {
                    *last_model = Some((p.to_string(), model.to_string()));
                }
            }
        }
        let _ = tx.send(Msg::Sessions(sessions));
    }

    if let Some(id) = selected {
        if let Ok(v) = call(client, "session::messages-tail", json!({ "session_id": id, "limit": 60 })).await {
            let rows = v["messages"].as_array().map(|a| a.iter().filter_map(msg_row).collect()).unwrap_or_default();
            let _ = tx.send(Msg::Messages { session: id.to_string(), rows });
        }
    }
}

/// One transcript entry: a chat message (its text parts, other parts as a tag) or a custom notice.
fn msg_row(entry: &Value) -> Option<MsgRow> {
    if let Some(m) = entry.get("message").filter(|m| m.is_object()) {
        let role = m["role"].as_str().unwrap_or("?").to_string();
        let mut text = String::new();
        match &m["content"] {
            Value::Array(parts) => {
                for p in parts {
                    match p["type"].as_str() {
                        Some("text") | None => {
                            if let Some(t) = p["text"].as_str() {
                                text.push_str(t);
                            }
                        }
                        Some(other) => {
                            let name = p["name"].as_str().or_else(|| p["function_id"].as_str()).unwrap_or("");
                            text.push_str(&format!("[{other} {name}]"));
                        }
                    }
                }
            }
            Value::String(s) => text = s.clone(),
            _ => {}
        }
        return Some(MsgRow { role, text });
    }
    let data = &entry["custom"]["data"];
    let text = data["text"].as_str()?;
    Some(MsgRow { role: "notice".into(), text: text.to_string() })
}
