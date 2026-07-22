//! Codex CLI adapter (ENG-1529).
//!
//! Drives the user's local `codex` headlessly: `codex exec --json` (JSONL on
//! stdout), resuming via `codex exec resume <thread_id>`. Auth is the user's
//! own `codex login` (~/.codex/auth.json) — Beakr handles no credential, same
//! posture as the Claude adapter (DESIGN.md decision 5).
//!
//! Guardrails (DESIGN.md decisions 1/7, DESIGN-REVIEW.md §7): the sandbox IS
//! the control. `--sandbox workspace-write` confines writes to the working
//! directory with network off; exec mode never prompts, so a sandbox-forbidden
//! action FAILS (surfaced via an `error` event / nonzero exit) instead of
//! hanging on an approval that can never come. Never `danger-full-access`;
//! never `--ephemeral` (it breaks resume).
//!
//! Schema discipline: Codex documents its JSONL as additive and
//! version-dependent — match known `type`/`item_type` values, Ignore all
//! unknowns, never error on them.
//!
//! Cost: Codex reports token usage on `turn.completed`, NOT dollars. No cost
//! chunk is emitted — converting tokens to dollars would assume API pricing,
//! which is wrong for the ChatGPT-plan users this auth model targets. The
//! run card simply shows no cost for Codex runs (David, 2026-07-17).

use std::path::Path;

use tokio::process::Command;

use super::runner::{Chunk, LocalCodingRunner, ParsedLine, RunResult, RunSpec};

pub struct CodexRunner;

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The item's discriminator. Captured 0.144.5 JSONL uses `"type"`
/// (`{"item":{"type":"agent_message",...}}`) although docs have shown
/// `item_type` — accept both, schema-defensively.
fn item_type(item: &serde_json::Value) -> Option<&str> {
    item["type"].as_str().or_else(|| item["item_type"].as_str())
}

/// Codex wraps every command in a login shell (captured fixtures:
/// `/bin/zsh -lc 'cat hello.txt'`). Unwrap that for the label — the user
/// should read "Run cat hello.txt", not the wrapper. The FULL original
/// command still rides the "command" chunk for the audit trail.
fn unwrap_shell(command: &str) -> &str {
    for shell in ["/bin/zsh", "/bin/bash", "/bin/sh", "zsh", "bash", "sh"] {
        for flag in ["-lc", "-c"] {
            let prefix = format!("{shell} {flag} ");
            if let Some(inner) = command.strip_prefix(&prefix) {
                let inner = inner.trim();
                // Strip one matching layer of quotes if present.
                for quote in ['\'', '"'] {
                    if inner.len() >= 2 && inner.starts_with(quote) && inner.ends_with(quote) {
                        return &inner[1..inner.len() - 1];
                    }
                }
                return inner;
            }
        }
    }
    command
}

/// First ~5 words of a shell command (unwrapped from its login-shell
/// wrapper), for the activity label.
fn command_label(command: &str) -> String {
    let display = unwrap_shell(command);
    let short: Vec<&str> = display.split_whitespace().take(5).collect();
    let mut label = format!("Run {}", short.join(" "));
    if display.split_whitespace().count() > 5 {
        label.push('…');
    }
    label
}

/// Map a `file_change` item's per-path `kind` onto the chunk vocabulary.
/// Codex CAN delete (its sandbox allows `rm`/patch-deletes inside the
/// workspace), unlike the Bash-denied Claude surface.
fn change_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "add" => Some("write"),
        "update" => Some("modify"),
        "delete" => Some("delete"),
        _ => None,
    }
}

impl LocalCodingRunner for CodexRunner {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn build_command(&self, binary: &Path, spec: &RunSpec) -> Command {
        // Flag placement is load-bearing and was verified against the real
        // binary (0.144.5, 2026-07-17): the `resume` subcommand REJECTS
        // --sandbox/--cd/--skip-git-repo-check, but accepts them at the
        // parent `exec` level BEFORE the subcommand — where they then apply
        // to the resumed turn. Canonical shape (works for fresh and resume):
        //   codex exec --json --sandbox workspace-write --skip-git-repo-check
        //     --cd <dir> [resume <thread_id>] -- <prompt>
        let mut cmd = Command::new(binary);
        cmd.current_dir(&spec.working_dir);
        cmd.arg("exec")
            .arg("--json")
            .args(["--sandbox", "workspace-write"])
            .arg("--skip-git-repo-check")
            .arg("--cd")
            .arg(&spec.working_dir);
        if let Some(session) = &spec.session_id {
            cmd.args(["resume", session]);
        }
        // Prompt last: positional, after every flag.
        cmd.arg("--").arg(&spec.prompt);
        // spec.api_key is the user's ANTHROPIC key for the Claude adapter —
        // never injected here. Codex uses its own `codex login` credential.
        cmd
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        let line = line.trim();
        if line.is_empty() {
            return ParsedLine::Ignore;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // Non-JSON noise on stdout — skip.
            Err(_) => return ParsedLine::Ignore,
        };

        match v["type"].as_str() {
            Some("thread.started") => match v["thread_id"].as_str() {
                // No model here: Codex does not expose one in its stream —
                // the card shows the CLI alone (ENG-1581).
                Some(tid) => ParsedLine::Chunk(Chunk {
                    session_id: Some(tid.to_string()),
                    cli: Some("codex"),
                    ..Chunk::bare("session")
                }),
                None => ParsedLine::Ignore,
            },

            // A command begins: show it on the activity line immediately.
            Some("item.started") => {
                let item = &v["item"];
                match item_type(item) {
                    Some("command_execution") => match item["command"].as_str() {
                        Some(cmd) => ParsedLine::Chunk(Chunk {
                            text: Some(command_label(cmd)),
                            ..Chunk::bare("tool")
                        }),
                        None => ParsedLine::Ignore,
                    },
                    _ => ParsedLine::Ignore,
                }
            }

            Some("item.completed") => {
                let item = &v["item"];
                match item_type(item) {
                    // The agent's message. Codex has no single terminal
                    // "result" event; the LAST agent_message is the answer,
                    // so each one is emitted as Final and the orchestrator's
                    // keep-draining loop retains the newest. thread_id isn't
                    // on this event — resume rides the session chunk from
                    // thread.started instead.
                    Some("agent_message") => match item["text"].as_str() {
                        Some(text) => ParsedLine::Final(RunResult {
                            session_id: None,
                            result: Some(text.to_string()),
                            total_cost_usd: None,
                            is_error: false,
                        }),
                        None => ParsedLine::Ignore,
                    },

                    // An executed command, now with its outcome — record it
                    // for the audit trail (the engine accumulates `command`).
                    Some("command_execution") => match item["command"].as_str() {
                        Some(cmd) => ParsedLine::Chunk(Chunk {
                            command: Some(cmd.to_string()),
                            ..Chunk::bare("command")
                        }),
                        None => ParsedLine::Ignore,
                    },

                    // Applied workspace edits: one file_changed per path —
                    // the run card's files-changed list and the audit row.
                    Some("file_change") => {
                        let mut chunks: Vec<Chunk> = Vec::new();
                        if let Some(changes) = item["changes"].as_array() {
                            for change in changes {
                                let (Some(path), Some(kind)) =
                                    (change["path"].as_str(), change["kind"].as_str())
                                else {
                                    continue;
                                };
                                let Some(mapped) = change_kind(kind) else {
                                    continue;
                                };
                                chunks.push(Chunk {
                                    text: Some(format!("Edit {}", basename(path))),
                                    path: Some(path.to_string()),
                                    change: Some(mapped),
                                    ..Chunk::bare("file_changed")
                                });
                            }
                        }
                        match chunks.len() {
                            0 => ParsedLine::Ignore,
                            1 => ParsedLine::Chunk(chunks.pop().expect("len checked")),
                            _ => ParsedLine::Chunks(chunks),
                        }
                    }

                    Some("error") => match item["message"].as_str() {
                        Some(msg) => ParsedLine::Chunk(Chunk {
                            text: Some(msg.to_string()),
                            ..Chunk::bare("status")
                        }),
                        None => ParsedLine::Ignore,
                    },

                    // reasoning, todo_list, web_search, mcp_tool_call, and
                    // anything newer: not run-card material. Additive schema —
                    // skip, never error.
                    _ => ParsedLine::Ignore,
                }
            }

            // A failed turn ends the run: surface the message as the terminal
            // error so classify_failure gets real text, not a bare exit code.
            Some("turn.failed") | Some("error") => {
                let msg = v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("Codex reported an error");
                ParsedLine::Final(RunResult {
                    session_id: None,
                    result: Some(msg.to_string()),
                    total_cost_usd: None,
                    is_error: true,
                })
            }

            // turn.started / turn.completed (token usage only — no dollars,
            // see module docs) and unknown event types: skip.
            _ => ParsedLine::Ignore,
        }
    }

    fn classify_failure(&self, exit_code: Option<i32>, stderr_tail: &str) -> String {
        let lower = stderr_tail.to_lowercase();
        if lower.contains("not logged in")
            || lower.contains("login")
            || lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("authentication")
        {
            return format!(
                "auth_failed: Codex isn't logged in on this Mac. Run `codex login` in a \
                 terminal and try again. ({stderr_tail})"
            );
        }
        if lower.contains("429")
            || lower.contains("rate limit")
            || lower.contains("usage limit")
            || lower.contains("quota")
        {
            return format!(
                "quota_exceeded: Codex hit your ChatGPT plan's usage limit. ({stderr_tail})"
            );
        }
        format!(
            "run_failed: codex exited with {} — {stderr_tail}",
            exit_code.map_or("signal".to_string(), |c| c.to_string())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> ParsedLine {
        CodexRunner.parse_line(line)
    }

    #[test]
    fn thread_started_yields_session_chunk() {
        let line = r#"{"type":"thread.started","thread_id":"0199-abc"}"#;
        // ENG-1581: names the CLI; no model — Codex doesn't expose one.
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                session_id: Some("0199-abc".into()),
                cli: Some("codex"),
                ..Chunk::bare("session")
            })
        );
    }

    #[test]
    fn agent_message_yields_final_with_answer() {
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Done - I added the endpoint."}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Final(RunResult {
                result: Some("Done - I added the endpoint.".into()),
                ..RunResult::default()
            })
        );
    }

    #[test]
    fn command_start_labels_activity_and_completion_records_command() {
        // Captured verbatim from codex-cli 0.144.5 (2026-07-17): commands
        // arrive wrapped in a login shell. The label unwraps it; the command
        // chunk keeps the full original for the audit trail.
        let started = r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'cat hello.txt'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
        assert_eq!(
            parse(started),
            ParsedLine::Chunk(Chunk {
                text: Some("Run cat hello.txt".into()),
                ..Chunk::bare("tool")
            })
        );

        let completed = r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'cat hello.txt'","aggregated_output":"hi\n","exit_code":0,"status":"completed"}}"#;
        assert_eq!(
            parse(completed),
            ParsedLine::Chunk(Chunk {
                command: Some("/bin/zsh -lc 'cat hello.txt'".into()),
                ..Chunk::bare("command")
            })
        );
    }

    #[test]
    fn unwrap_shell_handles_wrappers_quotes_and_bare_commands() {
        assert_eq!(unwrap_shell("/bin/zsh -lc 'cat hello.txt'"), "cat hello.txt");
        assert_eq!(unwrap_shell(r#"/bin/bash -c "npm test""#), "npm test");
        assert_eq!(unwrap_shell("bash -lc ls"), "ls");
        // Not a recognized wrapper — untouched.
        assert_eq!(unwrap_shell("cargo build --release"), "cargo build --release");
    }

    #[test]
    fn long_commands_truncate_at_five_words_after_unwrapping() {
        assert_eq!(
            command_label("/bin/zsh -lc 'npm test -- --coverage --watchAll=false --json'"),
            "Run npm test -- --coverage --watchAll=false…"
        );
    }

    #[test]
    fn file_change_double_emit_only_counts_completed() {
        // Captured: file_change items arrive TWICE — in_progress on
        // item.started, then completed. Only the completion may emit, or
        // every changed file would be double-counted in the audit row.
        let started = r#"{"type":"item.started","item":{"id":"item_1","type":"file_change","changes":[{"path":"/repo/hello.txt","kind":"add"}],"status":"in_progress"}}"#;
        assert_eq!(parse(started), ParsedLine::Ignore);
    }

    #[test]
    fn intermediate_agent_message_is_overwritten_by_the_last_one() {
        // Captured: Codex narrates ("I'll create the two files...") via an
        // agent_message BEFORE doing the work, then summarizes at the end.
        // Both parse as Final; the orchestrator keeps the newest — pin that
        // both are Final so that contract holds.
        let plan = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I'll create the two files, display hello.txt, then remove DELETEME.txt as requested."}}"#;
        let summary = r#"{"type":"item.completed","item":{"id":"item_6","type":"agent_message","text":"Created hello.txt containing exactly hi."}}"#;
        assert!(matches!(parse(plan), ParsedLine::Final(_)));
        let ParsedLine::Final(result) = parse(summary) else {
            panic!("expected Final");
        };
        assert_eq!(result.result.as_deref(), Some("Created hello.txt containing exactly hi."));
    }

    #[test]
    fn file_change_yields_one_chunk_per_path_with_mapped_kinds() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","status":"completed","changes":[{"path":"/repo/src/app.py","kind":"update"},{"path":"/repo/NEW.md","kind":"add"},{"path":"/repo/old.txt","kind":"delete"}]}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunks(vec![
                Chunk {
                    text: Some("Edit app.py".into()),
                    path: Some("/repo/src/app.py".into()),
                    change: Some("modify"),
                    ..Chunk::bare("file_changed")
                },
                Chunk {
                    text: Some("Edit NEW.md".into()),
                    path: Some("/repo/NEW.md".into()),
                    change: Some("write"),
                    ..Chunk::bare("file_changed")
                },
                Chunk {
                    text: Some("Edit old.txt".into()),
                    path: Some("/repo/old.txt".into()),
                    change: Some("delete"),
                    ..Chunk::bare("file_changed")
                },
            ])
        );
    }

    #[test]
    fn turn_failed_yields_error_final_with_message() {
        let line = r#"{"type":"turn.failed","error":{"message":"stream error: sandbox denied write outside workspace"}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Final(RunResult {
                result: Some("stream error: sandbox denied write outside workspace".into()),
                is_error: true,
                ..RunResult::default()
            })
        );
    }

    #[test]
    fn unknown_event_and_item_types_are_ignored_not_errors() {
        // Codex documents its schema as additive/version-dependent.
        for line in [
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#,
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#,
            r#"{"type":"item.completed","item":{"type":"web_search","query":"docs"}}"#,
            r#"{"type":"some.future.event","payload":{}}"#,
            "not json at all",
        ] {
            assert_eq!(parse(line), ParsedLine::Ignore, "line: {line}");
        }
    }

    #[test]
    fn auth_and_quota_failures_classify_with_stable_prefixes() {
        let c = CodexRunner;
        assert!(c
            .classify_failure(Some(1), "Error: not logged in, run codex login")
            .starts_with("auth_failed:"));
        assert!(c
            .classify_failure(Some(1), "You've hit your usage limit until 3pm")
            .starts_with("quota_exceeded:"));
        assert!(c
            .classify_failure(Some(2), "some sandbox denial")
            .starts_with("run_failed:"));
    }

    #[test]
    fn build_command_resume_uses_subcommand_and_never_leaks_anthropic_key() {
        let spec = RunSpec {
            prompt: "continue".into(),
            working_dir: "/tmp".into(),
            session_id: Some("0199-abc".into()),
            api_key: Some("sk-ant-should-not-appear".into()),
        };
        let cmd = CodexRunner.build_command(Path::new("/usr/local/bin/codex"), &spec);
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "exec");
        // Verified against the real binary: flags must PRECEDE the resume
        // subcommand (resume itself rejects --sandbox/--cd), prompt comes
        // last after `--`.
        let resume_pos = args.iter().position(|a| a == "resume").expect("resume present");
        assert_eq!(args[resume_pos + 1], "0199-abc");
        for flag in ["--json", "workspace-write", "--skip-git-repo-check", "--cd"] {
            let pos = args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag} missing"));
            assert!(pos < resume_pos, "{flag} must precede the resume subcommand");
        }
        assert_eq!(args.last().unwrap(), "continue");
        // The user's Anthropic key is Claude-only; Codex uses its own login.
        assert!(!std_cmd
            .get_envs()
            .any(|(k, _)| k.to_string_lossy() == "ANTHROPIC_API_KEY"));
    }
}
