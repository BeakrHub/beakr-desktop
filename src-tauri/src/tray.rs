use tauri::{
    menu::{MenuBuilder, MenuItem, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

use crate::state::{ActiveCodingRun, CodingRunStatus, ConnectionStatus};

const WINDOW_LABEL: &str = "settings";
const WINDOW_TITLE: &str = "Beakr Desktop";

/// Holds the tray menu items whose text we update at runtime.
pub struct TrayState {
    pub status_item: MenuItem<tauri::Wry>,
    /// "Stop coding run" — enabled only while a local coding run is active
    /// (ENG-1528). Clicking cancels the active run via the inflight registry,
    /// which SIGINTs the CLI's process group.
    pub stop_run_item: MenuItem<tauri::Wry>,
    /// Opens the app/pairing window. Its label is state-aware: "Pair device"
    /// when no device is paired (it lands on the pairing screen) and "Open Beakr"
    /// once paired (the window is the device's status/folders/activity view, not
    /// just preferences).
    pub settings_item: MenuItem<tauri::Wry>,
}

/// Build the system tray icon and menu.
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let status_item = MenuItemBuilder::with_id("status", "Status: Disconnected")
        .enabled(false)
        .build(app)?;

    // Default to the unpaired label; lib.rs updates it from the stored token at
    // startup, and claim_pairing_code/clear_token update it as pairing changes.
    let settings_item = MenuItemBuilder::with_id("settings", "Pair device").build(app)?;

    let stop_run_item = MenuItemBuilder::with_id("stop_run", "Stop coding run")
        .enabled(false)
        .build(app)?;

    // Store the menu item handles so we can update them at runtime.
    app.manage(TrayState {
        status_item: status_item.clone(),
        stop_run_item: stop_run_item.clone(),
        settings_item: settings_item.clone(),
    });

    let quit_item = MenuItemBuilder::with_id("quit", "Quit Beakr").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status_item)
        .separator()
        .item(&settings_item)
        .item(&stop_run_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().unwrap())
        .menu(&menu)
        .tooltip("Beakr Desktop")
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "settings" => {
                show_settings_window(app);
            }
            "stop_run" => {
                if let Some(state) = app.try_state::<crate::state::AppState>() {
                    if crate::state::stop_active_coding_run(&state).is_none() {
                        log::debug!("Tray stop clicked with no active run");
                    }
                }
            }
            "quit" => {
                // Reap any live coding-run process groups before exiting so
                // CLI children aren't orphaned to PID 1 (ENG-1527).
                if let Some(state) = app.try_state::<crate::state::AppState>() {
                    state.processes.kill_all();
                }
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// Create or focus the settings webview window.
pub fn show_settings_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        // unminimize first: show() doesn't deminiaturize a Dock-minimized
        // window and set_focus() is a no-op while minimized (tao),
        // so without this a Dock/tray click appears to do nothing.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    // Create window — hide on close instead of destroying
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title(WINDOW_TITLE)
    .inner_size(480.0, 640.0)
    .resizable(true)
    .center();

    match builder.build() {
        Ok(window) => {
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window_clone.hide();
                }
            });
        }
        Err(e) => {
            log::error!("Failed to create settings window: {e}");
        }
    }
}

/// Update the tray menu status text.
pub fn update_tray_status(app: &AppHandle, status: &ConnectionStatus) {
    let text = format!("Status: {status}");
    if let Some(tray_state) = app.try_state::<TrayState>() {
        let _ = tray_state.status_item.set_text(&text);
    }
}

/// Update the menu item label to reflect whether a device is paired.
/// Unpaired -> "Pair device" (the window opens on the pairing screen); paired ->
/// "Open Beakr". The click action is unchanged; only the label adapts.
pub fn update_tray_pairing(app: &AppHandle, is_paired: bool) {
    let label = if is_paired {
        "Open Beakr"
    } else {
        "Pair device"
    };
    if let Some(tray_state) = app.try_state::<TrayState>() {
        let _ = tray_state.settings_item.set_text(label);
    }
}

/// The stop item's (label, enabled) for a run state. Pure so it's testable:
/// the label says WHAT would be stopped (folder basename), and the item is
/// disabled while Stopping — the cancel is already signalled, a second click
/// can do nothing, and the label must not pretend the run is gone before the
/// child is confirmed dead (ENG-1552 run visibility).
fn stop_item_appearance(run: Option<&ActiveCodingRun>) -> (String, bool) {
    match run {
        None => ("Stop coding run".to_string(), false),
        Some(run) => match run.status {
            CodingRunStatus::Running => {
                let folder = std::path::Path::new(&run.working_dir)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| run.working_dir.clone());
                (format!("Stop coding run in {folder}"), true)
            }
            CodingRunStatus::Stopping => ("Stopping coding run…".to_string(), false),
        },
    }
}

/// Reflect the active coding run on the tray as runs start/stop/finish
/// (ENG-1528, enriched for ENG-1552 run visibility).
pub fn update_tray_coding_run(app: &AppHandle, run: Option<&ActiveCodingRun>) {
    let (label, enabled) = stop_item_appearance(run);
    if let Some(tray_state) = app.try_state::<TrayState>() {
        let _ = tray_state.stop_run_item.set_text(&label);
        let _ = tray_state.stop_run_item.set_enabled(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: CodingRunStatus) -> ActiveCodingRun {
        ActiveCodingRun {
            request_id: "req-1".into(),
            working_dir: "/Users/d/wandb-sandbox".into(),
            cli: "claude".into(),
            started_at_ms: 0,
            status,
        }
    }

    #[test]
    fn idle_item_is_generic_and_disabled() {
        assert_eq!(
            stop_item_appearance(None),
            ("Stop coding run".to_string(), false)
        );
    }

    #[test]
    fn running_item_names_the_folder_and_enables() {
        assert_eq!(
            stop_item_appearance(Some(&run(CodingRunStatus::Running))),
            ("Stop coding run in wandb-sandbox".to_string(), true)
        );
    }

    #[test]
    fn stopping_item_says_stopping_and_disables() {
        // The child was SIGINTed but is not confirmed dead — a second click is
        // useless and the label must not claim the run already stopped.
        assert_eq!(
            stop_item_appearance(Some(&run(CodingRunStatus::Stopping))),
            ("Stopping coding run…".to_string(), false)
        );
    }
}
