//! Claude Code adapter (ENG-1528).
//!
//! Drives the user's local `claude` CLI headlessly:
//! `claude -p <prompt> --output-format stream-json --verbose
//!  --include-partial-messages` — `--verbose` is REQUIRED with stream-json or
//! nothing streams until the end; partial messages give token-level deltas.
//!
//! Guardrails (DESIGN.md decision 7, DESIGN-REVIEW.md §3-4):
//! - Tool surface: Read/Glob/Grep/Edit/Write under `acceptEdits`. Bash is
//!   DENIED pending Max's ruling; WebFetch/WebSearch denied (Claude Code has
//!   no default network sandbox — an allowed fetch is an exfil channel).
//! - Protected paths: deny rules stop the CLI from editing its own security
//!   config inside the workspace (`.claude/`, `.git/`, `.vscode/`) — the
//!   self-escalation mechanism of the Copilot RCE (CVE-2025-53773). Deny
//!   rules outrank every permission mode, so this holds even if the mode
//!   changes later.

use std::path::Path;

use tokio::process::Command;

use super::runner::{Chunk, LocalCodingRunner, ParsedLine, RunResult, RunSpec};

pub struct ClaudeRunner;

/// Session-scoped settings passed via `--settings`: deny rules protecting the
/// agent's own config surface + secret files, regardless of permission mode.
/// Relative patterns resolve against the run's cwd.
const PROTECTED_PATH_SETTINGS: &str = r#"{"permissions":{"deny":[
"Edit(./.claude/**)","Write(./.claude/**)",
"Edit(./.git/**)","Write(./.git/**)",
"Edit(./.vscode/**)","Write(./.vscode/**)",
"Edit(./.codex/**)","Write(./.codex/**)",
"Read(./.env)","Read(./.env.*)","Read(./**/.env)","Read(./**/.env.*)"
]}}"#;

const ALLOWED_TOOLS: &str = "Read,Glob,Grep,Edit,Write";
/// Bash denied pending Max's write-scope ruling (DESIGN.md decision 7);
/// WebFetch/WebSearch denied because CC has no default network sandbox.
const DISALLOWED_TOOLS: &str = "Bash,WebFetch,WebSearch";

/// Normalize one tool_use into chunks: a "tool" activity marker carrying the
/// target path when the tool has one, plus a "file_changed" for write-capable
/// tools (ENG-1552). Derived from tool INPUTS, per DESIGN.md's observability
/// spec — under `acceptEdits` an emitted Edit/Write is applied, not proposed,
/// so the input path is the changed file.
fn tool_use_chunks(name: &str, input: &serde_json::Value) -> Vec<Chunk> {
    let file_path = input["file_path"].as_str().or_else(|| input["path"].as_str());
    let pattern = input["pattern"].as_str();

    // Human-readable activity line: "Edit seed_sweep.py", "Grep TODO", "Read".
    // Basename only — the full path is in `path` for anything that needs it.
    let label = if let Some(p) = file_path {
        format!("{name} {}", basename(p))
    } else if let Some(pat) = pattern {
        format!("{name} {pat}")
    } else {
        name.to_string()
    };

    let mut chunks = vec![Chunk {
        text: Some(label),
        path: file_path.map(String::from),
        ..Chunk::bare("tool")
    }];

    let change = match name {
        "Write" => Some("write"),
        "Edit" | "MultiEdit" | "NotebookEdit" => Some("modify"),
        _ => None,
    };
    if let (Some(change), Some(p)) = (change, file_path) {
        chunks.push(Chunk {
            path: Some(p.to_string()),
            change: Some(change),
            ..Chunk::bare("file_changed")
        });
    }
    chunks
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl LocalCodingRunner for ClaudeRunner {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn build_command(&self, binary: &Path, spec: &RunSpec) -> Command {
        let mut cmd = Command::new(binary);
        cmd.current_dir(&spec.working_dir)
            .arg("-p")
            .arg(&spec.prompt)
            .args(["--output-format", "stream-json"])
            .arg("--verbose")
            .arg("--include-partial-messages")
            .args(["--permission-mode", "acceptEdits"])
            .args(["--allowedTools", ALLOWED_TOOLS])
            .args(["--disallowedTools", DISALLOWED_TOOLS])
            .args(["--settings", &PROTECTED_PATH_SETTINGS.replace('\n', "")]);

        if let Some(session) = &spec.session_id {
            cmd.args(["--resume", session]);
        }
        if let Some(key) = &spec.api_key {
            // v1 auth decision (DESIGN.md decision 5): the user's own API key,
            // read from local settings, never synced. Non-interactive `claude
            // -p` prefers the API key when present, which is exactly what we
            // want — no dependency on the user's subscription login.
            cmd.env("ANTHROPIC_API_KEY", key);
        }
        cmd
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        let line = line.trim();
        if line.is_empty() {
            return ParsedLine::Ignore;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // Non-JSON noise on stdout (plugin banners etc.) — skip.
            Err(_) => return ParsedLine::Ignore,
        };

        match v["type"].as_str() {
            Some("system") => match v["subtype"].as_str() {
                Some("init") => match v["session_id"].as_str() {
                    // init also announces the model (ENG-1581): carry it so
                    // the run card can name its runner.
                    Some(sid) => ParsedLine::Chunk(Chunk {
                        session_id: Some(sid.to_string()),
                        cli: Some("claude"),
                        model: v["model"].as_str().map(String::from),
                        ..Chunk::bare("session")
                    }),
                    None => ParsedLine::Ignore,
                },
                Some("api_retry") => ParsedLine::Chunk(Chunk {
                    text: Some("retrying after an API error".to_string()),
                    ..Chunk::bare("status")
                }),
                _ => ParsedLine::Ignore,
            },
            // Token-level deltas (--include-partial-messages).
            Some("stream_event") => {
                let delta = &v["event"]["delta"];
                if delta["type"].as_str() == Some("text_delta") {
                    match delta["text"].as_str() {
                        Some(t) if !t.is_empty() => ParsedLine::Chunk(Chunk {
                            text: Some(t.to_string()),
                            ..Chunk::bare("text")
                        }),
                        _ => ParsedLine::Ignore,
                    }
                } else {
                    ParsedLine::Ignore
                }
            }
            // Whole assistant messages: forward each tool_use as a structured
            // activity marker (ENG-1552). Collapsing to bare tool names threw
            // away the target paths — exactly the structure the live-run card's
            // activity line and files-changed list need. Text content is
            // already covered by the deltas above.
            Some("assistant") => {
                let mut chunks: Vec<Chunk> = Vec::new();
                if let Some(items) = v["message"]["content"].as_array() {
                    for item in items.iter().filter(|i| i["type"].as_str() == Some("tool_use")) {
                        let Some(name) = item["name"].as_str() else {
                            continue;
                        };
                        chunks.extend(tool_use_chunks(name, &item["input"]));
                    }
                }
                match chunks.len() {
                    0 => ParsedLine::Ignore,
                    1 => ParsedLine::Chunk(chunks.pop().expect("len checked")),
                    _ => ParsedLine::Chunks(chunks),
                }
            }
            Some("result") => ParsedLine::Final(RunResult {
                session_id: v["session_id"].as_str().map(String::from),
                result: v["result"].as_str().map(String::from),
                total_cost_usd: v["total_cost_usd"].as_f64(),
                is_error: v["is_error"].as_bool().unwrap_or(false),
            }),
            // Additive schema: unknown types are skipped, never errors.
            _ => ParsedLine::Ignore,
        }
    }

    fn classify_failure(&self, exit_code: Option<i32>, stderr_tail: &str) -> String {
        let lower = stderr_tail.to_lowercase();
        if lower.contains("not logged in")
            || lower.contains("login expired")
            || lower.contains("please run /login")
            || lower.contains("401")
            || lower.contains("invalid api key")
            || lower.contains("authentication")
        {
            // Subscription-first message (DESIGN.md decision 5): the common
            // fix is logging into Claude Code, not adding an API key.
            return format!(
                "auth_failed: Claude Code isn't logged in on this Mac. Open Claude Code and \
                 log in (or add an API key in Beakr Desktop settings). ({stderr_tail})"
            );
        }
        if lower.contains("429") || lower.contains("rate limit") || lower.contains("overloaded") {
            return format!("quota_exceeded: Claude Code hit a rate/usage limit. ({stderr_tail})");
        }
        format!(
            "run_failed: claude exited with {} — {stderr_tail}",
            exit_code.map_or("signal".to_string(), |c| c.to_string())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> ParsedLine {
        ClaudeRunner.parse_line(line)
    }

    #[test]
    fn init_event_yields_session_chunk() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-fable-5","tools":["Read","Edit"]}"#;
        // ENG-1581: the session chunk names its runner and model.
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                session_id: Some("abc-123".into()),
                cli: Some("claude"),
                model: Some("claude-fable-5".into()),
                ..Chunk::bare("session")
            })
        );
    }

    #[test]
    fn text_delta_yields_text_chunk() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Fixing the test"}}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                text: Some("Fixing the test".into()),
                ..Chunk::bare("text")
            })
        );
    }

    #[test]
    fn assistant_tool_uses_yield_one_chunk_each_with_names() {
        // ENG-1552: no more "Edit, Read" collapsing — one tool chunk per call.
        // Inputs without paths degrade to name-only labels.
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Edit","input":{}},{"type":"tool_use","name":"Read","input":{}}]}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunks(vec![
                Chunk { text: Some("Edit".into()), ..Chunk::bare("tool") },
                Chunk { text: Some("Read".into()), ..Chunk::bare("tool") },
            ])
        );
    }

    #[test]
    fn edit_with_file_path_yields_tool_and_file_changed() {
        // ENG-1552: the trust surface — an applied edit reports its target.
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/app.py","old_string":"a","new_string":"b"}}]}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunks(vec![
                Chunk {
                    text: Some("Edit app.py".into()),
                    path: Some("/repo/src/app.py".into()),
                    ..Chunk::bare("tool")
                },
                Chunk {
                    path: Some("/repo/src/app.py".into()),
                    change: Some("modify"),
                    ..Chunk::bare("file_changed")
                },
            ])
        );
    }

    #[test]
    fn write_yields_file_changed_write_and_read_does_not() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/repo/new.md","content":"x"}},{"type":"tool_use","name":"Read","input":{"file_path":"/repo/old.md"}}]}}"#;
        let ParsedLine::Chunks(chunks) = parse(line) else {
            panic!("expected Chunks");
        };
        let file_changed: Vec<_> =
            chunks.iter().filter(|c| c.kind == "file_changed").collect();
        assert_eq!(file_changed.len(), 1, "Read must not emit file_changed");
        assert_eq!(file_changed[0].path.as_deref(), Some("/repo/new.md"));
        assert_eq!(file_changed[0].change, Some("write"));
        // Read still surfaces as an activity marker with its target.
        assert!(chunks
            .iter()
            .any(|c| c.kind == "tool" && c.text.as_deref() == Some("Read old.md")));
    }

    #[test]
    fn grep_pattern_lands_in_label_not_path() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"TODO"}}]}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                text: Some("Grep TODO".into()),
                ..Chunk::bare("tool")
            })
        );
    }

    #[test]
    fn result_event_yields_final_with_cost() {
        let line = r#"{"type":"result","subtype":"success","result":"Done — 3 tests fixed.","session_id":"abc-123","total_cost_usd":0.1234,"is_error":false,"usage":{}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Final(RunResult {
                session_id: Some("abc-123".into()),
                result: Some("Done — 3 tests fixed.".into()),
                total_cost_usd: Some(0.1234),
                is_error: false,
            })
        );
    }

    #[test]
    fn api_retry_yields_status_and_unknowns_are_ignored() {
        assert!(matches!(
            parse(r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":3}"#),
            ParsedLine::Chunk(Chunk { kind: "status", .. })
        ));
        // Additive-schema safety: unknown event types and non-JSON noise skip.
        assert_eq!(parse(r#"{"type":"totally_new_event_kind"}"#), ParsedLine::Ignore);
        assert_eq!(parse("plugin banner: loaded 3 plugins"), ParsedLine::Ignore);
        assert_eq!(parse(""), ParsedLine::Ignore);
    }

    #[test]
    fn command_carries_guardrails_resume_and_key() {
        let spec = RunSpec {
            prompt: "fix the tests".into(),
            working_dir: std::env::temp_dir(),
            session_id: Some("sess-9".into()),
            api_key: Some("sk-test".into()),
        };
        let cmd = ClaudeRunner.build_command(Path::new("/usr/local/bin/claude"), &spec);
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");

        // Streaming shape
        assert!(joined.contains("--output-format stream-json"));
        assert!(joined.contains("--verbose"), "--verbose is REQUIRED with stream-json");
        assert!(joined.contains("--include-partial-messages"));
        // Guardrails
        assert!(joined.contains("--permission-mode acceptEdits"));
        assert!(joined.contains("--allowedTools Read,Glob,Grep,Edit,Write"));
        assert!(
            joined.contains("--disallowedTools Bash,WebFetch,WebSearch"),
            "Bash must stay denied until Max's ruling"
        );
        let settings_arg = args
            .iter()
            .position(|a| a == "--settings")
            .map(|i| args[i + 1].clone())
            .expect("--settings deny rules present");
        let parsed: serde_json::Value =
            serde_json::from_str(&settings_arg).expect("settings arg is valid JSON");
        let denies = parsed["permissions"]["deny"]
            .as_array()
            .expect("deny list present");
        for protected in ["Write(./.claude/**)", "Write(./.git/**)", "Read(./.env)"] {
            assert!(
                denies.iter().any(|d| d == protected),
                "missing protected-path rule {protected}"
            );
        }
        // Resume + key
        assert!(joined.contains("--resume sess-9"));
        let envs: Vec<(String, Option<String>)> = std_cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.as_deref() == Some("sk-test")));
    }

    #[test]
    fn failure_classification_is_typed() {
        let r = ClaudeRunner;
        assert!(r
            .classify_failure(Some(1), "Login expired · Please run /login")
            .starts_with("auth_failed:"));
        assert!(r
            .classify_failure(Some(1), "429 rate limit exceeded")
            .starts_with("quota_exceeded:"));
        assert!(r
            .classify_failure(Some(2), "something else broke")
            .starts_with("run_failed:"));
    }

    // Real output captured from `claude -p ... --output-format stream-json`
    // on a machine with claude installed but NOT logged in (no subscription,
    // no API key). Locks the not-logged-in path against a real fixture.
    #[test]
    fn real_not_logged_in_output_parses_and_classifies_as_auth() {
        let init = r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"1853c519-9b0b-4c79-bd97-697f476c516a","tools":["Bash","Edit"],"apiKeySource":"none","model":"claude-opus-4-8"}"#;
        assert_eq!(
            parse(init),
            ParsedLine::Chunk(Chunk {
                session_id: Some("1853c519-9b0b-4c79-bd97-697f476c516a".into()),
                cli: Some("claude"),
                model: Some("claude-opus-4-8".into()),
                ..Chunk::bare("session")
            })
        );

        let result = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login","session_id":"1853c519-9b0b-4c79-bd97-697f476c516a","total_cost_usd":0,"usage":{}}"#;
        let final_result = match parse(result) {
            ParsedLine::Final(r) => r,
            other => panic!("expected Final, got {other:?}"),
        };
        assert!(final_result.is_error);
        assert_eq!(
            final_result.result.as_deref(),
            Some("Not logged in · Please run /login")
        );
        // The orchestrator routes an is_error result's text through
        // classify_failure; it must land as auth_failed, not run_failed.
        assert!(ClaudeRunner
            .classify_failure(Some(0), final_result.result.as_deref().unwrap())
            .starts_with("auth_failed:"));
    }
}
