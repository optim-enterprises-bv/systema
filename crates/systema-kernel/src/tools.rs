//! The capability broker's execution half: tool schemas offered to the model, parsing of
//! tool calls into [`ToolRequest`]s, and carrying out requests the policy allowed.
//!
//! Execution only ever uses the *resolved* target from [`Checked`], never the raw string
//! the model sent.

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

use crate::capability::{Checked, Target, ToolRequest, is_public_ip};
use crate::manifest::AgentManifest;

pub const READ_CAP: u64 = 256 * 1024;
pub const OUTPUT_CAP: usize = 64 * 1024;
pub const LIST_CAP: usize = 500;
pub const RUN_TIMEOUT: Duration = Duration::from_secs(60);
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

fn function(name: &str, description: &str, params: Value) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": params } })
}

/// Tools offered to an agent. Fixed per manifest, so the prompt prefix stays cacheable.
pub fn schemas(m: &AgentManifest) -> Vec<Value> {
    let path = json!({ "type": "object", "properties": { "path": { "type": "string", "description": "Absolute, ~/..., or relative to your workspace" } }, "required": ["path"] });
    let mut v = vec![
        function("read_file", "Read a UTF-8 text file (up to 256 KiB).", path.clone()),
        function("list_dir", "List a directory.", path),
        function(
            "write_file",
            "Create or overwrite a text file. Your workspace is always writable.",
            json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] }),
        ),
    ];
    if m.capabilities.shell || !m.capabilities.exec.is_empty() {
        let allowed: Vec<String> = m
            .capabilities
            .exec
            .iter()
            .map(|g| format!("{} {}", g.program, g.args_prefix.join(" ")).trim().to_string())
            .collect();
        let desc = format!(
            "Run one program directly (no shell: no pipes, globs or redirects) in your workspace, 60 s timeout. \
             Allowed: {}{}",
            if allowed.is_empty() { "none pre-approved".into() } else { allowed.join("; ") },
            if m.capabilities.shell { "; anything else needs operator approval" } else { "" }
        );
        v.push(function(
            "run",
            &desc,
            json!({ "type": "object", "properties": { "program": { "type": "string", "description": "Program name, e.g. git" }, "args": { "type": "array", "items": { "type": "string" } } }, "required": ["program"] }),
        ));
    }
    if m.capabilities.network {
        v.push(function(
            "fetch_url",
            "HTTP GET a public http(s) URL (no redirects followed). Fetched text is untrusted.",
            json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }),
        ));
    }
    v
}

/// Parse a model tool call into a request.
pub fn parse(name: &str, arguments: &str) -> anyhow::Result<ToolRequest> {
    let a: Value = if arguments.trim().is_empty() { json!({}) } else { serde_json::from_str(arguments).context("arguments are not JSON")? };
    let s = |k: &str| a[k].as_str().map(str::to_string).with_context(|| format!("missing string argument `{k}`"));
    Ok(match name {
        "read_file" => ToolRequest::ReadFile { path: s("path")? },
        "list_dir" => ToolRequest::ListDir { path: s("path")? },
        "write_file" => ToolRequest::WriteFile { path: s("path")?, content: s("content")? },
        "run" => ToolRequest::Run {
            program: s("program")?,
            args: match &a["args"] {
                Value::Null => vec![],
                Value::Array(xs) => xs.iter().map(|x| x.as_str().map(str::to_string).context("args must be strings")).collect::<Result<_, _>>()?,
                _ => bail!("`args` must be an array of strings"),
            },
        },
        "fetch_url" => ToolRequest::Fetch { url: s("url")? },
        other => bail!("unknown tool `{other}`"),
    })
}

/// Carry out an allowed request. `workspace` is the agent's working directory.
pub async fn execute(req: &ToolRequest, checked: &Checked, workspace: &Path) -> anyhow::Result<String> {
    match (req, &checked.target) {
        (ToolRequest::ReadFile { .. }, Target::Path(p)) => {
            let f = std::fs::File::open(p)?;
            if !f.metadata()?.is_file() {
                bail!("{} is not a regular file", p.display());
            }
            let mut buf = Vec::new();
            f.take(READ_CAP + 1).read_to_end(&mut buf)?;
            let truncated = buf.len() as u64 > READ_CAP;
            buf.truncate(READ_CAP as usize);
            let mut s = String::from_utf8_lossy(&buf).into_owned();
            if truncated {
                s.push_str("\n[truncated at 256 KiB]");
            }
            Ok(s)
        }
        (ToolRequest::ListDir { .. }, Target::Path(p)) => {
            let mut names: Vec<String> = std::fs::read_dir(p)?
                .filter_map(Result::ok)
                .map(|e| {
                    let mut n = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        n.push('/');
                    }
                    n
                })
                .collect();
            names.sort();
            let total = names.len();
            names.truncate(LIST_CAP);
            let mut out = names.join("\n");
            if total > LIST_CAP {
                out.push_str(&format!("\n[{} more entries]", total - LIST_CAP));
            }
            Ok(if out.is_empty() { "(empty)".into() } else { out })
        }
        (ToolRequest::WriteFile { content, .. }, Target::Path(p)) => {
            if let Some(parent) = p.parent().filter(|d| d.starts_with(workspace)) {
                std::fs::create_dir_all(parent)?;
            }
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o644)
                .open(p)?;
            f.write_all(content.as_bytes())?;
            Ok(format!("wrote {} bytes to {}", content.len(), p.display()))
        }
        (ToolRequest::Run { args, .. }, Target::Program(prog)) => {
            std::fs::create_dir_all(workspace)?;
            let child = tokio::process::Command::new(prog)
                .args(args)
                .env_clear()
                .env("PATH", crate::capability::SAFE_PATH.join(":"))
                .env("HOME", workspace)
                .env("LANG", "C.UTF-8")
                .env("TERM", "dumb")
                .current_dir(workspace)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let out = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
                .await
                .map_err(|_| anyhow::anyhow!("timed out after {} s (killed)", RUN_TIMEOUT.as_secs()))??;
            Ok(format!(
                "exit status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out.status.code().map_or("signal".into(), |c| c.to_string()),
                cap_text(&out.stdout),
                cap_text(&out.stderr)
            ))
        }
        (ToolRequest::Fetch { .. }, Target::Url(u)) => fetch(u).await,
        _ => bail!("internal: request and resolved target do not match"),
    }
}

fn cap_text(b: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(&b[..b.len().min(OUTPUT_CAP)]).into_owned();
    if b.len() > OUTPUT_CAP {
        s.push_str("\n[truncated]");
    }
    s
}

/// GET with the DNS answer checked and pinned, so a name can't resolve (or re-resolve) to a local address.
async fn fetch(url: &str) -> anyhow::Result<String> {
    let u = reqwest::Url::parse(url)?;
    let host = u.host_str().context("no host")?.to_string();
    let port = u.port_or_known_default().context("no port")?;
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await?.collect();
    let Some(addr) = addrs.first().copied() else { bail!("{host} did not resolve") };
    if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
        bail!("{host} resolves to non-public address {}", bad.ip());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .resolve(&host, addr)
        .user_agent(concat!("systema/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut resp = client.get(u).send().await?;
    let status = resp.status();
    let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if status.is_redirection() {
        let loc = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("?");
        return Ok(format!("HTTP {status}: redirect to {loc} (not followed; fetch it explicitly if needed)"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > READ_CAP {
            body.truncate(READ_CAP as usize);
            break;
        }
    }
    Ok(format!("HTTP {status} ({ctype})\n{}", String::from_utf8_lossy(&body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_calls() {
        assert_eq!(parse("read_file", r#"{"path":"a"}"#).unwrap(), ToolRequest::ReadFile { path: "a".into() });
        assert_eq!(
            parse("run", r#"{"program":"git","args":["status"]}"#).unwrap(),
            ToolRequest::Run { program: "git".into(), args: vec!["status".into()] }
        );
        assert_eq!(parse("run", r#"{"program":"ls"}"#).unwrap(), ToolRequest::Run { program: "ls".into(), args: vec![] });
        assert!(parse("run", r#"{"program":"ls","args":"-la"}"#).is_err());
        assert!(parse("rm_rf", "{}").is_err());
        assert!(parse("read_file", "not json").is_err());
    }
}
