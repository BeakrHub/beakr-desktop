use std::path::Path;

use serde_json::{json, Value};

use crate::security;

/// Handle a `reveal_file` request: select the file in the OS file manager
/// (Finder on macOS). Triggered by an explicit user click in the web UI and
/// relayed by the engine — this tool is deliberately NOT exposed to the LLM,
/// so a prompt-injected document can never pop windows on the user's machine.
/// No file content is read or transferred.
///
/// Params:
/// - `path` (string, required): File path to reveal
pub async fn handle(
    params: Value,
    scoped_folders: &[String],
) -> Result<(Value, Option<u64>), String> {
    handle_with(params, scoped_folders, reveal_in_file_manager)
}

/// The validation core, with the OS side effect injected so tests can cover
/// every gate without launching Finder.
fn handle_with<F>(
    params: Value,
    scoped_folders: &[String],
    reveal: F,
) -> Result<(Value, Option<u64>), String>
where
    F: Fn(&Path) -> Result<(), String>,
{
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("reveal_file requires 'path' parameter")?;

    // Same gate as read_file: inside a granted folder and not deny-listed.
    let canonical = security::validate_path(path, scoped_folders).map_err(|e| e.to_string())?;
    if security::is_denied(&canonical) {
        return Err(format!("Access denied — sensitive file: {path}"));
    }
    if !canonical.exists() {
        return Err(format!("File not found: {path}"));
    }

    reveal(&canonical)?;

    Ok((
        json!({
            "revealed": true,
            "path": canonical.display().to_string(),
        }),
        None,
    ))
}

/// `open -R` activates Finder with the file selected; it does not open or
/// execute the file itself.
#[cfg(target_os = "macos")]
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to launch Finder: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Finder returned a non-zero status for: {}",
            path.display()
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn reveal_in_file_manager(_path: &Path) -> Result<(), String> {
    // Windows/Linux support arrives with ENG-206; better a clear error than
    // a silent no-op the user interprets as a broken button.
    Err("Revealing files is not supported on this platform yet.".to_string())
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

    /// Same TempTree convention as search_files.rs (no tempfile dev-dep).
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "beakr_reveal_test_{tag}_{}_{n}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
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

    fn never_reveal(_: &Path) -> Result<(), String> {
        panic!("reveal must not be called when validation fails");
    }

    #[test]
    fn reveals_a_file_inside_scope() {
        let tree = TempTree::new("ok");
        let file = tree.write("notes.md", "content");

        let revealed = Cell::new(false);
        let (value, _) = handle_with(
            json!({ "path": file.display().to_string() }),
            &tree.scoped(),
            |p| {
                assert!(p.ends_with("notes.md"));
                revealed.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(revealed.get());
        assert_eq!(value.get("revealed").unwrap(), true);
    }

    #[test]
    fn rejects_missing_path_param() {
        let tree = TempTree::new("noparam");
        let err = handle_with(json!({}), &tree.scoped(), never_reveal).unwrap_err();
        assert!(err.contains("requires 'path'"), "got {err}");
    }

    #[test]
    fn rejects_path_outside_scope() {
        let tree = TempTree::new("scope");
        tree.write("inside.md", "content");
        // temp_dir is the parent of the scoped root, so it is out of scope.
        let outside = env::temp_dir().join("outside-reveal-test.md");
        fs::write(&outside, "content").unwrap();

        let result = handle_with(
            json!({ "path": outside.display().to_string() }),
            &tree.scoped(),
            never_reveal,
        );
        fs::remove_file(&outside).ok();
        assert!(result.is_err(), "expected out-of-scope error, got {result:?}");
    }

    #[test]
    fn rejects_deny_listed_file() {
        let tree = TempTree::new("deny");
        let secret = tree.write(".env", "SECRET=1");

        let err = handle_with(
            json!({ "path": secret.display().to_string() }),
            &tree.scoped(),
            never_reveal,
        )
        .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
    }

    #[test]
    fn rejects_missing_file() {
        let tree = TempTree::new("gone");

        let result = handle_with(
            json!({ "path": tree.root.join("ghost.md").display().to_string() }),
            &tree.scoped(),
            never_reveal,
        );
        assert!(result.is_err(), "expected not-found error, got {result:?}");
    }
}
