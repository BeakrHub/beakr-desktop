mod commands;
mod config;
mod diagnostics;
mod file_index;
mod file_watch;
mod process_group;
mod search_filter;
mod security;
mod session;
mod startup_update;
mod state;
mod tools;
mod tray;
pub mod unicode;
mod ws;

use state::AppState;
use tauri_plugin_autostart::MacosLauncher;

/// Windows-only startup gate for the race before Tauri's single-instance
/// plugin creates its hidden WM_COPYDATA target window.
///
/// The upstream plugin sees its mutex, calls `FindWindowW`, and only exits the
/// duplicate when that window already exists. Four cold launches can all see
/// the primary's mutex during the gap, miss the window, and continue as four
/// full applications. This earlier mutex makes contenders wait out that gap;
/// once the target exists they continue into the plugin's normal focus/handoff
/// path and exit there.
#[cfg(target_os = "windows")]
struct EarlySingleInstanceGuard(isize);

#[cfg(target_os = "windows")]
impl Drop for EarlySingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0 as _);
            windows_sys::Win32::Foundation::CloseHandle(self.0 as _);
        }
    }
}

#[cfg(target_os = "windows")]
fn early_single_instance_gate() -> Option<EarlySingleInstanceGuard> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, WAIT_ABANDONED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let gate_name = wide(r"Local\com.thebeakr.desktop-early-single-instance");
    let handle = unsafe { CreateMutexW(std::ptr::null(), true.into(), gate_name.as_ptr()) };
    if handle.is_null() {
        log::error!("Could not create the early single-instance mutex");
        return None;
    }

    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Some(EarlySingleInstanceGuard(handle as isize));
    }

    // A WebView child can outlive a force-terminated desktop process while
    // retaining a handle to the named mutex object. Object existence alone
    // therefore does not prove that another desktop instance is alive. If the
    // mutex is unowned or abandoned, this launch acquires it and becomes the
    // new primary.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => {
            return Some(EarlySingleInstanceGuard(handle as isize));
        }
        WAIT_TIMEOUT => {}
        result => {
            log::error!("Could not inspect the early single-instance mutex: {result}");
            unsafe { CloseHandle(handle) };
            return None;
        }
    }

    unsafe { CloseHandle(handle) };
    let class_name = wide("com.thebeakr.desktop-sic");
    let window_name = wide("com.thebeakr.desktop-siw");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let target = unsafe { FindWindowW(class_name.as_ptr(), window_name.as_ptr()) };
        if !target.is_null() {
            // Continue into Tauri. The normal single-instance plugin now has a
            // guaranteed handoff target and will focus the primary then exit.
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // A process owns the early mutex but never became handoff-ready. Starting
    // another full tray/WS client would be worse than dropping this launch.
    std::process::exit(0);
}

/// Returns the WebSocket URL based on build configuration.
/// Priority: BEAKR_WS_URL env var (compile-time) > debug=localhost > release=production
pub fn ws_url() -> String {
    if let Some(url) = option_env!("BEAKR_WS_URL") {
        return url.to_string();
    }
    if cfg!(debug_assertions) {
        "ws://localhost:8000/v1/desktop-agent/ws".to_string()
    } else {
        "wss://api.thebeakr.com/v1/desktop-agent/ws".to_string()
    }
}

/// Passed to the app by the launch-at-login entry the autostart plugin writes.
/// Its only job is to mark a launch as "the OS did this at login", not "a person
/// opened the app".
const AUTOSTART_ARG: &str = "--autostart";

/// Whether the settings window should open on this launch.
///
/// Two separate reasons to show it:
///   * not paired yet — the user has nothing else to act on, so the pairing
///     screen must be reachable without hunting for the tray icon;
///   * the user launched the app themselves — double-clicking an app and having
///     nothing appear reads as "it didn't start", which is the complaint that
///     started this whole investigation.
///
/// The one case that stays silent is a paired launch at login, so logging in
/// doesn't throw a window in the user's face.
fn should_open_window_on_launch(has_stored_token: bool, launched_by_autostart: bool) -> bool {
    !has_stored_token || !launched_by_autostart
}

fn launched_by_autostart() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_ARG)
}

fn autostart_entry_has_marker(registered_entry: &str) -> bool {
    registered_entry
        .split(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>')
        })
        .any(|token| token == AUTOSTART_ARG)
}

fn windows_autostart_command_is_safely_quoted(
    registered_command: &str,
    current_exe: &std::path::Path,
) -> bool {
    let Some(after_opening_quote) = registered_command.trim_start().strip_prefix('"') else {
        return false;
    };
    let Some((registered_exe, _remaining_args)) = after_opening_quote.split_once('"') else {
        return false;
    };

    registered_exe.eq_ignore_ascii_case(&current_exe.to_string_lossy())
}

/// Whether a launch-at-login command line refers to some binary other than the
/// one currently running. Compared case-insensitively because Windows paths are.
fn autostart_command_is_stale(registered_command: &str, current_exe: &std::path::Path) -> bool {
    let current = current_exe.to_string_lossy().to_lowercase();
    !registered_command.to_lowercase().contains(current.trim())
}

#[derive(Debug, PartialEq, Eq)]
struct AutostartStartupPlan {
    enable_default: bool,
    mark_default_applied: bool,
    rewrite_existing_entry: bool,
}

fn autostart_startup_plan(
    default_applied: bool,
    entry_enabled: bool,
    entry_is_stale: bool,
    entry_missing_marker: bool,
) -> AutostartStartupPlan {
    AutostartStartupPlan {
        enable_default: !default_applied && !entry_enabled,
        mark_default_applied: !default_applied,
        // The entry must already be enabled. In particular, a missing entry or
        // one disabled in Windows Startup Apps must never be resurrected by a
        // migration or repair.
        rewrite_existing_entry: entry_enabled && (entry_is_stale || entry_missing_marker),
    }
}

/// Read the platform launch-at-login entry. Any failure is treated as unknown,
/// so a registry/filesystem hiccup never causes a pointless rewrite.
#[cfg(target_os = "windows")]
fn registered_autostart_entry(app_name: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    unsafe {
        let mut key = std::ptr::null_mut();
        let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return None;
        }

        let name = wide(app_name);
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let rc = RegGetValueW(
            key,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        );
        RegCloseKey(key);
        if rc != 0 {
            return None;
        }

        let chars = (len as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..chars]))
    }
}

#[cfg(target_os = "macos")]
fn registered_autostart_entry(app_name: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    std::fs::read_to_string(
        std::path::PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{app_name}.plist")),
    )
    .ok()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn registered_autostart_entry(_app_name: &str) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn write_safely_quoted_windows_autostart_entry(app_name: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    let exe = std::env::current_exe()
        .map_err(|error| format!("Failed to resolve the current executable: {error}"))?;
    let command = format!("\"{}\" {AUTOSTART_ARG}", exe.display());
    let subkey = wide(std::ffi::OsStr::new(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
    ));
    let name = wide(std::ffi::OsStr::new(app_name));
    let value = wide(std::ffi::OsStr::new(&command));

    unsafe {
        let mut key = std::ptr::null_mut();
        let open_result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        );
        if open_result != 0 {
            return Err(format!("Failed to open the Windows Run key: {open_result}"));
        }

        let write_result = RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * std::mem::size_of::<u16>()) as u32,
        );
        RegCloseKey(key);
        if write_result != 0 {
            return Err(format!(
                "Failed to write the safely quoted Windows Run entry: {write_result}"
            ));
        }
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_synthetic_windows_startup_approval(app_name: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    let subkey = wide(std::ffi::OsStr::new(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
    ));
    let name = wide(std::ffi::OsStr::new(app_name));

    unsafe {
        let mut key = std::ptr::null_mut();
        let open_result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        );
        if open_result == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if open_result != 0 {
            return Err(format!(
                "Failed to open the Windows StartupApproved key: {open_result}"
            ));
        }

        let delete_result = RegDeleteValueW(key, name.as_ptr());
        RegCloseKey(key);
        if delete_result != 0 && delete_result != ERROR_FILE_NOT_FOUND {
            return Err(format!(
                "Failed to remove the synthetic Windows startup approval: {delete_result}"
            ));
        }
    }

    Ok(())
}

pub(crate) fn enable_autostart(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    app.autolaunch()
        .enable()
        .map_err(|error| format!("Failed to enable autostart: {error}"))?;

    // auto-launch 0.5.0 writes the Windows executable path without quotes and
    // fabricates a StartupApproved value. The former is hijackable; the latter
    // is filtered out by Shell-Core on the affected Windows 11 build even
    // though Settings displays it as enabled. Absence means enabled to both
    // Windows and auto-launch. This function is called only for an explicit
    // enable or an entry already known to be enabled, so a real user-disabled
    // StartupApproved value is never removed here.
    #[cfg(target_os = "windows")]
    {
        write_safely_quoted_windows_autostart_entry(&app.package_info().name)?;
        remove_synthetic_windows_startup_approval(&app.package_info().name)?;
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn record_early_startup_probe() {
    use std::io::Write;

    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let log_dir = std::path::PathBuf::from(local_app_data)
        .join("com.thebeakr.desktop")
        .join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let path = log_dir.join("startup-probe.log");
    let truncate = std::fs::metadata(&path)
        .map(|metadata| metadata.len() > 64 * 1024)
        .unwrap_or(false);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate)
        .open(path);
    let Ok(mut file) = file else {
        return;
    };
    let _ = writeln!(
        file,
        "{} pid={} autostart_marker={}",
        chrono::Local::now().to_rfc3339(),
        std::process::id(),
        launched_by_autostart()
    );
}

#[cfg(not(target_os = "macos"))]
fn should_prevent_exit(exit_code: Option<i32>) -> bool {
    // A window-count/user exit has no code. Explicit app.exit()/restart() calls
    // carry a code and must remain able to terminate the tray application.
    exit_code.is_none()
}

/// Bring the already-running instance forward when a duplicate launch is blocked.
///
/// On Windows this must NOT create the window synchronously. The single-instance
/// callback runs on the primary's main thread while the second process is still
/// inside its WM_COPYDATA handoff, and `WebviewWindowBuilder::build()` pumps the
/// event loop -- creating a window from inside a message handler deadlocks the
/// primary. Because the primary is then frozen it never acknowledges the
/// handoff, so the duplicates never exit either: one hung app plus a live
/// process and tray icon per launch. That is the "several Beakr icons" symptom.
///
/// It only bites when there is no window yet, which is precisely the paired
/// launch-at-login state, and it needs two launches close enough together that
/// the first has not finished creating the window -- a double-clicked shortcut,
/// or a shortcut landing at the same moment as autostart.
///
/// Deferring to the main-thread queue lets the handoff return first; the closure
/// then runs on the next turn of the event loop, with no reentrancy.
fn focus_existing_instance(app: &tauri::AppHandle) {
    #[cfg(target_os = "windows")]
    {
        // run_on_main_thread ALONE is not enough here. This callback already
        // runs on the main thread, and a main-thread caller can have its
        // closure executed inline -- which is still inside the handoff, so the
        // reentrancy remains. Measured: it took the burst failure rate from
        // 5/5 to 1/5 rather than to 0/5.
        //
        // Bouncing through the async runtime first guarantees the handoff has
        // returned before any window work starts; only then do we come back to
        // the main thread, which is where window APIs must be called.
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let for_main = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                tray::show_settings_window(&for_main);
            });
        });
    }

    #[cfg(not(target_os = "windows"))]
    tray::show_settings_window(app);
}

fn spawn_benchling_liveness(app_handle: tauri::AppHandle, state: AppState) {
    tauri::async_runtime::spawn(session::benchling::watch_session_liveness(
        app_handle, state,
    ));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "windows")]
    record_early_startup_probe();

    #[cfg(target_os = "windows")]
    let _early_single_instance_guard = early_single_instance_gate();

    let app_state = AppState::new();

    tauri::Builder::default()
        // Must be registered first so a duplicate process exits before other
        // plugins initialize. Reuse the tray/Dock window recovery path so the
        // surviving instance is shown, unminimized, and focused.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            focus_existing_instance(app);
        }))
        // The default targets write both to stdout and to the platform log
        // directory (on Windows: %LOCALAPPDATA%\com.thebeakr.desktop\logs).
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        // The marker argument is what lets setup() tell a login launch from the
        // user deliberately opening the app. Without it the two are
        // indistinguishable and a paired user who double-clicks gets no window.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(app_state.clone())
        .setup(move |app| {
            diagnostics::install_panic_hook();
            diagnostics::spawn_log_flusher();
            diagnostics::trigger_test_panic_if_configured();
            log::info!(
                "Beakr Desktop startup diagnostics initialized (process_id={})",
                std::process::id()
            );
            log::logger().flush();

            // ENG-1953: Windows only. Fail fast if WebView2 is missing, before
            // the updater, the WebSocket client or the Benchling restore start.
            // Any of those can reach a webview, and a webview call without a
            // runtime panics on a worker thread — which kills the task and
            // leaves the process alive and headless. Nothing below this line
            // may run when the runtime is unusable.
            #[cfg(target_os = "windows")]
            tray::ensure_webview_runtime_or_exit(app.handle());

            // ENG-1377: keep the default Regular activation policy so the app has
            // a Dock icon users can click to open it. Do not set
            // ActivationPolicy::Accessory — tray-only proved too hidden (notched
            // menu bars can swallow the tray icon). Dock click → RunEvent::Reopen
            // (handled in run()) → settings window.

            // Load persisted settings
            let settings = config::load_settings(app.handle());

            // Check the updater feed from the application lifecycle rather
            // than from React. Paired release builds deliberately launch with
            // no window, so a component-mounted check is not reachable there.
            startup_update::spawn(app.handle().clone());

            let has_stored_token = {
                use tauri_plugin_store::StoreExt;
                app.handle()
                    .store("settings.json")
                    .ok()
                    .and_then(|store| store.get("device_token"))
                    .and_then(|v| v.as_str().map(|s| !s.is_empty()))
                    .unwrap_or(false)
            };

            {
                let state = app_state.clone();
                let settings_folders = settings.scoped_folders.clone();
                let settings_name = settings.device_name.clone();
                tauri::async_runtime::spawn(async move {
                    *state.scoped_folders.write().await = settings_folders;
                    if let Some(name) = settings_name {
                        *state.device_name.write().await = name;
                    }
                    // Start file-index maintenance now that the scoped folders
                    // are loaded (watcher + periodic rescan fallback).
                    file_watch::spawn(state.clone());
                });
            }

            // Enable launch-at-login on the first run only. A missing OS entry
            // after that is an explicit user preference, not another first run.
            {
                use tauri_plugin_autostart::ManagerExt;
                let autostart = app.autolaunch();
                let entry_enabled = autostart.is_enabled().unwrap_or(false);
                let registered_entry = if entry_enabled {
                    registered_autostart_entry(&app.package_info().name)
                } else {
                    None
                };
                #[cfg(target_os = "windows")]
                let entry_is_stale = registered_entry
                    .as_deref()
                    .and_then(|registered| {
                        std::env::current_exe()
                            .ok()
                            .map(|exe| autostart_command_is_stale(registered, &exe))
                    })
                    .unwrap_or(false);
                #[cfg(not(target_os = "windows"))]
                let entry_is_stale = false;
                let entry_missing_marker = registered_entry
                    .as_deref()
                    .map(|registered| !autostart_entry_has_marker(registered))
                    .unwrap_or(false);
                #[cfg(target_os = "windows")]
                let entry_needs_safe_quote = registered_entry
                    .as_deref()
                    .and_then(|registered| {
                        std::env::current_exe().ok().map(|exe| {
                            !windows_autostart_command_is_safely_quoted(registered, &exe)
                        })
                    })
                    .unwrap_or(false);
                #[cfg(not(target_os = "windows"))]
                let entry_needs_safe_quote = false;
                let default_applied = config::autostart_default_applied(app.handle());
                let plan = autostart_startup_plan(
                    default_applied,
                    entry_enabled,
                    entry_is_stale,
                    entry_missing_marker || entry_needs_safe_quote,
                );

                let default_succeeded = if plan.enable_default {
                    match enable_autostart(app.handle()) {
                        Ok(()) => {
                            log::info!("Autostart enabled by first-run default");
                            true
                        }
                        Err(error) => {
                            log::warn!("Failed to apply first-run autostart default: {error}");
                            false
                        }
                    }
                } else {
                    entry_enabled
                };

                if plan.mark_default_applied && default_succeeded {
                    if let Err(error) = config::mark_autostart_default_applied(app.handle()) {
                        log::warn!("{error}");
                    }
                }

                // is_enabled() only reports that an entry EXISTS -- not that
                // it points at this binary. Preserve stale-install repair even
                // after the one-time default has been recorded.
                if plan.rewrite_existing_entry {
                    log::info!(
                        "Rewriting enabled autostart entry (stale={entry_is_stale}, missing_marker={entry_missing_marker}, unsafe_windows_command={entry_needs_safe_quote})"
                    );
                    if let Err(error) = enable_autostart(app.handle()) {
                        log::warn!("Failed to rewrite autostart entry: {error}");
                    }
                }
            }

            // Set up system tray, then set the pairing-aware menu label from the
            // stored token (claim_pairing_code / clear_token keep it in sync after).
            tray::setup_tray(app.handle())?;
            tray::update_tray_pairing(app.handle(), has_stored_token);

            // Open the window unless this is a paired launch at login. See
            // should_open_window_on_launch. On macOS this also covers the
            // Finder/Spotlight first-launch case, where Reopen never fires.
            if should_open_window_on_launch(has_stored_token, launched_by_autostart()) {
                tray::show_settings_window(app.handle());
            }

            // In dev builds, always auto-open the window on launch so testing
            // doesn't depend on clicking the Dock or tray icon.
            #[cfg(debug_assertions)]
            tray::show_settings_window(app.handle());

            // Auto-connect if we have a stored device token
            // (In dev mode without a token, the frontend will handle connection)
            if has_stored_token {
                log::info!("Found stored device token, auto-connecting on startup");
                let app_handle = app.handle().clone();
                let state_clone = app_state.clone();

                tauri::async_runtime::spawn(async move {
                    // Load token from store and set in state
                    use tauri_plugin_store::StoreExt;
                    if let Ok(store) = app_handle.store("settings.json") {
                        if let Some(token) = store
                            .get("device_token")
                            .and_then(|v| serde_json::from_value::<String>(v).ok())
                        {
                            *state_clone.auth_token.write().await = Some(token);
                        }
                    }

                    {
                        let ws_app = app_handle.clone();
                        let ws_state = state_clone.clone();

                        // Claim the connection slot BEFORE the delay below.
                        //
                        // The settings window can mount during those 500ms and
                        // call connect_ws. That command refuses to start a
                        // second client only when the status already says a
                        // connection is under way -- and until WsClient::run
                        // actually starts, the status is still Disconnected. So
                        // the window would slip through and one process would
                        // run two clients, registering this device twice.
                        //
                        // Not platform-gated: the same race exists on macOS,
                        // where the window opens on first launch and on every
                        // Dock click.
                        *ws_state.ws_status.write().await = state::ConnectionStatus::Connecting;

                        tauri::async_runtime::spawn(async move {
                            // Brief delay to let state initialization complete
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                            let ws_url = ws_url();
                            let client = ws::WsClient::new(ws_app, ws_state, ws_url);
                            client.run().await;
                        });
                    }

                    let restored = session::benchling::restore_session_on_startup(
                        app_handle.clone(),
                        state_clone.clone(),
                    )
                    .await;
                    if restored {
                        log::info!("Benchling startup session restore succeeded");
                    }

                    spawn_benchling_liveness(app_handle, state_clone);
                });
            } else {
                // With no stored token there is nothing to restore yet, but keep
                // the liveness watcher alive so a later pairing/login flow is
                // monitored without needing an app restart.
                spawn_benchling_liveness(app.handle().clone(), app_state.clone());

                if cfg!(debug_assertions) {
                    log::info!("Dev mode: auto-connecting WebSocket client");
                    let app_handle = app.handle().clone();
                    let state_clone = app_state.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let ws_url = ws_url();
                        let client = ws::WsClient::new(app_handle, state_clone, ws_url);
                        client.run().await;
                    });
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::set_auth_token,
            commands::connect_ws,
            commands::disconnect_ws,
            commands::get_connection_status,
            commands::get_scoped_folders,
            commands::set_scoped_folders,
            commands::get_device_name,
            commands::set_device_name,
            commands::get_autostart,
            commands::set_autostart,
            commands::claim_pairing_code,
            commands::get_stored_token,
            commands::clear_token,
            commands::get_ws_url,
            commands::get_coding_agent_settings,
            commands::set_coding_agent_settings,
            diagnostics::open_log_folder,
            commands::get_active_coding_run,
            commands::stop_coding_run,
            commands::open_run_terminal,
            commands::get_coding_agent_readiness,
            session::commands::connect_session,
            session::commands::benchling_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, _event| {
            // Windows/Linux tray applications must stay resident after their last
            // window closes. Keep explicit app.exit()/restart() working, and do
            // not change macOS's normal Cmd-Q / application-menu quit behaviour.
            #[cfg(not(target_os = "macos"))]
            if let tauri::RunEvent::ExitRequested { code, api, .. } = _event {
                if should_prevent_exit(code) {
                    api.prevent_exit();
                }
            }

            // macOS fires Reopen when the Dock icon is clicked (and on
            // Finder/Spotlight re-launch of a running app). Only open the
            // settings window when nothing is visible — a Dock click while
            // e.g. the Benchling session window is up must not cover it.
            // A minimized-only app reports has_visible_windows == false.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = _event
            {
                if !has_visible_windows {
                    tray::show_settings_window(_app_handle);
                }
            }
        });
}

#[cfg(test)]
mod launch_tests {
    use super::{
        autostart_command_is_stale, autostart_entry_has_marker, autostart_startup_plan,
        should_open_window_on_launch, windows_autostart_command_is_safely_quoted,
    };
    use std::path::Path;

    #[test]
    fn a_person_opening_the_app_always_gets_a_window() {
        // The complaint that started ENG-206: double-click, nothing appears.
        assert!(should_open_window_on_launch(true, false));
        assert!(should_open_window_on_launch(false, false));
    }

    #[test]
    fn a_paired_login_launch_stays_silent() {
        // Logging in must not throw a window in the user's face.
        assert!(!should_open_window_on_launch(true, true));
    }

    #[test]
    fn an_unpaired_login_launch_still_shows_the_pairing_screen() {
        // Nothing to act on otherwise, and the tray icon starts hidden in the
        // Windows 11 overflow, so staying silent would strand the user.
        assert!(should_open_window_on_launch(false, true));
    }

    #[test]
    fn autostart_entry_naming_this_binary_is_not_stale() {
        let exe = Path::new(r"C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe");
        assert!(!autostart_command_is_stale(
            r#""C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe" --autostart"#,
            exe
        ));
    }

    #[test]
    fn autostart_entry_is_case_insensitive_like_windows_paths() {
        let exe = Path::new(r"C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe");
        assert!(!autostart_command_is_stale(
            r#""c:\users\me\appdata\local\beakr desktop\BEAKR-DESKTOP.EXE" --autostart"#,
            exe
        ));
    }

    #[test]
    fn autostart_entry_left_by_a_previous_install_location_is_stale() {
        // Exactly what a reinstall to a different path leaves behind: the entry
        // survives, points at a binary that may be gone, and login silently
        // starts nothing.
        let exe = Path::new(r"C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe");
        assert!(autostart_command_is_stale(
            r#""C:\Dev\Beakr\beakr-desktop\src-tauri\target\release\beakr-desktop.exe""#,
            exe
        ));
    }

    #[test]
    fn windows_autostart_command_quotes_the_executable_as_the_first_argument() {
        let exe = Path::new(r"C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe");
        assert!(windows_autostart_command_is_safely_quoted(
            r#""C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe" --autostart"#,
            exe
        ));
        assert!(!windows_autostart_command_is_safely_quoted(
            r#"C:\Users\me\AppData\Local\Beakr Desktop\beakr-desktop.exe --autostart"#,
            exe
        ));
    }

    #[test]
    fn autostart_marker_is_detected_in_windows_and_macos_entries() {
        assert!(autostart_entry_has_marker(
            r#""C:\Program Files\Beakr\beakr-desktop.exe" --autostart"#
        ));
        assert!(autostart_entry_has_marker(
            "<array><string>/Applications/Beakr Desktop.app/Contents/MacOS/beakr-desktop</string><string>--autostart</string></array>"
        ));
        assert!(!autostart_entry_has_marker(
            r#""C:\Program Files\Beakr\beakr-desktop.exe""#
        ));
    }

    #[test]
    fn first_run_applies_the_autostart_default_once() {
        let plan = autostart_startup_plan(false, false, false, false);

        assert!(plan.enable_default);
        assert!(plan.mark_default_applied);
        assert!(!plan.rewrite_existing_entry);
    }

    #[test]
    fn an_absent_entry_stays_absent_after_the_default_was_applied() {
        let plan = autostart_startup_plan(true, false, false, true);

        assert!(!plan.enable_default);
        assert!(!plan.mark_default_applied);
        assert!(!plan.rewrite_existing_entry);
    }

    #[test]
    fn an_enabled_stale_entry_is_still_repaired_after_the_default() {
        let plan = autostart_startup_plan(true, true, true, false);

        assert!(!plan.enable_default);
        assert!(!plan.mark_default_applied);
        assert!(plan.rewrite_existing_entry);
    }

    #[test]
    fn an_enabled_legacy_entry_without_the_marker_is_rewritten() {
        let plan = autostart_startup_plan(true, true, false, true);

        assert!(!plan.enable_default);
        assert!(!plan.mark_default_applied);
        assert!(plan.rewrite_existing_entry);
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod lifecycle_tests {
    use super::should_prevent_exit;

    #[test]
    fn user_or_last_window_exit_is_prevented_for_tray_lifetime() {
        assert!(should_prevent_exit(None));
    }

    #[test]
    fn explicit_exit_and_restart_remain_allowed() {
        assert!(!should_prevent_exit(Some(0)));
        assert!(!should_prevent_exit(Some(1)));
    }
}
