mod monitor;
mod ssh;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(ssh::SessionManager::default())
        .invoke_handler(tauri::generate_handler![
            ssh::connect,
            ssh::term_write,
            ssh::term_resize,
            ssh::disconnect,
            ssh::list_sessions,
            ssh::monitor_set_visible
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
