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
                    Some(sid) => ParsedLine::Chunk(Chunk {
                        kind: "session",
                        text: None,
                        session_id: Some(sid.to_string()),
                    }),
                    None => ParsedLine::Ignore,
                },
                Some("api_retry") => ParsedLine::Chunk(Chunk {
                    kind: "status",
                    text: Some("retrying after an API error".to_string()),
                    session_id: None,
                }),
                _ => ParsedLine::Ignore,
            },
            // Token-level deltas (--include-partial-messages).
            Some("stream_event") => {
                let delta = &v["event"]["delta"];
                if delta["type"].as_str() == Some("text_delta") {
                    match delta["text"].as_str() {
                        Some(t) if !t.is_empty() => ParsedLine::Chunk(Chunk {
                            kind: "text",
                            text: Some(t.to_string()),
                            session_id: None,
                        }),
                        _ => ParsedLine::Ignore,
                    }
                } else {
                    ParsedLine::Ignore
                }
            }
            // Whole assistant messages: forward tool_use as activity markers.
            // Text content is already covered by the deltas above.
            Some("assistant") => {
                let tools: Vec<&str> = v["message"]["content"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter(|i| i["type"].as_str() == Some("tool_use"))
                            .filter_map(|i| i["name"].as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                if tools.is_empty() {
                    ParsedLine::Ignore
                } else {
                    ParsedLine::Chunk(Chunk {
                        kind: "tool",
                        text: Some(tools.join(", ")),
                        session_id: None,
                    })
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
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                kind: "session",
                text: None,
                session_id: Some("abc-123".into()),
            })
        );
    }

    #[test]
    fn text_delta_yields_text_chunk() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Fixing the test"}}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                kind: "text",
                text: Some("Fixing the test".into()),
                session_id: None,
            })
        );
    }

    #[test]
    fn assistant_tool_use_yields_tool_chunk() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Edit","input":{}},{"type":"tool_use","name":"Read","input":{}}]}}"#;
        assert_eq!(
            parse(line),
            ParsedLine::Chunk(Chunk {
                kind: "tool",
                text: Some("Edit, Read".into()),
                session_id: None,
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
                kind: "session",
                text: None,
                session_id: Some("1853c519-9b0b-4c79-bd97-697f476c516a".into()),
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
