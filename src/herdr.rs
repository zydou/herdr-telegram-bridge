//! The herdr CLI wrapper — `herdr agent list`, the one call the feed makes.
//!
//! herdr reports failures as `{"error": {...}}` on stdout *with exit code 0*,
//! so the parsed error string — not the exit code — is what callers check.
//! The binary is re-resolved on every call: HERDR_BIN_PATH is snapshotted into
//! the daemon's environment at spawn, so a managed install (mise) can upgrade
//! herdr and delete the version that path named — re-resolving lets the
//! daemon recover without a restart.

use crate::resolve::Agent;
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

fn herdr_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_BIN_PATH") {
        let p = PathBuf::from(&p);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("herdr") // resolved through PATH
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    result: ResultBody,
}

#[derive(Deserialize, Default)]
struct ResultBody {
    #[serde(default)]
    agents: Vec<Agent>,
}

/// List every pane herdr tracks; Err when the CLI fails, times out or answers
/// something unparseable. The daemon keeps sweeping its frozen listing.
pub fn agent_list() -> Result<Vec<Agent>, String> {
    let mut child = Command::new(herdr_bin())
        .args(["agent", "list"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn herdr: {e}"))?;

    // drain stdout off-thread so a chatty child can never fill the pipe and
    // deadlock against our try_wait polling
    let mut pipe = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + LIST_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("herdr agent list: timeout after 30s".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("herdr agent list: {e}")),
        }
    };
    let stdout = reader.join().unwrap_or_default();

    if !status.success() {
        return Err(format!(
            "herdr agent list: exit {}",
            status.code().unwrap_or(-1)
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("herdr agent list: bad json: {e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .or_else(|| err.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("herdr error");
        return Err(msg.to_string());
    }
    let env: Envelope = serde_json::from_value(v).map_err(|e| format!("herdr agent list: {e}"))?;
    Ok(env.result.agents)
}
