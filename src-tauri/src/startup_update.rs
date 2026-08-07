use tauri_plugin_updater::UpdaterExt;

/// Check the signed updater feed independently of the settings window.
///
/// This is intentionally check-only: downloading, installing, and relaunching
/// remain explicit user actions in the settings UI so a background agent is
/// never interrupted mid-operation.
pub fn spawn(app_handle: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        log::info!("Checking for updates on startup");

        let updater = match app_handle.updater() {
            Ok(updater) => updater,
            Err(error) => {
                log::warn!("Could not initialize startup update check: {error}");
                return;
            }
        };

        match updater.check().await {
            Ok(Some(update)) => {
                log::info!("Update {} is available", update.version);

                // A check-only result is useless to a Windows user whose
                // settings window never mounted. Open the existing update UI
                // so the user can explicitly approve download and relaunch.
                #[cfg(target_os = "windows")]
                {
                    let window_app = app_handle.clone();
                    if let Err(error) = app_handle.run_on_main_thread(move || {
                        crate::tray::show_settings_window(&window_app);
                    }) {
                        log::warn!("Could not open settings for available update: {error}");
                    }
                }
            }
            Ok(None) => {
                log::info!("Startup update check found no newer version");
            }
            Err(error) => {
                // Match the old silent React check: an offline machine or a
                // transient release-feed error must not nag or stop the agent.
                log::warn!("Silent startup update check failed: {error}");
            }
        }
    });
}
