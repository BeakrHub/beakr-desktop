use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "settings.json";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    pub scoped_folders: Vec<String>,
    pub device_name: Option<String>,
    pub auto_connect: bool,
    /// User's own Anthropic API key for local Claude Code runs (ENG-1528,
    /// DESIGN.md decision 5). Stored locally in the settings store like the
    /// device token; never synced to Beakr cloud and never returned to the
    /// webview (write-only — see `get_coding_agent_settings`).
    pub anthropic_api_key: Option<String>,
    /// Optional explicit path to the `claude` binary (Settings override for
    /// the login-shell/well-known-path resolution).
    pub claude_binary_path: Option<String>,
    /// Which CLI a run uses when the engine doesn't request one explicitly
    /// ("claude" | "codex"). None = claude (the pre-picker behavior).
    pub default_cli: Option<String>,
    /// Outcome of the most recent real run's auth, per CLI (ENG-1536).
    /// `true` after a successful run, `false` after an auth_failed one. This
    /// is the strongest login signal we have that costs nothing: readiness
    /// NEVER probes the CLI (a probe against a logged-in CLI is a real,
    /// quota-burning API call — David 2026-07-17). Self-healing: if a stale
    /// `true` lets a run through after a logout, that run fails typed as
    /// auth_failed and flips this to `false`.
    pub claude_auth_ok: Option<bool>,
    pub codex_auth_ok: Option<bool>,
}

pub fn load_settings(app: &AppHandle) -> Settings {
    let store = match app.store(STORE_FILE) {
        Ok(s) => s,
        Err(_) => return Settings::default(),
    };

    let scoped_folders: Vec<String> = store
        .get("scoped_folders")
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    let device_name: Option<String> = store
        .get("device_name")
        .and_then(|v| serde_json::from_value(v).ok());

    let auto_connect: bool = store
        .get("auto_connect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let anthropic_api_key: Option<String> = store
        .get("anthropic_api_key")
        .and_then(|v| serde_json::from_value(v).ok());

    let claude_binary_path: Option<String> = store
        .get("claude_binary_path")
        .and_then(|v| serde_json::from_value(v).ok());

    let default_cli: Option<String> = store
        .get("default_cli")
        .and_then(|v| serde_json::from_value(v).ok());

    let claude_auth_ok: Option<bool> = store.get("claude_auth_ok").and_then(|v| v.as_bool());
    let codex_auth_ok: Option<bool> = store.get("codex_auth_ok").and_then(|v| v.as_bool());

    Settings {
        scoped_folders,
        device_name,
        auto_connect,
        anthropic_api_key,
        claude_binary_path,
        default_cli,
        claude_auth_ok,
        codex_auth_ok,
    }
}

/// Record the auth outcome of a real run (success -> true, auth_failed ->
/// false) as the zero-cost login signal for readiness (ENG-1536).
pub fn record_cli_auth(app: &AppHandle, cli: &str, ok: bool) {
    let store = match app.store(STORE_FILE) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to open store to record cli auth: {e}");
            return;
        }
    };
    let key = match cli {
        "claude" => "claude_auth_ok",
        "codex" => "codex_auth_ok",
        _ => return,
    };
    let _ = store.set(key, serde_json::Value::Bool(ok));
}

pub fn save_settings(app: &AppHandle, settings: &Settings) {
    let store = match app.store(STORE_FILE) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to open store: {e}");
            return;
        }
    };

    let _ = store.set(
        "scoped_folders",
        serde_json::to_value(&settings.scoped_folders).unwrap_or_default(),
    );

    if let Some(ref name) = settings.device_name {
        let _ = store.set(
            "device_name",
            serde_json::to_value(name).unwrap_or_default(),
        );
    }

    let _ = store.set(
        "auto_connect",
        serde_json::Value::Bool(settings.auto_connect),
    );

    if let Some(ref key) = settings.anthropic_api_key {
        store.set("anthropic_api_key", serde_json::to_value(key).unwrap_or_default());
    }
    if let Some(ref path) = settings.claude_binary_path {
        store.set(
            "claude_binary_path",
            serde_json::to_value(path).unwrap_or_default(),
        );
    }
    if let Some(ref cli) = settings.default_cli {
        store.set("default_cli", serde_json::to_value(cli).unwrap_or_default());
    }
}
