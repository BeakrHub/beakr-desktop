//! CLI binary resolution (ENG-1528).
//!
//! A Finder-launched .app gets a minimal PATH (`/usr/bin:/bin:...`), so a bare
//! `claude` / `codex` usually isn't findable. Resolution order:
//! 1. explicit user override from Settings,
//! 2. the user's login shell (`$SHELL -lc "command -v <name>"` — picks up
//!    nvm/npm/Homebrew profiles),
//! 3. well-known install locations.

use std::path::PathBuf;

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

    if let Some(p) = via_login_shell(name) {
        return Ok(p);
    }

    for candidate in well_known_paths(name) {
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }

    Err(format!(
        "binary_not_found: `{name}` was not found on this Mac. Install it, or set its \
         path in Beakr Desktop settings."
    ))
}

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

fn well_known_paths(name: &str) -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        PathBuf::from(format!("{home}/.local/bin/{name}")),
        PathBuf::from(format!("/opt/homebrew/bin/{name}")),
        PathBuf::from(format!("/usr/local/bin/{name}")),
        PathBuf::from(format!("{home}/.npm-global/bin/{name}")),
        PathBuf::from(format!("{home}/.claude/local/{name}")),
    ]
}

fn is_executable(p: &std::path::Path) -> bool {
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
        p.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins_and_must_be_executable() {
        // /bin/ls is always executable on macOS/Linux.
        let ok = resolve("anything", Some("/bin/ls"));
        assert_eq!(ok.unwrap(), PathBuf::from("/bin/ls"));

        let missing = resolve("anything", Some("/nonexistent/claude"));
        assert!(missing.unwrap_err().starts_with("binary_not_found:"));
    }

    #[test]
    fn unknown_binary_yields_typed_error() {
        let err = resolve("definitely-not-a-real-cli-xyz", None).unwrap_err();
        assert!(err.starts_with("binary_not_found:"));
        assert!(err.contains("definitely-not-a-real-cli-xyz"));
    }

    #[test]
    fn login_shell_finds_standard_tools() {
        // `ls` exists in every login shell PATH — proves the resolution path
        // itself works end-to-end without depending on claude being installed.
        let p = resolve("ls", None).expect("ls resolvable via login shell");
        assert!(is_executable(&p));
    }
}
