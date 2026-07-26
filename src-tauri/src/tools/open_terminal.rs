use std::path::Path;

use serde_json::{json, Value};

use crate::security;

/// Handle an `open_terminal` request: open a terminal window at a folder
/// (Terminal.app on macOS). Triggered by an explicit user click on the
/// coding-run card's "Open in Terminal" in the web UI and relayed by the
/// engine — this tool is deliberately NOT exposed to the LLM, so a
/// prompt-injected document can never spawn terminals on the user's machine.
/// No file content is read or transferred, and no command is run in the shell.
///
/// Params:
/// - `path` (string, required): Folder to open the terminal in (the coding
///   run's working directory).
pub async fn handle(
    params: Value,
    scoped_folders: &[String],
) -> Result<(Value, Option<u64>), String> {
    handle_with(params, scoped_folders, open_terminal_at)
}

/// The validation core, with the OS side effect injected so tests can cover
/// every gate without launching Terminal.
fn handle_with<F>(
    params: Value,
    scoped_folders: &[String],
    open: F,
) -> Result<(Value, Option<u64>), String>
where
    F: Fn(&Path) -> Result<(), String>,
{
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("open_terminal requires 'path' parameter")?;

    // Same gate as reveal_file: inside a granted folder and not deny-listed.
    let canonical = security::validate_path(path, scoped_folders).map_err(|e| e.to_string())?;
    if security::is_denied(&canonical) {
        return Err(format!("Access denied — sensitive path: {path}"));
    }
    // A terminal opens at a directory. Refusing a file (or anything else) keeps
    // the contract honest — the coding run's working_dir is always a folder.
    if !canonical.is_dir() {
        return Err(format!("Not a folder: {path}"));
    }

    open(&canonical)?;

    Ok((
        json!({
            "opened": true,
            "path": canonical.display().to_string(),
        }),
        None,
    ))
}

/// `open -a Terminal <dir>` launches Terminal.app with a new window whose
/// working directory is <dir>. It opens no file and runs no command.
#[cfg(target_os = "macos")]
fn open_terminal_at(path: &Path) -> Result<(), String> {
    let status = std::process::Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to launch Terminal: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Terminal returned a non-zero status for: {}",
            path.display()
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn open_terminal_at(_path: &Path) -> Result<(), String> {
    // Windows/Linux support arrives with ENG-206 (same as reveal_file); a clear
    // error beats a silent no-op the user reads as a broken button.
    Err("Opening a terminal is not supported on this platform yet.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Same TempTree convention as reveal_file.rs (no tempfile dev-dep).
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "beakr_open_terminal_test_{tag}_{}_{n}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }

        fn scoped(&self) -> Vec<String> {
            vec![self.root.display().to_string()]
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn never_open(_: &Path) -> Result<(), String> {
        panic!("open must not be called when validation fails");
    }

    #[test]
    fn opens_a_folder_inside_scope() {
        let tree = TempTree::new("ok");
        let dir = tree.mkdir("project");

        let opened = Cell::new(false);
        let (value, _) = handle_with(
            json!({ "path": dir.display().to_string() }),
            &tree.scoped(),
            |p| {
                assert!(p.ends_with("project"));
                opened.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(opened.get());
        assert_eq!(value.get("opened").unwrap(), true);
    }

    #[test]
    fn rejects_missing_path_param() {
        let tree = TempTree::new("noparam");
        let err = handle_with(json!({}), &tree.scoped(), never_open).unwrap_err();
        assert!(err.contains("requires 'path'"), "got {err}");
    }

    #[test]
    fn rejects_path_outside_scope() {
        let tree = TempTree::new("scope");
        tree.mkdir("inside");
        // temp_dir is the parent of the scoped root, so it is out of scope.
        let outside = env::temp_dir().join(format!("outside-open-terminal-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();

        let result = handle_with(
            json!({ "path": outside.display().to_string() }),
            &tree.scoped(),
            never_open,
        );
        fs::remove_dir_all(&outside).ok();
        assert!(result.is_err(), "expected out-of-scope error, got {result:?}");
    }

    #[test]
    fn rejects_a_file_not_a_folder() {
        let tree = TempTree::new("file");
        let file = tree.write("notes.md", "content");

        let err = handle_with(
            json!({ "path": file.display().to_string() }),
            &tree.scoped(),
            never_open,
        )
        .unwrap_err();
        assert!(err.contains("Not a folder"), "got {err}");
    }

    #[test]
    fn rejects_deny_listed_folder() {
        let tree = TempTree::new("deny");
        let secret = tree.mkdir(".ssh");

        let err = handle_with(
            json!({ "path": secret.display().to_string() }),
            &tree.scoped(),
            never_open,
        )
        .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
    }
}
