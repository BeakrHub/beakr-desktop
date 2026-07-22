//! The `LocalCodingRunner` abstraction (ENG-1528).
//!
//! One trait, one adapter per CLI (Claude Code here; Codex in ENG-1529).
//! Adapters are PURE with respect to I/O: they build the command line and
//! translate one stdout line at a time into normalized chunks. All process
//! handling (spawn, cancel, reap) stays in `super::run` so it is shared.

use serde::Serialize;
use tokio::process::Command;

/// What a run needs from the caller, already validated by the orchestrator
/// (cwd inside scoped folders, coding-run slot held).
pub struct RunSpec {
    pub prompt: String,
    /// Canonicalized working directory the CLI runs in.
    pub working_dir: std::path::PathBuf,
    /// Resume a previous session on this machine, if the engine sent one.
    pub session_id: Option<String>,
    /// User's API key for the CLI's backend, when that CLI needs one
    /// (Claude Code under the v1 auth decision; Codex uses its own login).
    pub api_key: Option<String>,
}

/// One normalized streaming chunk, forwarded to the engine as
/// `response_chunk.data`. Kept deliberately small and additive.
#[derive(Debug, Serialize, PartialEq)]
pub struct Chunk {
    /// "session" | "text" | "tool" | "file_changed" | "cost" | "status"
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Target file for "tool" (when the tool has one) and "file_changed".
    /// The live-run card's files-changed list and activity line read this
    /// (ENG-1552) — a bare tool name tells the user nothing they can audit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For "file_changed": "write" (file created/replaced) | "modify" (edited).
    /// No "delete" until Bash lands — the write-capable tool surface can't rm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<&'static str>,
    /// For "cost": the run's total cost so far in USD. `claude -p` only
    /// reports cost on its terminal result event, so today this arrives once
    /// at the end of the run rather than as live ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
}

impl Chunk {
    /// A chunk of `kind` with every optional field unset — construction sites
    /// spread onto this so adding a field never touches them again.
    pub fn bare(kind: &'static str) -> Self {
        Chunk {
            kind,
            text: None,
            session_id: None,
            path: None,
            change: None,
            total_cost_usd: None,
        }
    }
}

/// Terminal result of a run, embedded in the terminal `response.data`.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct RunResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The final assistant message, when the CLI reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub is_error: bool,
}

/// What an adapter makes of one stdout line.
#[derive(Debug, PartialEq)]
pub enum ParsedLine {
    /// Forward this chunk to the engine.
    Chunk(Chunk),
    /// Forward several chunks, in order (one assistant message can carry
    /// multiple tool calls, and an Edit/Write yields tool + file_changed).
    Chunks(Vec<Chunk>),
    /// The run's terminal payload (the CLI's own "result" event). The process
    /// may still take a moment to exit; the orchestrator keeps draining.
    Final(RunResult),
    /// Line understood but not worth forwarding (or unknown type — both CLIs
    /// document their stream schemas as additive, so unknowns are skipped,
    /// never errors).
    Ignore,
}

pub trait LocalCodingRunner: Send + Sync {
    /// Stable name used in params, settings, and audit ("claude" | "codex").
    fn name(&self) -> &'static str;

    /// Build the full command line for a run. The orchestrator applies
    /// process-group + stdio settings afterwards.
    fn build_command(&self, binary: &std::path::Path, spec: &RunSpec) -> Command;

    /// Translate one stdout line (NDJSON for both CLIs).
    fn parse_line(&self, line: &str) -> ParsedLine;

    /// Map a nonzero-exit / stderr tail to a typed, user-facing error.
    /// Stable `code:` prefixes — the engine matches on them.
    fn classify_failure(&self, exit_code: Option<i32>, stderr_tail: &str) -> String;
}
