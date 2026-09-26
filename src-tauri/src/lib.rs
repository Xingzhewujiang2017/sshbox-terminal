mod ai;
mod ai_commands;
mod alerts;
mod commands;
mod forward;
mod history;
mod hostkey;
mod lab;
mod localfs;
mod logbook;
mod monitor;
mod report;
mod sftp;
mod ssh;
mod store;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // C1：轮转上限（默认 5MB）支持环境变量覆盖，冒烟 C19 用它把上限调小来测轮转
    let max_log_bytes = std::env::var("SSHBOX_LOG_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u128>().ok())
        .unwrap_or(5 * 1024 * 1024);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                // C2 级别开关的接法（grill 致命修正 #2）：
                // .level() 是 fern 的 dispatch 级过滤器、构造时固化，运行时改
                // log::set_max_level 穿不过它 → 这里放行全部，真实闸门交给
                // setup 里运行时可改的 log::set_max_level（默认 Info，重启回 Info）。
                .level(log::LevelFilter::Trace)
                .level_for("hyper", log::LevelFilter::Info)
                .level_for("reqwest", log::LevelFilter::Info)
                .level_for("tungstenite", log::LevelFilter::Info)
                .level_for("tao", log::LevelFilter::Info)
                .level_for("wry", log::LevelFilter::Info)
                // Q1 拍板：日志时间戳用本机本地时间（默认是 UTC，差 8 小时）
                .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                .max_file_size(max_log_bytes)
                // C1 拍板：KeepAll 只改名归档、从不删 → 改 KeepSome(10)，上限 50MB
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(10))
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
            ssh::monitor_recent,
            ssh::monitor_set_visible,
            ssh::monitor_restart,
            ssh::monitor_set_paused,
            ssh::monitor_paused,
            ssh::monitor_sample_now,
            // --- 故障演练台 ---
            lab::exec_batch,
            lab::ping_now,
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
            ai_commands::ai_set_explain_profile,
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
            // --- 日志（v0.7.1）---
            commands::log_set_level,
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
        .setup(move |app| {
            // Writer thread first: samples may start arriving as soon as a
            // session connects.
            history::init();
            // 存量配置里 max_tokens=1024 对推理模型不可用（思考会把额度吃光），
            // 这里一次性升上去并写回 —— 写盘只在这一处，便于排查配置何时被改。
            store::migrate_settings_file();

            // --- 日志与故障记录兜底（v0.7.1）---
            // 日志目录与插件一致：%LOCALAPPDATA%\<identifier>\logs
            let log_dir = app
                .path()
                .app_log_dir()
                .unwrap_or_else(|_| store::data_dir().join("logs"));
            let _ = logbook::LOG_DIR.set(log_dir.clone());
            let log_file = log_dir.join(format!("{}.log", app.package_info().name));
            logbook::install_panic_hook(log_file);
            // A3 异常退出标记：先扫上一次的残留（报一次即清），再立自己的
            logbook::scan_and_warn(&log_dir);
            logbook::mark_started(&log_dir);
            // C2 级别开关：真实闸门在这里，运行时可改；重启回到 Info
            log::set_max_level(log::LevelFilter::Info);

            // B1 启动行：先打快的（版本 + 配置目录 + 设置快照）。
            // OS / WebView2 版本放后台线程补 `[env]` 行 ——
            // webview_version 首次调用实测 2–5 秒，放 setup 里会拖死
            // 窗口创建，WebView2 初始导航等不及就整页 ERR_CONNECTION_REFUSED。
            let s = store::load_settings();
            log::info!(
                "SSHBox {} 启动 | 配置目录 {} | 采样间隔 {}s | 落盘 {} | 告警 {} | 轮转上限 {}B",
                env!("CARGO_PKG_VERSION"),
                store::data_dir().display(),
                s.sample_interval_secs,
                s.history_enabled,
                s.alerts_enabled,
                max_log_bytes,
            );
            std::thread::spawn(|| {
                let wv = logbook::webview2_version();
                log::info!("[env] OS {} | WebView2 {}", logbook::os_version(), wv);
            });

            // A1 验收入口（冒烟 C22）：强制在 setup 里 panic，验证 panic hook 落盘 +
            // 异常退出标记。环境变量门控，正常启动绝不影响。
            if std::env::var("SSHBOX_TEST_PANIC").is_ok() {
                panic!("SSHBOX_TEST_PANIC forced panic for log verification");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {});
    // 正常退出（所有窗口关完）才走到这里：删掉自己的退出标记
    logbook::mark_stopped(logbook::log_dir());
}
