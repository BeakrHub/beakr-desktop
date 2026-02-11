use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "settings.json";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    pub scoped_folders: Vec<String>,
    pub device_name: Option<String>,
    pub auto_connect: bool,
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

    Settings {
        scoped_folders,
        device_name,
        auto_connect,
    }
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
}
