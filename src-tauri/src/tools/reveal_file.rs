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

#[cfg(target_os = "windows")]
fn windows_shell_display_path(path: &Path) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    const VERBATIM_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC_PREFIX: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    if wide.starts_with(VERBATIM_UNC_PREFIX) {
        let mut shell_path = vec![b'\\' as u16, b'\\' as u16];
        shell_path.extend_from_slice(&wide[VERBATIM_UNC_PREFIX.len()..]);
        std::ffi::OsString::from_wide(&shell_path)
    } else if wide.starts_with(VERBATIM_PREFIX) {
        std::ffi::OsString::from_wide(&wide[VERBATIM_PREFIX.len()..])
    } else {
        path.as_os_str().to_os_string()
    }
}

#[cfg(target_os = "windows")]
fn windows_shell_item_id_list(
    path: &Path,
) -> Result<*mut windows_sys::Win32::UI::Shell::Common::ITEMIDLIST, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ILCreateFromPathW;

    // Rust canonicalization adds a verbatim `\\?\` prefix to long Windows
    // paths. Keep that canonical form everywhere else, but translate only the
    // Shell display name: Shell item parsing rejects the verbatim namespace.
    let shell_path = windows_shell_display_path(path);
    let mut wide_path: Vec<u16> = shell_path.encode_wide().collect();
    wide_path.push(0);

    // SAFETY: `wide_path` is NUL-terminated and remains alive for this call.
    unsafe {
        let item_id_list = ILCreateFromPathW(wide_path.as_ptr());
        if item_id_list.is_null() {
            Err(format!(
                "Windows Shell could not resolve the file path: {}",
                path.display()
            ))
        } else {
            Ok(item_id_list)
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_shell_select_item(path: &Path) -> Result<(), String> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::{ILFree, SHOpenFolderAndSelectItems};

    let item_id_list = windows_shell_item_id_list(path)?;

    // SAFETY: `item_id_list` is a valid PIDL allocated by ILCreateFromPathW.
    // ILFree releases it after SHOpenFolderAndSelectItems has synchronously
    // consumed it.
    unsafe {
        // With cidl == 0, the Shell contract treats pidlFolder as the fully
        // qualified item to select, opens its parent, and selects that item.
        let result = SHOpenFolderAndSelectItems(item_id_list, 0, ptr::null(), 0);
        ILFree(item_id_list);

        if result < 0 {
            Err(format!(
                "Windows Shell selection failed with HRESULT 0x{:08X}",
                result as u32
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
fn reveal_with_windows_shell<F>(path: &Path, select_item: F) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    select_item(path).map_err(|error| format!("Failed to reveal {}: {error}", path.display()))
}

#[cfg(target_os = "windows")]
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    reveal_with_windows_shell(path, windows_shell_select_item)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn reveal_in_file_manager(_path: &Path) -> Result<(), String> {
    // Better a clear error than a silent no-op the user interprets as a broken button.
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

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_shell_selection_success_is_reported() {
        let path = Path::new(r"C:\Dev\beakr-windows-test\onboarding checklist.txt");

        reveal_with_windows_shell(path, |received| {
            assert_eq!(received, path);
            Ok(())
        })
        .unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_shell_display_path_translates_only_verbatim_namespace() {
        use std::ffi::OsString;

        assert_eq!(
            windows_shell_display_path(Path::new(r"\\?\C:\very\long\file.md")),
            OsString::from(r"C:\very\long\file.md")
        );
        assert_eq!(
            windows_shell_display_path(Path::new(r"\\?\UNC\server\share\file.md")),
            OsString::from(r"\\server\share\file.md")
        );
        assert_eq!(
            windows_shell_display_path(Path::new(r"C:\ordinary\file.md")),
            OsString::from(r"C:\ordinary\file.md")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reveal_propagates_shell_selection_failure() {
        let path = Path::new(r"C:\Dev\beakr-windows-test\notes.md");

        let err = reveal_with_windows_shell(path, |_| {
            Err("Shell selection failed with HRESULT 0x800700CE".to_string())
        })
        .unwrap_err();

        assert!(err.contains("0x800700CE"), "got {err}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_shell_resolves_path_beyond_legacy_max_path() {
        use windows_sys::Win32::UI::Shell::ILFree;

        let tree = TempTree::new("long_shell_path");
        let mut directory = tree.root.clone();
        let mut segment = 0;
        while directory.join("deep-file.md").as_os_str().len() <= 300 {
            directory.push(format!("level_{segment:02}_padding_segment_for_long_path"));
            segment += 1;
        }
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("deep-file.md");
        fs::write(&file, "long path reveal fixture").unwrap();
        let canonical = fs::canonicalize(&file).unwrap();
        assert!(
            canonical.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "Windows long-path fixture should canonicalize with a verbatim prefix: {}",
            canonical.display()
        );

        let item_id_list = windows_shell_item_id_list(&canonical).unwrap();
        assert!(!item_id_list.is_null());

        // SAFETY: the helper returned a PIDL allocated by ILCreateFromPathW.
        unsafe { ILFree(item_id_list) };
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
