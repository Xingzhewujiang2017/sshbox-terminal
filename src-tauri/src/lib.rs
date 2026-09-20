mod ai;
mod ai_commands;
mod alerts;
mod commands;
mod forward;
mod history;
mod hostkey;
mod localfs;
mod monitor;
mod report;
mod sftp;
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
        .plugin(tauri_plugin_notification::init())
        .manage(ssh::SessionManager::default())
        .manage(sftp::SftpManager::default())
        .manage(forward::ForwardManager::default())
        .invoke_handler(tauri::generate_handler![
            // --- sessions / terminal ---
            ssh::connect,
            ssh::connect_host,
            ssh::term_write,
            ssh::term_resize,
            ssh::disconnect,
            ssh::list_sessions,
            ssh::session_alive,
            ssh::monitor_static,
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
            commands::groups_reorder,
            // --- AI（v0.5）---
            ai_commands::ai_settings,
            ai_commands::ai_profile_save,
            ai_commands::ai_profile_delete,
            ai_commands::ai_set_active,
            ai_commands::ai_set_report_summary,
            ai_commands::ai_key_has,
            ai_commands::ai_key_delete,
            ai_commands::ai_presets,
            ai_commands::ai_test,
            ai_commands::ai_chat,
            ai_commands::ai_ask,
            ai_commands::ai_cancel,
            ai_commands::ai_protocol_label,
            // --- 巡检报告（v0.5）---
            report::report_generate,
            report::report_reveal,
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
            // --- history ---
            history::history_range,
            history::history_range_multi,
            history::history_hosts,
            history::history_stats,
            history::history_export,
                        history::export_csv_text,
                        history::ping_range,
                        history::ping_range_multi,
            // --- sftp / 本地文件 ---
            sftp::sftp_list,
            sftp::sftp_stat,
            sftp::sftp_mkdir,
            sftp::sftp_rename,
            sftp::sftp_remove,
            sftp::sftp_upload,
            sftp::sftp_download,
            sftp::sftp_cancel,
            sftp::sftp_forget,
            sftp::sftp_upload_dir,
            sftp::sftp_download_dir,
            sftp::sftp_upload_drop,
            localfs::local_list,
            localfs::local_kinds,
            // --- 端口转发 ---
            forward::forward_list,
            forward::forward_save,
            forward::forward_delete,
            forward::forward_start,
            forward::forward_stop,
            forward::forward_stop_all,
            localfs::local_mkdir,
            localfs::local_rename,
            localfs::local_remove,
        ])
        .setup(|_app| {
            // Writer thread first: samples may start arriving as soon as a
            // session connects.
            history::init();
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
