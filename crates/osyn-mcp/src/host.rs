//! Connection to the persistent Serum helper (`python -m serum_host.server`).
//!
//! Started on first use and kept alive, so only the first render pays Serum's ~10 s load.
//! Requests are serialized; a request that times out or a helper that exits is killed and
//! the next request starts a fresh helper. Helper stderr goes to `$TMPDIR/open-synth/host.log`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// The first request also waits for Serum to load.
const FIRST_TIMEOUT: Duration = Duration::from_secs(120);
const TIMEOUT: Duration = Duration::from_secs(60);

struct Helper {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    warm: bool,
}

static HOST: Mutex<Option<Helper>> = Mutex::const_new(None);

pub fn tools_dir() -> PathBuf {
    std::env::var_os("OSYN_TOOLS_DIR")
        .map_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools"), PathBuf::from)
}

pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("open-synth").join("host.log")
}

async fn spawn() -> Result<Helper, String> {
    let tools = tools_dir();
    let python = tools.join("serum_host/.venv/bin/python");
    if !python.exists() {
        return Err(format!(
            "this tool needs the Serum host environment: run `{}` once",
            tools.join("serum_host/setup.sh").display()
        ));
    }
    let log = log_path();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let stderr = std::fs::File::create(&log).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
    let mut child = tokio::process::Command::new(&python)
        .current_dir(&tools)
        .args(["-m", "serum_host.server"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start Serum helper: {e}"))?;
    let stdin = child.stdin.take().ok_or("helper has no stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("helper has no stdout")?).lines();
    Ok(Helper { _child: child, stdin, stdout, next_id: 0, warm: false })
}

impl Helper {
    async fn call(&mut self, cmd: &str, args: &Json) -> Result<Json, String> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"id": id, "cmd": cmd, "args": args}).to_string() + "\n";
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("helper write failed: {e}"))?;
        self.stdin.flush().await.map_err(|e| format!("helper write failed: {e}"))?;
        loop {
            let line = self
                .stdout
                .next_line()
                .await
                .map_err(|e| format!("helper read failed: {e}"))?
                .ok_or_else(|| format!("Serum helper exited (see {})", log_path().display()))?;
            let Ok(msg) = serde_json::from_str::<Json>(&line) else { continue };
            if let Some(event) = msg.get("event").and_then(Json::as_str) {
                if event == "load_failed" {
                    return Err(format!("Serum failed to load: {}", msg["error"]));
                }
                continue;
            }
            if msg.get("id").and_then(Json::as_u64) != Some(id) {
                continue;
            }
            return if msg.get("ok").and_then(Json::as_bool) == Some(true) {
                Ok(msg.get("result").cloned().unwrap_or(Json::Null))
            } else {
                Err(msg.get("error").and_then(Json::as_str).unwrap_or("helper error").to_owned())
            };
        }
    }
}

/// Send a request to the helper, starting it if needed. If a previously working helper has
/// died (e.g. Serum crashed on an earlier patch), it is restarted and the request retried once.
pub async fn request(cmd: &str, args: Json) -> Result<Json, String> {
    let mut guard = HOST.lock().await;
    for attempt in 0..2 {
        if guard.is_none() {
            *guard = Some(spawn().await?);
        }
        let helper = guard.as_mut().expect("just started");
        let was_warm = helper.warm;
        let timeout = if was_warm { TIMEOUT } else { FIRST_TIMEOUT };
        match tokio::time::timeout(timeout, helper.call(cmd, &args)).await {
            Ok(Ok(result)) => {
                helper.warm = true;
                return Ok(result);
            }
            Ok(Err(e)) if e.starts_with("bad input") || e.starts_with("unknown command") => {
                helper.warm = true; // the helper is fine; the request was wrong
                return Err(e);
            }
            Ok(Err(e)) => {
                *guard = None;
                if !(was_warm && attempt == 0) {
                    return Err(e);
                }
            }
            Err(_) => {
                *guard = None; // kill_on_drop stops the hung helper
                return Err(format!("Serum helper timed out after {}s and was restarted", timeout.as_secs()));
            }
        }
    }
    Err("Serum helper failed twice; see the log".into())
}
