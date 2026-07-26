//! Human-readable per-run log powering the local terminal live view.
//!
//! The CLI runs headlessly (stream-json on stdout), so there is no terminal a
//! user could watch. This mirrors the parsed chunk stream — the same
//! normalized vocabulary both adapters emit — into a plain-text log under the
//! app's log dir, and "Watch in Terminal" opens Terminal.app tailing it.
//!
//! Logging is strictly best-effort: any I/O failure disables the log and never
//! disturbs the run. Content mirrors what already streams to the engine
//! (assistant narration, tool activity, changed files) — never CLI internals
//! or file contents beyond what the run card shows.

use std::io::Write;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use super::runner::Chunk;

pub struct RunLog {
    file: std::fs::File,
    pub path: PathBuf,
    /// True while the last write was a text fragment without a trailing
    /// newline; structural markers then break the line first.
    mid_line: bool,
}

impl RunLog {
    /// Create `<app-log-dir>/coding-runs/<request_id>.log` with a header.
    /// Returns None (and logs) on any failure — the run proceeds unlogged.
    pub fn create(
        app: &AppHandle,
        request_id: &str,
        cli: &str,
        working_dir: &str,
    ) -> Option<RunLog> {
        let dir = match app.path().app_log_dir() {
            Ok(base) => base.join("coding-runs"),
            Err(e) => {
                log::warn!("run log disabled: no app log dir: {e}");
                return None;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("run log disabled: cannot create {}: {e}", dir.display());
            return None;
        }
        let path = dir.join(format!("{request_id}.log"));
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("run log disabled: cannot create {}: {e}", path.display());
                return None;
            }
        };
        let mut this = RunLog {
            file,
            path,
            mid_line: false,
        };
        let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        this.write_str(&format!(
            "== Beakr coding run ==\nstarted: {started}\ncli: {cli}\ndir: {working_dir}\n\n"
        ));
        Some(this)
    }

    /// Mirror one normalized chunk into the log, tail-friendly (flushed).
    pub fn chunk(&mut self, chunk: &Chunk) {
        match chunk.kind {
            "text" => {
                if let Some(text) = &chunk.text {
                    self.write_text_fragment(text);
                }
            }
            "tool" => {
                if let Some(label) = &chunk.text {
                    self.break_line();
                    self.write_str(&format!("-> {label}\n"));
                }
            }
            "file_changed" => {
                if let Some(path) = &chunk.path {
                    let verb = chunk.change.unwrap_or("changed");
                    self.break_line();
                    self.write_str(&format!("   [{verb}] {path}\n"));
                }
            }
            "status" => {
                if let Some(text) = &chunk.text {
                    self.break_line();
                    self.write_str(&format!("[{text}]\n"));
                }
            }
            "session" => {
                let cli = chunk.cli.unwrap_or("cli");
                let model = chunk.model.as_deref().unwrap_or("");
                self.break_line();
                if model.is_empty() {
                    self.write_str(&format!("[session started: {cli}]\n"));
                } else {
                    self.write_str(&format!("[session started: {cli} · {model}]\n"));
                }
            }
            // "command" duplicates the "tool" activity label for Codex, and
            // "cost" is covered by the footer.
            _ => {}
        }
    }

    /// Terminal footer for a successful (or device-cancelled) run.
    pub fn finish(&mut self, answer: Option<&str>, cost_usd: Option<f64>, cancelled: bool) {
        self.break_line();
        if cancelled {
            self.write_str("\n== run stopped by the user ==\n");
            return;
        }
        self.write_str("\n== run finished ==\n");
        if let Some(answer) = answer {
            self.write_str(answer);
            if !answer.ends_with('\n') {
                self.write_str("\n");
            }
        }
        if let Some(cost) = cost_usd {
            self.write_str(&format!("cost: ${cost:.4}\n"));
        }
    }

    /// Terminal footer for a failed run (typed error string).
    pub fn finish_error(&mut self, error: &str) {
        self.break_line();
        self.write_str(&format!("\n== run failed ==\n{error}\n"));
    }

    fn write_text_fragment(&mut self, text: &str) {
        self.mid_line = !text.ends_with('\n');
        let _ = self.file.write_all(text.as_bytes());
        let _ = self.file.flush();
    }

    fn break_line(&mut self) {
        if self.mid_line {
            let _ = self.file.write_all(b"\n");
            self.mid_line = false;
        }
    }

    fn write_str(&mut self, s: &str) {
        self.mid_line = false;
        let _ = self.file.write_all(s.as_bytes());
        let _ = self.file.flush();
    }
}

/// Open Terminal.app tailing `log_path`, via a generated `.command` file —
/// the standard macOS way to hand Terminal a command without scripting
/// permissions. The wrapper lives next to the log and is overwritten per use.
#[cfg(target_os = "macos")]
pub fn open_log_in_terminal(log_path: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let log = Path::new(log_path);
    if !log.is_file() {
        return Err("the run's log file does not exist yet".to_string());
    }
    let dir = log.parent().ok_or("log path has no parent directory")?;
    let stem = log
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "run".to_string());
    let wrapper = dir.join(format!("watch-{stem}.command"));

    // Single-quote for zsh, escaping embedded single quotes.
    let quoted = format!("'{}'", log_path.replace('\'', r"'\''"));
    let script = format!(
        "#!/bin/zsh\nclear\necho 'Beakr coding run - live view (Ctrl+C to stop watching; the run itself is not affected)'\necho\ntail -n +1 -f {quoted}\n"
    );
    std::fs::write(&wrapper, script).map_err(|e| format!("could not write launcher: {e}"))?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("could not mark launcher executable: {e}"))?;

    let status = std::process::Command::new("open")
        .arg(&wrapper)
        .status()
        .map_err(|e| format!("could not open Terminal: {e}"))?;
    if !status.success() {
        return Err("Terminal did not open the live view".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn open_log_in_terminal(_log_path: &str) -> Result<(), String> {
    Err("watching a run in a terminal is only supported on macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &'static str) -> Chunk {
        Chunk::bare(kind)
    }

    fn log_in(dir: &Path) -> RunLog {
        RunLog {
            file: std::fs::File::create(dir.join("t.log")).unwrap(),
            path: dir.join("t.log"),
            mid_line: false,
        }
    }

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("t.log")).unwrap()
    }

    #[test]
    fn text_fragments_concatenate_and_markers_break_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = log_in(dir.path());
        log.chunk(&Chunk {
            text: Some("Fixing".into()),
            ..chunk("text")
        });
        log.chunk(&Chunk {
            text: Some(" the tests".into()),
            ..chunk("text")
        });
        log.chunk(&Chunk {
            text: Some("Edit app.py".into()),
            ..chunk("tool")
        });
        log.chunk(&Chunk {
            path: Some("/repo/app.py".into()),
            change: Some("modify"),
            ..chunk("file_changed")
        });
        let content = read(dir.path());
        // The token fragments join into one line; the tool marker starts its own.
        assert!(content.contains("Fixing the tests\n-> Edit app.py\n"));
        assert!(content.contains("   [modify] /repo/app.py\n"));
    }

    #[test]
    fn finish_writes_answer_and_cost() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = log_in(dir.path());
        log.finish(Some("Done - 3 tests fixed."), Some(0.1234), false);
        let content = read(dir.path());
        assert!(content.contains("== run finished =="));
        // The answer gets its own line — cost must not concatenate onto it
        // (live-verified nit: "...agent.cost: $" without this).
        assert!(content.contains("Done - 3 tests fixed.\ncost: $0.1234"));
    }

    #[test]
    fn finish_error_and_cancelled_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = log_in(dir.path());
        log.finish_error("auth_failed: not logged in");
        log.finish(None, None, true);
        let content = read(dir.path());
        assert!(content.contains("== run failed ==\nauth_failed: not logged in"));
        assert!(content.contains("== run stopped by the user =="));
    }
}
