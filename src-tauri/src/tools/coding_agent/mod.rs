//! `run_coding_agent` — drive the user's local coding CLI (ENG-1528).
//!
//! Orchestration only; CLI specifics live in the adapters (`claude.rs`,
//! `codex.rs` in ENG-1529). Rides the ENG-1527 rails: streams
//! `response_chunk`s, honors the cancel signal (SIGINT to the child's process
//! group; the terminal response still goes out), holds the one-coding-run
//! slot, and registers the child for reap-on-quit.

mod binary;
mod claude;
pub mod runner;

use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::process_group::GroupChild;
use crate::state::AppState;
use crate::ws::inflight::CancelSignal;
use crate::ws::ToolStream;
use runner::{LocalCodingRunner, ParsedLine, RunResult, RunSpec};

/// Hard ceiling on a single run. Long enough for a real coding task, short
/// enough that a wedged CLI can't hold the coding slot forever.
const RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// After a cancel SIGINT, how long the CLI gets to exit cleanly (saving its
/// session for resume) before the whole group is SIGTERMed.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct Params {
    prompt: String,
    working_dir: String,
    /// "claude" (default) — "codex" lands with ENG-1529.
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

pub fn handles(tool: &str) -> bool {
    tool == "run_coding_agent"
}

/// Run one coding-agent turn, streaming chunks as they arrive.
/// Every exit path returns a terminal value — including cancellation.
pub async fn handle_streaming(
    app: &AppHandle,
    state: &AppState,
    params: serde_json::Value,
    scoped_folders: &[String],
    stream: &ToolStream,
    mut cancel: CancelSignal,
) -> Result<(serde_json::Value, Option<u64>), String> {
    let params: Params =
        serde_json::from_value(params).map_err(|e| format!("bad_params: {e}"))?;

    // Adapter selection (detect+default is a Settings concern; explicit here).
    let runner: &'static dyn LocalCodingRunner = match params.cli.as_deref() {
        None | Some("claude") => &claude::ClaudeRunner,
        Some("codex") => return Err("bad_params: codex adapter lands with ENG-1529".into()),
        Some(other) => return Err(format!("bad_params: unknown cli '{other}'")),
    };

    // The CLI's cwd must be inside a user-granted folder, same rule as every
    // other desktop tool. canonicalize + prefix-match via the shared validator.
    let working_dir = crate::security::validate_path(&params.working_dir, scoped_folders)
        .map_err(|e| format!("out_of_scope: {e}"))?;
    if !working_dir.is_dir() {
        return Err(format!(
            "bad_params: working_dir is not a directory: {}",
            working_dir.display()
        ));
    }

    // One coding run at a time per device (ENG-1527 slot). Refuse, don't queue.
    let _slot = state
        .inflight
        .try_begin_coding_run()
        .ok_or("coding_run_busy: a coding run is already in progress on this device")?;

    let settings = crate::config::load_settings(app);
    let api_key = settings.anthropic_api_key.clone().filter(|k| !k.is_empty());
    if runner.name() == "claude" && api_key.is_none() {
        return Err(
            "api_key_missing: add your Anthropic API key in Beakr Desktop settings to run \
             Claude Code from Beakr (v1 uses your own key — see ENG-1286)"
                .into(),
        );
    }

    let binary = binary::resolve(runner.name(), settings.claude_binary_path.as_deref())?;

    let spec = RunSpec {
        prompt: params.prompt,
        working_dir,
        session_id: params.session_id,
        api_key,
    };

    let mut cmd = runner.build_command(&binary, &spec);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = GroupChild::spawn(&mut cmd)
        .map_err(|e| format!("spawn_failed: could not start {}: {e}", runner.name()))?;
    state.processes.register(stream.request_id(), &child);
    set_run_ui(app, state, Some(stream.request_id())).await;

    let stdout = child
        .stdout_take()
        .ok_or("spawn_failed: no stdout from the CLI")?;
    let stderr = child.stderr_take();

    // Collect a stderr tail concurrently for failure classification.
    let stderr_task = tokio::spawn(async move {
        let mut tail: Vec<String> = Vec::new();
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        }
        tail.join("\n")
    });

    // Main pump: stdout lines → normalized chunks → response_chunk frames.
    let mut lines = BufReader::new(stdout).lines();
    let mut final_result: Option<RunResult> = None;
    let mut cancelled = false;
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => match runner.parse_line(&line) {
                        ParsedLine::Chunk(chunk) => {
                            let data = serde_json::to_value(&chunk).unwrap_or_default();
                            stream.chunk(data).await;
                        }
                        ParsedLine::Final(result) => {
                            final_result = Some(result);
                            // Keep draining until EOF: the process exit code
                            // and any trailing events still matter.
                        }
                        ParsedLine::Ignore => {}
                    },
                    Ok(None) => break, // EOF: child closed stdout.
                    Err(e) => {
                        log::warn!("stdout read error from {}: {e}", runner.name());
                        break;
                    }
                }
            }
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                log::info!("Cancel received for {} — SIGINT to the group", stream.request_id());
                child.interrupt();
                // Give the CLI CANCEL_GRACE to exit cleanly (session stays
                // resumable), then escalate. The loop keeps draining stdout
                // meanwhile so a final `result` event can still be captured.
                let _ = tokio::time::timeout(CANCEL_GRACE, lines.next_line()).await;
                child.terminate();
            }
            _ = tokio::time::sleep_until(deadline) => {
                child.terminate();
                cleanup(app, state, stream.request_id()).await;
                return Err(format!(
                    "run_timeout: coding run exceeded {} minutes and was stopped; \
                     the session may be resumable",
                    RUN_TIMEOUT.as_secs() / 60
                ));
            }
        }
    }

    let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            log::warn!("wait() failed for {}: {e}", runner.name());
            None
        }
        Err(_) => {
            // stdout hit EOF but the process is wedged — kill the group.
            child.kill_group();
            None
        }
    };
    let stderr_tail = stderr_task.await.unwrap_or_default();
    cleanup(app, state, stream.request_id()).await;

    if cancelled {
        // Terminal response still goes out (LSP cancel semantics); carry the
        // session id so the next turn can resume where the user stopped it.
        let session_id = final_result.and_then(|r| r.session_id);
        return Ok((
            serde_json::json!({
                "cancelled": true,
                "session_id": session_id,
                "cli": runner.name(),
            }),
            None,
        ));
    }

    let exited_ok = status.map(|s| s.success()).unwrap_or(false);
    match final_result {
        Some(result) if exited_ok && !result.is_error => {
            let mut data = serde_json::to_value(&result).unwrap_or_default();
            data["cli"] = serde_json::Value::String(runner.name().to_string());
            data["cancelled"] = serde_json::Value::Bool(false);
            Ok((data, None))
        }
        Some(result) => Err(runner.classify_failure(
            status.and_then(|s| s.code()),
            result.result.as_deref().unwrap_or(&stderr_tail),
        )),
        None => Err(runner.classify_failure(status.and_then(|s| s.code()), &stderr_tail)),
    }
}

/// Reflect run start/stop in the tray + frontend, and keep the active-run id
/// where the tray "Stop run" handler can reach it.
async fn set_run_ui(app: &AppHandle, state: &AppState, request_id: Option<&str>) {
    *state
        .active_coding_run
        .write()
        .expect("active run lock poisoned") = request_id.map(String::from);
    crate::tray::update_tray_coding_run(app, request_id.is_some());
    let _ = app.emit(
        "coding_run:changed",
        serde_json::json!({ "active": request_id.is_some() }),
    );
}

async fn cleanup(app: &AppHandle, state: &AppState, request_id: &str) {
    state.processes.unregister(request_id);
    set_run_ui(app, state, None).await;
}
