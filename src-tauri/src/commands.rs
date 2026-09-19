//! IPC commands for host management, credentials, settings and known_hosts.

use serde::{Deserialize, Serialize};

use crate::{hostkey, store};

fn e(err: impl std::fmt::Display) -> String {
    crate::ssh::err_kind("store_error", format!("{:#}", err))
}

#[tauri::command]
pub fn hosts_list() -> store::HostsFile {
    store::load_hosts()
}

/// Create/update a host. `password` (when given) goes straight to the OS
/// credential store — it is never written into hosts.json.
#[tauri::command]
pub fn host_save(host: store::Host, password: Option<String>) -> Result<store::Host, String> {
    let mut host = host;
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        store::set_secret(&host.id, &pw).map_err(e)?;
        host.save_password = true;
    } else if !host.save_password {
        // User unchecked "记住密码" — drop any stored secret.
        let _ = store::delete_secret(&host.id);
    }
    store::upsert_host(host).map_err(e)
}

#[tauri::command]
pub fn host_delete(id: String) -> Result<(), String> {
    store::delete_host(&id).map_err(e)
}

#[tauri::command]
pub fn host_reorder(ids: Vec<String>) -> Result<(), String> {
    let mut file = store::load_hosts();
    let mut ordered: Vec<store::Host> = Vec::new();
    for id in &ids {
        if let Some(pos) = file.hosts.iter().position(|h| &h.id == id) {
            ordered.push(file.hosts.remove(pos));
        }
    }
    ordered.extend(file.hosts.drain(..));
    file.hosts = ordered;
    store::save_hosts(&file).map_err(e)
}

#[tauri::command]
pub fn host_touch(id: String) {
    store::mark_used(&id);
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn credential_has(id: String) -> bool {
    store::has_secret(&id)
}

#[tauri::command]
pub fn credential_set(id: String, password: String) -> Result<(), String> {
    store::set_secret(&id, &password).map_err(e)
}

#[tauri::command]
pub fn credential_delete(id: String) -> Result<(), String> {
    store::delete_secret(&id).map_err(e)
}

#[tauri::command]
pub fn credential_store_status() -> bool {
    store::credential_store_ok()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn settings_get() -> store::Settings {
    store::load_settings()
}

#[tauri::command]
pub fn settings_set(settings: store::Settings) -> Result<(), String> {
    store::save_settings(&settings).map_err(e)
}

// ---------------------------------------------------------------------------
// known_hosts
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn known_hosts_list() -> Vec<hostkey::KnownHostEntry> {
    hostkey::list(&store::known_hosts_path())
}

#[tauri::command]
pub fn known_hosts_remove(host: String, port: u16) -> Result<usize, String> {
    hostkey::remove(&store::known_hosts_path(), &host, port).map_err(e)
}

// ---------------------------------------------------------------------------
// Import / export
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportBundle {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<store::Host>,
    /// host id -> password, only when the user asked to include secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<std::collections::HashMap<String, String>>,
}

fn one() -> u32 {
    1
}

#[tauri::command]
pub fn hosts_export(path: String, include_secrets: bool) -> Result<usize, String> {
    let file = store::load_hosts();
    let secrets = if include_secrets {
        let mut map = std::collections::HashMap::new();
        for h in &file.hosts {
            if let Ok(pw) = store::get_secret(&h.id) {
                map.insert(h.id.clone(), pw);
            }
        }
        Some(map)
    } else {
        None
    };
    let bundle = ExportBundle {
        version: file.version,
        groups: file.groups,
        hosts: file.hosts.clone(),
        secrets,
    };
    let n = bundle.hosts.len();
    let json = serde_json::to_string_pretty(&bundle).map_err(e)?;
    std::fs::write(&path, json).map_err(e)?;
    Ok(n)
}

#[tauri::command]
pub fn hosts_import(path: String) -> Result<usize, String> {
    let text = std::fs::read_to_string(&path).map_err(e)?;
    let bundle: ExportBundle = serde_json::from_str(&text).map_err(e)?;
    let mut file = store::load_hosts();
    let mut imported = 0usize;
    let mut pending_secrets: Vec<(String, String)> = Vec::new();
    for host in bundle.hosts {
        let mut host = host;
        let orig_id = host.id.clone();
        // Avoid clobbering an unrelated host that happens to share the id.
        if file.hosts.iter().any(|h| h.id == host.id) {
            host.id = uuid::Uuid::new_v4().to_string();
        }
        if !file.groups.iter().any(|g| g == &host.group) {
            file.groups.push(host.group.clone());
        }
        if let Some(pw) = bundle.secrets.as_ref().and_then(|s| s.get(&orig_id)) {
            pending_secrets.push((host.id.clone(), pw.clone()));
        }
        file.hosts.push(host);
        imported += 1;
    }
    store::save_hosts(&file).map_err(e)?;

    // Secrets follow the (possibly regenerated) ids captured above.
    for (id, pw) in pending_secrets {
        let _ = store::set_secret(&id, &pw);
    }
    Ok(imported)
}

#[derive(Debug, Serialize)]
pub struct AppPaths {
    pub data_dir: String,
    pub hosts_json: String,
    pub settings_json: String,
    pub known_hosts: String,
    pub credential_store_ok: bool,
}

#[tauri::command]
pub fn app_paths() -> AppPaths {
    let d = store::data_dir();
    AppPaths {
        data_dir: d.display().to_string(),
        hosts_json: d.join("hosts.json").display().to_string(),
        settings_json: d.join("settings.json").display().to_string(),
        known_hosts: d.join("known_hosts").display().to_string(),
        credential_store_ok: store::credential_store_ok(),
    }
}
