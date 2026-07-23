//! Per-CLI readiness detection (ENG-1536, DESIGN.md decision 8).
//!
//! Tells the user UP FRONT whether this machine can run a local coding agent,
//! instead of letting a run fail mid-turn. Three facts per CLI: installed?
//! signed in? which version?
//!
//! HARD RULE — no quota-burning probes (David, 2026-07-17): a "trivial" test
//! run against a logged-in CLI is a real API call on the user's plan.
//! Sign-in detection therefore uses only free signals, in order:
//! 1. credential PRESENCE on disk (`~/.codex/auth.json`;
//!    `~/.claude/.credentials.json`; the macOS keychain item Claude Code
//!    writes) — reading presence, never contents;
//! 2. the cached outcome of the most recent REAL run (config::record_cli_auth)
//!    — a run that just succeeded is the strongest proof of login there is;
//! 3. otherwise an honest "unknown — verifies on first run".
//! Self-healing: a stale signed_in lets a run through that then fails typed
//! as auth_failed, which flips the cache to false.

use serde::Serialize;

use super::binary;

/// What the settings UI / WS register report per CLI. Field names are a
/// frontend + engine contract.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CliReadiness {
    /// "claude" | "codex"
    pub cli: &'static str,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// "signed_in" | "not_signed_in" | "unknown"
    pub login: &'static str,
    /// True when a run could plausibly work now (installed && login is not
    /// definitively absent). The engine/web gate on this single bit.
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Presence {
    Present,
    Absent,
    Unknown,
}

/// Credential presence from the filesystem, relative to `home` (injectable
/// for tests). Never reads file contents.
fn credential_presence_in(cli: &str, home: &std::path::Path) -> Presence {
    match cli {
        "codex" => {
            if home.join(".codex/auth.json").is_file() {
                Presence::Present
            } else {
                Presence::Absent
            }
        }
        "claude" => {
            // Linux/other: a plain credentials file. On macOS the credential
            // usually lives in the keychain instead (checked separately) —
            // a missing file here is NOT evidence of absence.
            if home.join(".claude/.credentials.json").is_file() {
                Presence::Present
            } else {
                Presence::Unknown
            }
        }
        _ => Presence::Unknown,
    }
}

/// macOS keychain presence for Claude Code's credential item. Queries item
/// EXISTENCE only (no -w — the secret is never read). Any failure to run the
/// check degrades to Unknown, never to Absent.
#[cfg(target_os = "macos")]
fn claude_keychain_presence() -> Presence {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "Claude Code-credentials"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match out {
        Ok(status) if status.success() => Presence::Present,
        Ok(_) => Presence::Absent, // ran fine, item not found
        Err(_) => Presence::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_presence() -> Presence {
    Presence::Unknown
}

/// Combine free signals into the login state. Pure — fully unit-tested.
///
/// `cached_auth_ok` is the last real run's outcome. It outranks an Absent
/// presence on purpose: presence heuristics can miss a login location (e.g.
/// keychain variants), while a successful run is ground truth. The converse
/// stale-cache case self-heals via the next run's auth_failed.
fn login_state(presence: Presence, cached_auth_ok: Option<bool>) -> &'static str {
    match (presence, cached_auth_ok) {
        (Presence::Present, _) => "signed_in",
        (_, Some(true)) => "signed_in",
        (Presence::Absent, _) | (_, Some(false)) => "not_signed_in",
        (Presence::Unknown, None) => "unknown",
    }
}

fn presence_for(cli: &str) -> Presence {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let fs_presence = credential_presence_in(cli, &home);
    if cli == "claude" && fs_presence == Presence::Unknown {
        return claude_keychain_presence();
    }
    fs_presence
}

/// `<binary> --version`, best-effort with a hard timeout. Local spawn only —
/// never an API call.
async fn probe_version(binary: &std::path::Path) -> Option<String> {
    let fut = tokio::process::Command::new(binary)
        .arg("--version")
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Full readiness for one CLI. `binary_override` is the per-CLI settings
/// path override (claude only today); `cached_auth_ok` the recorded last-run
/// outcome. `with_version` controls the (local, but slow-ish) version spawn —
/// the settings UI wants it, the WS register skips it.
pub async fn detect(
    cli: &'static str,
    binary_override: Option<&str>,
    cached_auth_ok: Option<bool>,
    with_version: bool,
) -> CliReadiness {
    let resolved = binary::resolve(cli, binary_override);
    let (installed, binary_path) = match &resolved {
        Ok(p) => (true, Some(p.to_string_lossy().into_owned())),
        Err(_) => (false, None),
    };

    if !installed {
        return CliReadiness {
            cli,
            installed: false,
            binary_path: None,
            version: None,
            login: "unknown",
            ready: false,
        };
    }

    let login = login_state(presence_for(cli), cached_auth_ok);
    let version = if with_version {
        probe_version(resolved.as_ref().expect("installed").as_path()).await
    } else {
        None
    };

    CliReadiness {
        cli,
        installed: true,
        binary_path,
        version,
        // "unknown" stays ready: the first run verifies for real, and gating
        // it off would dead-end users whose login we simply can't see.
        ready: login != "not_signed_in",
        login,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_state_prefers_presence_then_cache_then_honest_unknown() {
        // Presence on disk wins outright.
        assert_eq!(login_state(Presence::Present, None), "signed_in");
        assert_eq!(login_state(Presence::Present, Some(false)), "signed_in");
        // A successful real run outranks an Absent presence heuristic —
        // presence can miss login locations; a run that worked cannot lie.
        assert_eq!(login_state(Presence::Absent, Some(true)), "signed_in");
        assert_eq!(login_state(Presence::Unknown, Some(true)), "signed_in");
        // Definitive absence signals.
        assert_eq!(login_state(Presence::Absent, None), "not_signed_in");
        assert_eq!(login_state(Presence::Unknown, Some(false)), "not_signed_in");
        // No signal at all: say so, don't guess.
        assert_eq!(login_state(Presence::Unknown, None), "unknown");
    }

    #[test]
    fn codex_credential_presence_is_definitive_either_way() {
        let tmp = std::env::temp_dir().join(format!("readiness-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".codex")).unwrap();

        assert_eq!(credential_presence_in("codex", &tmp), Presence::Absent);
        std::fs::write(tmp.join(".codex/auth.json"), "{}").unwrap();
        assert_eq!(credential_presence_in("codex", &tmp), Presence::Present);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn claude_missing_credentials_file_is_unknown_not_absent() {
        // On macOS the credential normally lives in the keychain, so a
        // missing ~/.claude/.credentials.json must NOT read as logged-out.
        let tmp = std::env::temp_dir().join(format!("readiness-test-c-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(credential_presence_in("claude", &tmp), Presence::Unknown);

        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        std::fs::write(tmp.join(".claude/.credentials.json"), "{}").unwrap();
        assert_eq!(credential_presence_in("claude", &tmp), Presence::Present);

        std::fs::remove_dir_all(&tmp).ok();
    }

    fn agent(cli: &'static str, installed: bool) -> CliReadiness {
        CliReadiness {
            cli,
            installed,
            binary_path: None,
            version: None,
            login: "unknown",
            ready: installed,
        }
    }

    #[test]
    fn effective_default_is_the_cli_the_user_actually_has() {
        // Codex-only machine: defaulting to claude would exec a binary that
        // isn't there (the bug this function exists to prevent).
        let codex_only = [agent("claude", false), agent("codex", true)];
        assert_eq!(effective_default(None, &codex_only), "codex");

        // Both installed: claude wins the tie.
        let both = [agent("claude", true), agent("codex", true)];
        assert_eq!(effective_default(None, &both), "claude");

        // Explicit setting beats everything, even an uninstalled-looking one
        // (the user may have a custom path the probe can't see).
        assert_eq!(effective_default(Some("codex"), &both), "codex");
        assert_eq!(effective_default(Some("claude"), &codex_only), "claude");

        // Nothing installed: claude, so the error guidance names the primary.
        let none = [agent("claude", false), agent("codex", false)];
        assert_eq!(effective_default(None, &none), "claude");
    }

    #[tokio::test]
    async fn not_installed_is_never_ready_and_never_probes() {
        let r = detect("claude", Some("/nonexistent/claude"), Some(true), true).await;
        assert!(!r.installed);
        assert!(!r.ready);
        assert_eq!(r.login, "unknown");
        assert!(r.version.is_none());
    }
}

/// The CLI a run uses when neither the engine nor the user named one
/// (David, 2026-07-17): an explicit setting wins; otherwise the CLI the
/// user actually HAS — claude only wins the tie when both are installed.
/// A codex-only machine must never default to a claude binary that isn't
/// there. Falls back to "claude" when nothing is installed, purely so the
/// resulting binary_not_found error carries the primary CLI's guidance.
pub fn effective_default(explicit: Option<&str>, agents: &[CliReadiness]) -> &'static str {
    match explicit {
        Some("codex") => return "codex",
        Some("claude") => return "claude",
        _ => {}
    }
    let installed = |cli: &str| agents.iter().any(|a| a.cli == cli && a.installed);
    if installed("claude") {
        "claude"
    } else if installed("codex") {
        "codex"
    } else {
        "claude"
    }
}
