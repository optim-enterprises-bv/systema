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

use axum::extract::State;
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
