//! `run_coding_agent` — drive the user's local coding CLI (ENG-1528).
//!
//! Orchestration only; CLI specifics live in the adapters (`claude.rs`,
//! `codex.rs` in ENG-1529). Rides the ENG-1527 rails: streams
//! `response_chunk`s, honors the cancel signal (SIGINT to the child's process
//! group; the terminal response still goes out), holds the one-coding-run
//! slot, and registers the child for reap-on-quit.

mod binary;
mod claude;
mod codex;
pub mod readiness;
pub mod runner;

use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::process_group::GroupChild;
use crate::state::{ActiveCodingRun, AppState, CodingRunStatus};
use crate::ws::inflight::CancelSignal;
use crate::ws::ToolStream;
use runner::{Chunk, LocalCodingRunner, ParsedLine, RunResult, RunSpec};

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

    let settings = crate::config::load_settings(app);
    // Adapter selection: an explicit engine request wins; otherwise the
    // user's default-CLI setting (ENG-1536 picker); otherwise claude (the
    // pre-picker behavior).
    let requested_cli = params.cli.as_deref().or(settings.default_cli.as_deref());
    let runner: &'static dyn LocalCodingRunner = match requested_cli {
        Some("claude") => &claude::ClaudeRunner,
        Some("codex") => &codex::CodexRunner,
        Some(other) => return Err(format!("bad_params: unknown cli '{other}'")),
        None => {
            // Auto-default (David, 2026-07-17): run the CLI the user actually
            // HAS — a codex-only machine must not exec a claude binary that
            // isn't there. Claude wins the tie when both are installed, and
            // is the fallback when neither is (so the error carries the
            // primary CLI's install guidance).
            if binary::resolve("claude", settings.claude_binary_path.as_deref()).is_ok() {
                &claude::ClaudeRunner
            } else if binary::resolve("codex", None).is_ok() {
                &codex::CodexRunner
            } else {
                &claude::ClaudeRunner
            }
        }
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

    // Auth is agnostic (DESIGN.md decision 5): most Beakr users have a Claude
    // subscription, not an API key. We inject ANTHROPIC_API_KEY only if the
    // user explicitly set one; otherwise the CLI uses whatever login already
    // exists in ~/.claude / keychain from their own `claude` login. If neither
    // is present, the run fails fast with "Not logged in" and we classify it
    // as auth_failed with a "log into Claude Code" hint — no server-side
    // pre-flight guess needed. Beakr never handles the subscription credential.
    let api_key = settings.anthropic_api_key.clone().filter(|k| !k.is_empty());

    // The settings override is per-CLI: claude_binary_path must never route a
    // codex run to the claude binary. A codex_binary_path setting can land
    // with the ENG-1536 readiness/picker work; until then codex resolves via
    // the standard PATH probe in binary::resolve.
    let binary_override = match runner.name() {
        "claude" => settings.claude_binary_path.as_deref(),
        _ => None,
    };
    let binary = binary::resolve(runner.name(), binary_override)?;

    let working_dir_display = working_dir.to_string_lossy().into_owned();
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
    set_run_ui(
        app,
        state,
        Some(ActiveCodingRun {
            request_id: stream.request_id().to_string(),
            working_dir: working_dir_display,
            cli: runner.name().to_string(),
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            status: CodingRunStatus::Running,
        }),
    )
    .await;

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
                        ParsedLine::Chunks(chunks) => {
                            for chunk in &chunks {
                                let data = serde_json::to_value(chunk).unwrap_or_default();
                                stream.chunk(data).await;
                            }
                        }
                        ParsedLine::Final(result) => {
                            // Surface the run's cost through the normal chunk
                            // stream too (ENG-1552): the engine's accumulator
                            // folds it into every subsequent card emit. The CLI
                            // only reports cost here, on its terminal event —
                            // there are no mid-run ticks to forward.
                            if let Some(cost) = result.total_cost_usd {
                                let data = serde_json::to_value(Chunk {
                                    total_cost_usd: Some(cost),
                                    ..Chunk::bare("cost")
                                })
                                .unwrap_or_default();
                                stream.chunk(data).await;
                            }
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
                // Truth-telling (ENG-1552): the child has been signalled but
                // is NOT confirmed dead. The tray/window show "Stopping…"
                // until cleanup() clears the run after the process is reaped.
                mark_run_stopping(app, state).await;
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
    let outcome = match final_result {
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
    };

    // Feed the zero-cost readiness signal (ENG-1536): a successful run is
    // proof of login; an auth_failed one is proof of its absence. Other
    // failures say nothing about auth and leave the cache alone.
    match &outcome {
        Ok(_) => crate::config::record_cli_auth(app, runner.name(), true),
        Err(e) if e.starts_with("auth_failed:") => {
            crate::config::record_cli_auth(app, runner.name(), false)
        }
        Err(_) => {}
    }
    outcome
}

/// Reflect run start/stop in the tray + frontend, and keep the active run
/// where the tray "Stop run" handler and the app window can reach it.
async fn set_run_ui(app: &AppHandle, state: &AppState, run: Option<ActiveCodingRun>) {
    *state
        .active_coding_run
        .write()
        .expect("active run lock poisoned") = run.clone();
    crate::tray::update_tray_coding_run(app, run.as_ref());
    // `active` is kept for compatibility with the original event shape; `run`
    // is the full payload the ActiveRunCard renders (null when no run).
    let _ = app.emit(
        "coding_run:changed",
        serde_json::json!({ "active": run.is_some(), "run": run }),
    );
}

/// Move the active run to Stopping without touching its identity fields.
/// No-op if the run has already been cleared (cancel racing a natural exit).
async fn mark_run_stopping(app: &AppHandle, state: &AppState) {
    let stopping = {
        let mut guard = state
            .active_coding_run
            .write()
            .expect("active run lock poisoned");
        match guard.as_mut() {
            Some(run) => {
                run.status = CodingRunStatus::Stopping;
                Some(run.clone())
            }
            None => None,
        }
    };
    if let Some(run) = stopping {
        crate::tray::update_tray_coding_run(app, Some(&run));
        let _ = app.emit(
            "coding_run:changed",
            serde_json::json!({ "active": true, "run": run }),
        );
    }
}

async fn cleanup(app: &AppHandle, state: &AppState, request_id: &str) {
    state.processes.unregister(request_id);
    set_run_ui(app, state, None).await;
}
