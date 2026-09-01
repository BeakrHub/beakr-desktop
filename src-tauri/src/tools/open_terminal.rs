use std::path::Path;

#[cfg(target_os = "windows")]
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "windows")]
use std::io;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};

use crate::security;

/// Handle an `open_terminal` request: open a terminal window at a folder
/// (Terminal.app on macOS, Windows Terminal or PowerShell on Windows).
/// Triggered by an explicit user click on the
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

#[cfg(target_os = "windows")]
const CREATE_NEW_CONSOLE: u32 = 0x00000010;

/// Prefer Windows Terminal and force a new window rooted at `path`. Windows
/// Terminal is optional, so fall back to a new Windows PowerShell console with
/// the child's working directory already set. No shell command or path string
/// is evaluated, which keeps folder names from becoming command input.
#[cfg(target_os = "windows")]
fn open_terminal_at(path: &Path) -> Result<(), String> {
    open_terminal_on_windows_with(path, |program, args, working_dir, creation_flags| {
        let mut command = std::process::Command::new(program);
        command.args(args).current_dir(working_dir);
        if creation_flags != 0 {
            command.creation_flags(creation_flags);
        }
        command.spawn().map(|_| ())
    })
}

#[cfg(target_os = "windows")]
fn open_terminal_on_windows_with<F>(path: &Path, mut launch: F) -> Result<(), String>
where
    F: FnMut(&OsStr, &[OsString], &Path, u32) -> io::Result<()>,
{
    let windows_terminal_args = [
        OsString::from("-w"),
        OsString::from("new"),
        OsString::from("-d"),
        path.as_os_str().to_os_string(),
    ];

    match launch(OsStr::new("wt.exe"), &windows_terminal_args, path, 0) {
        Ok(()) => Ok(()),
        Err(windows_terminal_error) => {
            let powershell_args = [OsString::from("-NoLogo"), OsString::from("-NoExit")];
            launch(
                OsStr::new("powershell.exe"),
                &powershell_args,
                path,
                CREATE_NEW_CONSOLE,
            )
            .map_err(|powershell_error| {
                format!(
                    "Failed to launch Windows Terminal ({windows_terminal_error}); \
                     PowerShell fallback also failed ({powershell_error})"
                )
            })
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn open_terminal_at(_path: &Path) -> Result<(), String> {
    // Linux is not currently a supported desktop release; keep the failure
    // explicit rather than silently ignoring an intentional user click.
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
        assert!(
            result.is_err(),
            "expected out-of-scope error, got {result:?}"
        );
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

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_prefers_a_new_windows_terminal_window_at_the_folder() {
        let tree = TempTree::new("windows-terminal");
        let dir = tree.mkdir("project with spaces");
        let mut calls = Vec::new();

        open_terminal_on_windows_with(&dir, |program, args, working_dir, flags| {
            calls.push((
                program.to_os_string(),
                args.to_vec(),
                working_dir.to_path_buf(),
                flags,
            ));
            Ok(())
        })
        .unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, OsString::from("wt.exe"));
        assert_eq!(
            calls[0].1,
            vec![
                OsString::from("-w"),
                OsString::from("new"),
                OsString::from("-d"),
                dir.as_os_str().to_os_string(),
            ]
        );
        assert_eq!(calls[0].2, dir);
        assert_eq!(calls[0].3, 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_falls_back_to_a_new_powershell_console() {
        let tree = TempTree::new("powershell-fallback");
        let dir = tree.mkdir("project");
        let mut calls = Vec::new();

        open_terminal_on_windows_with(&dir, |program, args, working_dir, flags| {
            calls.push((
                program.to_os_string(),
                args.to_vec(),
                working_dir.to_path_buf(),
                flags,
            ));
            if program == OsStr::new("wt.exe") {
                Err(io::Error::new(io::ErrorKind::NotFound, "not installed"))
            } else {
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, OsString::from("powershell.exe"));
        assert_eq!(
            calls[1].1,
            vec![OsString::from("-NoLogo"), OsString::from("-NoExit")]
        );
        assert_eq!(calls[1].2, dir);
        assert_eq!(calls[1].3, CREATE_NEW_CONSOLE);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reports_both_launcher_failures() {
        let tree = TempTree::new("launcher-errors");
        let dir = tree.mkdir("project");

        let err = open_terminal_on_windows_with(&dir, |program, _, _, _| {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} missing", program.to_string_lossy()),
            ))
        })
        .unwrap_err();

        assert!(err.contains("Windows Terminal"), "got {err}");
        assert!(err.contains("PowerShell fallback"), "got {err}");
    }
}
