//! CLI binary resolution (ENG-1528).
//!
//! A Finder-launched .app gets a minimal PATH (`/usr/bin:/bin:...`), so a bare
//! `claude` / `codex` usually isn't findable. Resolution order:
//! 1. explicit user override from Settings,
//! 2. the user's login shell (`$SHELL -lc "command -v <name>"` — picks up
//!    nvm/npm/Homebrew profiles),
//! 3. well-known install locations.

use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Build a process command for a resolved CLI. npm installs command shims as
/// `.cmd` files on Windows; CreateProcess cannot execute those directly.
pub(super) fn command(binary: &Path) -> Command {
    #[cfg(target_os = "windows")]
    if matches!(
        binary.extension().and_then(|extension| extension.to_str()),
        Some(extension) if extension.eq_ignore_ascii_case("cmd")
            || extension.eq_ignore_ascii_case("bat")
    ) {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C"]).arg(binary);
        return command;
    }

    Command::new(binary)
}

pub fn resolve(name: &str, settings_override: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = settings_override {
        let p = PathBuf::from(path);
        if is_executable(&p) {
            return Ok(p);
        }
        return Err(format!(
            "binary_not_found: configured path for {name} is not executable: {path}"
        ));
    }

    #[cfg(target_os = "windows")]
    if let Some(p) = via_windows_path(name) {
        return Ok(p);
    }

    #[cfg(not(target_os = "windows"))]
    if let Some(p) = via_login_shell(name) {
        return Ok(p);
    }

    for candidate in well_known_paths(name) {
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }

    Err(format!(
        "binary_not_found: `{name}` was not found on this computer. Install it, or set its \
         path in Beakr Desktop settings."
    ))
}

#[cfg(not(target_os = "windows"))]
fn via_login_shell(name: &str) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let out = std::process::Command::new(shell)
        .args(["-lc", &format!("command -v {name}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    is_executable(&p).then_some(p)
}

#[cfg(target_os = "windows")]
fn via_windows_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    let pathext = std::env::var("PATHEXT").ok();
    via_windows_path_with(name, path.as_deref(), pathext.as_deref())
}

#[cfg(target_os = "windows")]
fn via_windows_path_with(
    name: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&str>,
) -> Option<PathBuf> {
    let path = path?;
    let mut names = vec![std::ffi::OsString::from(name)];
    if Path::new(name).extension().is_none() {
        let extensions = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
        names.extend(
            extensions
                .split(';')
                .map(str::trim)
                .filter(|extension| !extension.is_empty())
                .map(|extension| {
                    let extension = if extension.starts_with('.') {
                        extension.to_string()
                    } else {
                        format!(".{extension}")
                    };
                    std::ffi::OsString::from(format!("{name}{extension}"))
                }),
        );
    }

    for directory in std::env::split_paths(path) {
        for executable_name in &names {
            let candidate = directory.join(executable_name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn well_known_paths(name: &str) -> Vec<PathBuf> {
    let home = home_dir().unwrap_or_default();
    vec![
        home.join(".local").join("bin").join(name),
        PathBuf::from(format!("/opt/homebrew/bin/{name}")),
        PathBuf::from(format!("/usr/local/bin/{name}")),
        home.join(".npm-global").join("bin").join(name),
        home.join(".claude").join("local").join(name),
    ]
}

#[cfg(target_os = "windows")]
fn well_known_paths(name: &str) -> Vec<PathBuf> {
    let mut directories = Vec::new();

    if let Some(app_data) = std::env::var_os("APPDATA") {
        directories.push(PathBuf::from(app_data).join("npm"));
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let local_app_data = PathBuf::from(local_app_data);
        directories.push(local_app_data.join("Programs").join(name));
        directories.push(local_app_data.join(name));
    }
    if let Some(user_profile) = home_dir() {
        directories.push(user_profile.join(".local").join("bin"));
        directories.push(user_profile.join(".npm-global").join("bin"));
        directories.push(user_profile.join(".claude").join("local"));
    }

    let mut candidates = Vec::new();
    for directory in directories {
        for extension in ["exe", "cmd", "bat"] {
            candidates.push(directory.join(format!("{name}.{extension}")));
        }
    }
    candidates
}

#[cfg(target_os = "windows")]
pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // Packaged-app resources can appear on PATH and expose metadata while
        // denying execution to other desktop processes (for example a CLI
        // inside WindowsApps). Requiring ordinary file access filters those
        // false positives without launching the CLI during resolution.
        p.is_file() && std::fs::File::open(p).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_path_honors_pathext_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.cmd");
        std::fs::write(&script, "@echo off\r\necho codex-test\r\n").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();

        let resolved = via_windows_path_with("codex", Some(&path), Some(".EXE;.CMD"));

        assert_eq!(
            resolved.unwrap().canonicalize().unwrap(),
            script.canonicalize().unwrap()
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_command_runs_cmd_shims() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.cmd");
        std::fs::write(
            &script,
            "@echo off\r\nif \"%~1\"==\"--version\" echo codex-cli 1959.0-test\r\n",
        )
        .unwrap();

        let output = command(&script).arg("--version").output().await.unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "codex-cli 1959.0-test"
        );
    }

    #[test]
    fn user_facing_copy_is_platform_neutral() {
        let sources = [
            include_str!("../../../../src/components/CodingAgentSettings.tsx"),
            include_str!("claude.rs"),
            include_str!("codex.rs"),
        ];

        for source in sources {
            assert!(!source.contains("this Mac"));
        }

        let error = resolve("definitely-not-a-real-cli-copy-test", None).unwrap_err();
        assert!(!error.contains("this Mac"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_override_wins_and_must_be_executable() {
        // /bin/ls is always executable on macOS/Linux.
        let ok = resolve("anything", Some("/bin/ls"));
        assert_eq!(ok.unwrap(), PathBuf::from("/bin/ls"));

        let missing = resolve("anything", Some("/nonexistent/claude"));
        assert!(missing.unwrap_err().starts_with("binary_not_found:"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn explicit_override_wins_and_must_be_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("anything.cmd");
        std::fs::write(&executable, "@echo off\r\n").unwrap();

        assert_eq!(
            resolve("anything", Some(executable.to_str().unwrap())).unwrap(),
            executable
        );

        let missing = resolve("anything", Some(r"C:\nonexistent\claude.cmd"));
        assert!(missing.unwrap_err().starts_with("binary_not_found:"));
    }

    #[test]
    fn unknown_binary_yields_typed_error() {
        let err = resolve("definitely-not-a-real-cli-xyz", None).unwrap_err();
        assert!(err.starts_with("binary_not_found:"));
        assert!(err.contains("definitely-not-a-real-cli-xyz"));
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_finds_standard_tools() {
        // `ls` exists in every login shell PATH — proves the resolution path
        // itself works end-to-end without depending on claude being installed.
        let p = resolve("ls", None).expect("ls resolvable via login shell");
        assert!(is_executable(&p));
    }
}
