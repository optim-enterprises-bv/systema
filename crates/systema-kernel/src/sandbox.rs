//! microVM isolation for swarm-facing (public) agents. A public agent's shell runs inside a
//! libkrun microVM (`podman run --runtime krun`), not on the host — a different guest kernel,
//! hardware-isolated, with no host filesystem except the agent's own workspace, and no network.
//! So a stranger's agent can be given a real shell without being able to touch your machine.
//!
//! If the runtime isn't present the broker denies a public shell outright (never host execution),
//! so enabling the sandbox only ever *adds* capability — it never weakens isolation.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::config::SandboxConfig;

/// Whether the microVM runtime is usable: `podman` on PATH and the `krun` crun variant present.
pub fn available() -> bool {
    which("podman") && (which("krun") || which("crun-krun"))
}

fn which(bin: &str) -> bool {
    crate::capability::SAFE_PATH.iter().any(|d| Path::new(d).join(bin).is_file())
}

/// Run `command` in a microVM, mounting `workspace` as the only writable host path (`/work`),
/// with no network. Returns the combined, capped output. Errors only on infrastructure failure;
/// a non-zero exit from the command is reported in the returned text, like the host shell.
pub async fn run(cfg: &SandboxConfig, workspace: &Path, command: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(workspace)?;
    let ws = workspace.to_string_lossy();
    // --network=none: no exfiltration path from a stranger's shell. Only the agent's workspace is
    // mounted, SELinux-relabelled (:Z). --rm so nothing persists beyond the workspace.
    let child = tokio::process::Command::new("podman")
        .args([
            "run",
            "--rm",
            "--runtime",
            &cfg.runtime,
            "--network=none",
            "-v",
            &format!("{ws}:/work:Z"),
            "-w",
            "/work",
            &cfg.image,
            "/bin/sh",
            "-lc",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = tokio::time::timeout(Duration::from_secs(cfg.timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("sandboxed command timed out after {}s (killed)", cfg.timeout_secs))??;
    Ok(format!(
        "[microVM] exit status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code().map_or("signal".into(), |c| c.to_string()),
        cap(&out.stdout),
        cap(&out.stderr),
    ))
}

const OUTPUT_CAP: usize = 64 * 1024;

fn cap(b: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(&b[..b.len().min(OUTPUT_CAP)]).into_owned();
    if b.len() > OUTPUT_CAP {
        s.push_str("\n[truncated]");
    }
    s
}
