use std::{backtrace::Backtrace, fs, panic, sync::Once};

use tauri::{AppHandle, Manager};

static PANIC_HOOK: Once = Once::new();

fn panic_report(info: &str, backtrace: &Backtrace) -> String {
    format!("Beakr Desktop panicked: {info}\nBacktrace:\n{backtrace}")
}

/// Install a process-wide hook after the file logger is initialized.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let backtrace = Backtrace::force_capture();
            log::error!("{}", panic_report(&info.to_string(), &backtrace));
            log::logger().flush();
            default_hook(info);
        }));
    });
}

/// Flush ordinary runtime records regularly so a force-terminated GUI process
/// still leaves recent evidence, not only records from graceful shutdown.
pub fn spawn_log_flusher() {
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            log::logger().flush();
        }
    });
}

/// Compile-time-only release panic used by the packaged regression.
/// Production builds do not contain an enabled path to this panic.
pub fn trigger_test_panic_if_configured() {
    if option_env!("BEAKR_TEST_PANIC_ON_STARTUP") == Some("1") {
        panic!("ENG-1967 controlled startup panic");
    }
}

/// Open the platform log directory with the user's default file manager.
#[tauri::command]
pub fn open_log_folder(app: AppHandle) -> Result<(), String> {
    let log_directory = app
        .path()
        .app_log_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&log_directory).map_err(|error| error.to_string())?;
    open::that_detached(&log_directory).map_err(|error| error.to_string())?;
    log::info!("Opened log folder: {}", log_directory.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_report_contains_context_and_backtrace_heading() {
        let report = panic_report("controlled failure", &Backtrace::disabled());
        assert!(report.contains("Beakr Desktop panicked: controlled failure"));
        assert!(report.contains("Backtrace:"));
    }
}
