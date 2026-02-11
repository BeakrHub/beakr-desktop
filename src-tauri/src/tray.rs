use tauri::{
    menu::{MenuBuilder, MenuItem, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

use crate::state::ConnectionStatus;

const WINDOW_LABEL: &str = "settings";
const WINDOW_TITLE: &str = "Beakr Desktop";

/// Holds the tray status menu item so we can update its text later.
pub struct TrayState {
    pub status_item: MenuItem<tauri::Wry>,
}

/// Build the system tray icon and menu.
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let status_item = MenuItemBuilder::with_id("status", "Status: Disconnected")
        .enabled(false)
        .build(app)?;

    // Store the menu item handle so we can update it when connection status changes
    app.manage(TrayState {
        status_item: status_item.clone(),
    });

    let settings_item = MenuItemBuilder::with_id("settings", "Settings…")
        .build(app)?;

    let quit_item = MenuItemBuilder::with_id("quit", "Quit Beakr")
        .build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status_item)
        .separator()
        .item(&settings_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().unwrap())
        .menu(&menu)
        .tooltip("Beakr Desktop")
        .on_menu_event(move |app, event| {
            match event.id().as_ref() {
                "settings" => {
                    show_settings_window(app);
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .build(app)?;

    Ok(())
}

/// Create or focus the settings webview window.
pub fn show_settings_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
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
