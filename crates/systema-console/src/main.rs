//! Systema console: a browser dashboard over the running node's control socket.
//!
//! Exposes the same primitives the TUI and CLI already use (`systema_node::control::Control`)
//! as a small HTTP + Server-Sent-Events API, plus a single embedded dashboard page. No new
//! security surface is created: every request is forwarded verbatim to the control socket
//! (`$SYSTEMA_HOME/control.sock`, mode 0600), so the console can only ever do what `systema` and
//! `systema-tui` could already do over that socket -- it inherits the node's existing trust gate
//! and capability broker rather than bypassing them.
//!
//! This is the "console" half of folding iii's composable workers/functions/triggers model into
//! Systema (see docs/IIi-FUSION.md): a live, browsable, HTTP-reachable surface onto the kernel,
//! shaped like iii's own web console (worker catalog, trace explorer, approvals) but built
//! natively on Systema's existing control plane -- no new dependency on iii at runtime.
//!
//! Run alongside `systema node`:
//! ```sh
//! systema-console --home ~/.systema --port 8787
//! ```

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use systema_kernel::Answer;
use systema_node::control::{self, Control};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt as _};

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Parser, Debug)]
#[command(name = "systema-console", about = "Browser dashboard for a running Systema node")]
struct Args {
    /// Systema state directory (default $SYSTEMA_HOME or ~/.systema).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Address to bind the HTTP server to.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 8787)]
    port: u16,
}

#[derive(Clone)]
struct AppState {
    home: Arc<PathBuf>,
}

fn home_dir(arg: Option<PathBuf>) -> PathBuf {
    if let Some(p) = arg {
        return p;
    }
    if let Some(p) = std::env::var_os("SYSTEMA_HOME") {
        return PathBuf::from(p);
    }
    let mut p = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    p.push(".systema");
    p
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let home = home_dir(args.home);
    if !control::socket_path(&home).exists() {
        tracing::warn!(
            "no control socket at {} yet -- start `systema node --home {}` first",
            control::socket_path(&home).display(),
            home.display()
        );
    }
    let state = AppState { home: Arc::new(home) };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/agents", get(agents))
        .route("/api/peers", get(peers))
        .route("/api/sessions", get(sessions))
        .route("/api/approvals", get(approvals))
        .route("/api/inbox", get(inbox))
        .route("/api/grants", get(grants))
        .route("/api/rules", get(rules))
        .route("/api/ask", post(ask))
        .route("/api/steer", post(steer))
        .route("/api/cancel", post(cancel))
        .route("/api/resolve", post(resolve))
        .route("/api/approve", post(approve))
        .route("/api/reject", post(reject))
        .route("/api/block", post(block))
        .route("/api/send", post(send))
        .route("/api/find", post(find))
        .route("/api/forget", post(forget))
        .route("/api/webhook/{name}", post(webhook))
        .route("/api/mcp-servers", get(mcp_servers).post(mcp_add))
        .route("/api/mcp-servers/{name}", axum::routing::delete(mcp_remove))
        .route("/api/providers", get(providers).post(provider_add))
        .route("/api/providers/{name}", axum::routing::delete(provider_remove))
        .route("/api/providers/test", post(provider_test_default))
        .route("/api/providers/{name}/test", post(provider_test))
        .route("/api/history", get(history))
        .route("/api/history/{id}", get(replay))
        .route("/api/workspace/{agent}", get(workspace_list))
        .route("/api/workspace/{agent}/file", get(workspace_read).post(workspace_write))
        .route("/api/events", get(events))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    tracing::info!("systema-console listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Forward one `Control` command to the running node and return its JSON reply.
async fn call(state: AppState, cmd: Control) -> Result<Json<Value>, (StatusCode, String)> {
    control::request(&state.home, &cmd).await.map(Json).map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

async fn status(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Status).await
}

async fn agents(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Agents).await
}

async fn peers(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Peers).await
}

async fn sessions(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Sessions).await
}

async fn approvals(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Approvals).await
}

async fn inbox(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Inbox).await
}

async fn grants(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Grants).await
}

async fn rules(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Rules).await
}

#[derive(Deserialize)]
struct AskReq {
    agent: String,
    text: String,
}
async fn ask(State(state): State<AppState>, Json(req): Json<AskReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Ask { agent: req.agent, text: req.text }).await
}

#[derive(Deserialize)]
struct SteerReq {
    session: String,
    text: String,
}
async fn steer(State(state): State<AppState>, Json(req): Json<SteerReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Steer { session: req.session, text: req.text }).await
}

#[derive(Deserialize)]
struct CancelReq {
    session: String,
}
async fn cancel(State(state): State<AppState>, Json(req): Json<CancelReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Cancel { session: req.session }).await
}

#[derive(Deserialize)]
struct ResolveReq {
    id: String,
    answer: Answer,
    remember: bool,
}
async fn resolve(State(state): State<AppState>, Json(req): Json<ResolveReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Resolve { id: req.id, answer: req.answer, remember: req.remember }).await
}

#[derive(Deserialize)]
struct ApproveReq {
    id: String,
    per_hour: u32,
    hours: Option<u64>,
}
async fn approve(State(state): State<AppState>, Json(req): Json<ApproveReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Approve { id: req.id, per_hour: req.per_hour, hours: req.hours }).await
}

#[derive(Deserialize)]
struct RejectReq {
    id: String,
}
async fn reject(State(state): State<AppState>, Json(req): Json<RejectReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Reject { id: req.id }).await
}

#[derive(Deserialize)]
struct BlockReq {
    peer: String,
}
async fn block(State(state): State<AppState>, Json(req): Json<BlockReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Block { peer: req.peer }).await
}

#[derive(Deserialize)]
struct SendReq {
    peer: String,
    agent: String,
    skill: String,
    text: String,
}
async fn send(State(state): State<AppState>, Json(req): Json<SendReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Send { peer: req.peer, agent: req.agent, skill: req.skill, text: req.text }).await
}

#[derive(Deserialize)]
struct FindReq {
    skill: String,
}
async fn find(State(state): State<AppState>, Json(req): Json<FindReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Find { skill: req.skill }).await
}

#[derive(Deserialize)]
struct ForgetReq {
    agent: String,
    key: String,
}
async fn forget(State(state): State<AppState>, Json(req): Json<ForgetReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Forget { agent: req.agent, key: req.key }).await
}

/// Fire a named webhook trigger -- `POST /api/webhook/<name>` -- for any `on_http = "<name>"`
/// job in `schedules.toml`. The path segment is the whole request; no body is required.
async fn webhook(State(state): State<AppState>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::FireWebhook { name }).await
}

async fn mcp_servers(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::McpServers).await
}

#[derive(Deserialize)]
struct McpAddReq {
    name: String,
    config: systema_kernel::mcp::McpServerConfig,
}
/// `POST /api/mcp-servers` -- add or replace an MCP server at runtime (Phase 4 of
/// docs/IIi-FUSION.md). Still capability-gated: an agent can only reach `name` once its own
/// manifest already lists it under `capabilities.mcp`.
async fn mcp_add(State(state): State<AppState>, Json(req): Json<McpAddReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::McpAdd { name: req.name, config: req.config }).await
}

async fn mcp_remove(State(state): State<AppState>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::McpRemove { name }).await
}

// --- Phase 5: provider/model management --------------------------------------------------

async fn providers(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Providers).await
}

#[derive(Deserialize)]
struct ProviderAddReq {
    name: String,
    config: systema_kernel::config::ProviderConfig,
}
async fn provider_add(State(state): State<AppState>, Json(req): Json<ProviderAddReq>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::ProviderAdd { name: req.name, config: req.config }).await
}

async fn provider_remove(State(state): State<AppState>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::ProviderRemove { name }).await
}

async fn provider_test(State(state): State<AppState>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::ProviderTest { name: Some(name) }).await
}

async fn provider_test_default(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::ProviderTest { name: None }).await
}

// --- Phase 6: chat history / transcript ---------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    agent: Option<String>,
    #[serde(default = "default_history_limit")]
    limit: usize,
}
fn default_history_limit() -> usize {
    50
}
async fn history(State(state): State<AppState>, Query(q): Query<HistoryQuery>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::History { agent: q.agent, limit: q.limit }).await
}

async fn replay(State(state): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::Replay { id }).await
}

// --- Phase 7: workspace / project file browser --------------------------------------------

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}
async fn workspace_list(
    State(state): State<AppState>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::WorkspaceList { agent, path: q.path }).await
}

async fn workspace_read(
    State(state): State<AppState>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::WorkspaceRead { agent, path: q.path }).await
}

#[derive(Deserialize)]
struct WorkspaceWriteReq {
    path: String,
    content_base64: String,
}
async fn workspace_write(
    State(state): State<AppState>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Json(req): Json<WorkspaceWriteReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    call(state, Control::WorkspaceWrite { agent, path: req.path, content_base64: req.content_base64 }).await
}

/// Live trace: bridges the control socket's `Watch` line stream to the browser as
/// Server-Sent-Events, one event per kernel `Event` JSON line -- the same feed the TUI renders,
/// now reachable from any browser on the console's bind address.
async fn events(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let home = (*state.home).clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(async move {
        match control::watch(&home).await {
            Ok(mut lines) => loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if tx.send(line).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            },
            Err(e) => {
                let _ = tx.send(format!("{{\"error\":{:?}}}", e.to_string())).await;
            }
        }
    });
    let stream = ReceiverStream::new(rx).map(|line| Ok(SseEvent::default().data(line)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
