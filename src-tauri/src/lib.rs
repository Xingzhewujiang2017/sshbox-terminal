mod commands;
mod hostkey;
mod monitor;
mod ssh;
mod store;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .manage(ssh::SessionManager::default())
        .invoke_handler(tauri::generate_handler![
            // --- sessions / terminal ---
            ssh::connect,
            ssh::connect_host,
            ssh::term_write,
            ssh::term_resize,
            ssh::disconnect,
            ssh::list_sessions,
            ssh::session_alive,
            ssh::monitor_set_visible,
            ssh::monitor_restart,
            ssh::monitor_set_paused,
            ssh::monitor_paused,
            ssh::monitor_sample_now,
            // --- hosts ---
            commands::hosts_list,
            commands::host_save,
            commands::host_delete,
            commands::host_reorder,
            commands::host_touch,
            // --- credentials ---
            commands::credential_has,
            commands::credential_set,
            commands::credential_delete,
            commands::credential_store_status,
            // --- settings ---
            commands::settings_get,
            commands::settings_set,
            // --- known_hosts ---
            commands::known_hosts_list,
            commands::known_hosts_remove,
            // --- import / export / paths ---
            commands::hosts_export,
            commands::hosts_import,
            commands::app_paths,
            commands::ui_log,
        ])
        .setup(|_app| {
            log::info!(
                "SSHBox {} 启动，配置目录 {}",
                env!("CARGO_PKG_VERSION"),
                store::data_dir().display()
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
