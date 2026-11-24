//! Web / Electron 壳的后端：HTTP + WebSocket 服务，浏览器或 Electron 窗口当界面。
//!
//! 为什么存在：目标机 openEuler 系把 WebKitGTK 冻结在 2.38.2，而 Tauri v2
//! （wry 0.55 的 `linux-body` → `webkit2gtk/v2_40`）硬性要求 ≥ 2.40；那台机器的
//! libsoup3 还缺 nghttp2 符号，任何 GTK 程序加载即崩。原生窗口路线关闭。
//!
//! 关键设计：**业务代码不分叉**。这里复用 `ssh::connect_host_impl`、
//! `monitor::*`、`store::*`，和桌面版走同一条路径；事件通过 `emit::emit`
//! 走进程级 sink（桌面版发给窗口，这里发给 WebSocket 广播）。
//!
//! 启动：`sshbox-web [端口]`，默认随机空闲端口由壳指定；`SSHBOX_DIST` 指前端目录。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::ssh::{self, PtyCmd, SessionManager};
use crate::{monitor, store};

/// 前端 dist 里 index.html 要插的一行。
const SHIM_TAG: &str = "<script src=\"/shim.js\"></script>";
/// shim 本身：实现 `window.__TAURI_INTERNALS__`，让现有前端不改一行就能跑。
const SHIM_JS: &str = include_str!("../web/shim.js");

#[derive(Clone)]
struct Ctx {
    /// 事件总线：所有 WebSocket 客户端共享（等价于 Tauri 的 app.emit）
    ev: broadcast::Sender<String>,
    /// 和桌面版同一个会话管理器 —— 这样 monitor/sftp 等模块能直接复用
    sessions: Arc<SessionManager>,
    /// 进程内密码缓存。目标机没有 keyring 守护进程，`keyring` 会失败；
    /// 正式版必须接回凭据管理器（或自建加密文件 + 主密码）。
    passwords: Arc<Mutex<HashMap<String, String>>>,
    dist: Arc<PathBuf>,
}

// ---------------------------------------------------------------- 静态资源

fn dist_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SSHBOX_DIST") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for cand in [dir.join("dist"), dir.join("../dist"), dir.join("../../dist")] {
        if cand.join("index.html").is_file() {
            return cand;
        }
    }
    PathBuf::from("dist")
}

/// 在 `<head>` 后立刻插一行 shim —— 必须早于任何应用脚本。
fn inject_shim(html: &str) -> String {
    if let Some(i) = html.find("<head>") {
        let at = i + "<head>".len();
        return format!("{}{}{}", &html[..at], SHIM_TAG, &html[at..]);
    }
    match html.find("</head>") {
        Some(i) => format!("{}{}{}", &html[..i], SHIM_TAG, &html[i..]),
        None => format!("{}{}", SHIM_TAG, html),
    }
}

async fn index(State(ctx): State<Ctx>) -> Response {
    let p = ctx.dist.join("index.html");
    match tokio::fs::read_to_string(&p).await {
        Ok(html) => Html(inject_shim(&html)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "读不到 {}（{}）。用 SSHBOX_DIST 指向前端 dist 目录。",
                p.display(),
                e
            ),
        )
            .into_response(),
    }
}

async fn shim() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        SHIM_JS,
    )
        .into_response()
}

// ---------------------------------------------------------------- WebSocket

async fn ws_upgrade(ws: WebSocketUpgrade, State(ctx): State<Ctx>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, ctx))
}

async fn ws_loop(mut socket: WebSocket, ctx: Ctx) {
    let mut rx = ctx.ev.subscribe();
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(msg) => {
                    if socket.send(Message::Text(msg.into())).await.is_err() { break }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    println!("[web] 事件积压，丢了 {} 条", n);
                }
                Err(_) => break,
            },
            m = socket.recv() => {
                if m.is_none() { break }
            }
        }
    }
}

// ---------------------------------------------------------------- IPC 入口

async fn invoke(State(ctx): State<Ctx>, Json(req): Json<Value>) -> Response {
    let cmd = req
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = req.get("args").cloned().unwrap_or(Value::Null);
    let res = dispatch(&ctx, &cmd, &args).await;
    // 高频命令（每次按键都会来）失败时不打日志，免得刷屏
    let quiet = matches!(
        cmd.as_str(),
        "term_write" | "term_resize" | "session_alive" | "ui_log" | "list_sessions"
    );
    let out = match res {
        Ok(v) => json!({ "ok": true, "data": v }),
        Err(e) => {
            if !quiet {
                println!("[invoke] {} 失败: {}", cmd, e);
            }
            json!({ "ok": false, "error": e })
        }
    };
    Json(out).into_response()
}

/// 取命令参数：Tauri 的 `#[tauri::command]` 会把前端传来的 camelCase
/// 自动转成 snake_case 再绑定；我们直接吃原始 JSON，所以两种写法都得认。
/// （踩过：`connect_host` 传的是 hostId/acceptHostKey，只认 snake 就永远取空。）
fn arg<'a>(a: &'a Value, snake: &str) -> Option<&'a Value> {
    if let Some(v) = a.get(snake) {
        return Some(v);
    }
    let mut it = snake.split('_');
    let mut camel = String::from(it.next().unwrap_or(""));
    for part in it {
        let mut cs = part.chars();
        if let Some(first) = cs.next() {
            camel.extend(first.to_uppercase());
            camel.push_str(cs.as_str());
        }
    }
    a.get(&camel)
}

fn s_arg(a: &Value, key: &str) -> String {
    arg(a, key).and_then(Value::as_str).unwrap_or_default().to_string()
}

async fn dispatch(ctx: &Ctx, cmd: &str, a: &Value) -> Result<Value, String> {
    match cmd {
        // --- 启动必需 ---
        "hosts_list" => Ok(serde_json::to_value(store::load_hosts()).unwrap_or(Value::Null)),
        "settings_get" => Ok(serde_json::to_value(store::load_settings()).unwrap_or(Value::Null)),
        "app_paths" => Ok(json!({
            "data_dir": store::data_dir().to_string_lossy(),
            "hosts_json": store::data_dir().join("hosts.json").to_string_lossy(),
            "settings_json": store::data_dir().join("settings.json").to_string_lossy(),
            "known_hosts": store::known_hosts_path().to_string_lossy(),
            "credential_store_ok": false,
        })),
        "known_hosts_list" => Ok(json!([])),
        "ai_settings" => Ok(json!({
            "active_profile_id": "",
            "explain_profile_id": "",
            "profiles": [],
            "report_ai_summary": false,
        })),
        "ui_log" => {
            let msg = s_arg(a, "msg");
            let level = arg(a, "level").and_then(Value::as_str).unwrap_or("info");
            println!("[ui][{}] {}", level, msg);
            Ok(Value::Null)
        }
        "log_set_level" => Ok(json!("info")),

        // --- 终端主线 ---
        "connect_host" => {
            let host_id = s_arg(a, "host_id");
            let mut password = arg(a, "password").and_then(Value::as_str).map(str::to_string);
            if password.as_deref().unwrap_or("").is_empty() {
                password = ctx.passwords.lock().ok().and_then(|m| m.get(&host_id).cloned());
            }
            let key_passphrase = arg(a, "key_passphrase")
                .and_then(Value::as_str)
                .map(str::to_string);
            let accept_host_key = arg(a, "accept_host_key")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let save_password = arg(a, "save_password").and_then(Value::as_bool);
            let sid = ssh::connect_host_impl(
                host_id,
                password,
                key_passphrase,
                accept_host_key,
                save_password,
                &ctx.sessions,
            )
            .await?;
            Ok(json!(sid))
        }
        "list_sessions" => {
            let sessions = ctx.sessions.sessions.lock().await;
            let list: Vec<_> = sessions
                .values()
                .map(|s| {
                    let mut i = s.info.clone();
                    i.closed = s.closed.load(std::sync::atomic::Ordering::SeqCst);
                    i
                })
                .collect();
            Ok(serde_json::to_value(list).unwrap_or(Value::Null))
        }
        "session_alive" => {
            let sid = s_arg(a, "sid");
            let sessions = ctx.sessions.sessions.lock().await;
            Ok(json!(sessions
                .get(&sid)
                .map(|s| !s.closed.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap_or(false)))
        }
        "term_write" => {
            let sid = s_arg(a, "sid");
            let data = s_arg(a, "data");
            let sessions = ctx.sessions.sessions.lock().await;
            let s = sessions.get(&sid).ok_or("会话不存在")?;
            s.cmd_tx
                .send(PtyCmd::Data(data.as_bytes().to_vec()))
                .map_err(|_| "会话已断开".to_string())?;
            Ok(Value::Null)
        }
        "term_resize" => {
            let sid = s_arg(a, "sid");
            let cols = arg(a, "cols").and_then(Value::as_u64).unwrap_or(80) as u32;
            let rows = arg(a, "rows").and_then(Value::as_u64).unwrap_or(24) as u32;
            let sessions = ctx.sessions.sessions.lock().await;
            if let Some(s) = sessions.get(&sid) {
                let _ = s.cmd_tx.send(PtyCmd::Resize(cols, rows));
            }
            Ok(Value::Null)
        }
        "disconnect" => {
            let sid = s_arg(a, "sid");
            let mut sessions = ctx.sessions.sessions.lock().await;
            if let Some(session) = sessions.remove(&sid) {
                let _ = session
                    .handle
                    .disconnect(russh::Disconnect::ByApplication, "user closed", "en");
            }
            drop(sessions);
            crate::history::unbind(&sid);
            monitor::forget(&sid);
            Ok(Value::Null)
        }

        // --- 监控（复用桌面版同一套采集）---
        "monitor_static" => {
            let sid = s_arg(a, "sid");
            Ok(serde_json::to_value(monitor::cached_static(&sid)).unwrap_or(Value::Null))
        }
        "monitor_recent" => {
            let sid = s_arg(a, "sid");
            Ok(serde_json::to_value(monitor::recent(&sid)).unwrap_or(Value::Null))
        }
        "monitor_set_visible" => {
            let sid = s_arg(a, "sid");
            let visible = arg(a, "visible").and_then(Value::as_bool).unwrap_or(true);
            monitor::set_visible(&sid, visible);
            Ok(Value::Null)
        }
        "monitor_set_paused" => {
            let sid = s_arg(a, "sid");
            let paused = arg(a, "paused").and_then(Value::as_bool).unwrap_or(false);
            monitor::set_paused(&sid, paused);
            Ok(Value::Null)
        }
        "monitor_paused" => Ok(json!(monitor::is_paused(&s_arg(a, "sid")))),
        "monitor_sample_now" => {
            monitor::sample_now(&s_arg(a, "sid"));
            Ok(Value::Null)
        }
        "monitor_restart" => {
            let sid = s_arg(a, "sid");
            let sessions = ctx.sessions.sessions.lock().await;
            let s = sessions.get(&sid).ok_or("会话不存在")?;
            monitor::stop(&sid);
            monitor::spawn(sid.clone(), s.handle.clone(), s.closed.clone());
            Ok(Value::Null)
        }

        // --- 主机增删改（目标机上要靠它新建主机）---
        "host_save" => {
            let host: store::Host = serde_json::from_value(
                arg(a, "host").cloned().ok_or("缺少 host 参数")?,
            )
            .map_err(|e| format!("主机数据无效: {}", e))?;
            let saved = store::upsert_host(host).map_err(|e| e.to_string())?;
            if let Some(pw) = arg(a, "password").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                if let Ok(mut m) = ctx.passwords.lock() {
                    m.insert(saved.id.clone(), pw.to_string());
                }
                if let Err(e) = store::set_secret(&saved.id, pw) {
                    println!("[web] 凭据管理器不可用（验证件里属预期）: {}", e);
                }
            }
            Ok(serde_json::to_value(saved).unwrap_or(Value::Null))
        }
        "host_delete" => {
            let id = s_arg(a, "id");
            store::delete_host(&id).map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "host_touch" => {
            let id = s_arg(a, "id");
            store::mark_used(&id);
            Ok(Value::Null)
        }
        "credential_set" => {
            let id = s_arg(a, "id");
            let pw = s_arg(a, "password");
            if let Ok(mut m) = ctx.passwords.lock() {
                m.insert(id.clone(), pw.clone());
            }
            let _ = store::set_secret(&id, &pw);
            Ok(Value::Null)
        }
        "credential_delete" => {
            let id = s_arg(a, "id");
            if let Ok(mut m) = ctx.passwords.lock() {
                m.remove(&id);
            }
            let _ = store::delete_secret(&id);
            Ok(Value::Null)
        }
        "credential_has" => {
            let id = s_arg(a, "id");
            let has = ctx.passwords.lock().map(|m| m.contains_key(&id)).unwrap_or(false);
            Ok(json!(has))
        }
        // 验证件不落盘密码，所以"凭据管理器可用"恒为 false
        "credential_store_status" => Ok(json!(false)),

        // --- 还没接的模块 ---
        "forward_list" => Ok(json!([])),
        "sftp_forget" => Ok(Value::Null),

        _ => Err(format!("spike 未实现：{}", cmd)),
    }
}

// ---------------------------------------------------------------- 启动

pub async fn run(port: u16) -> anyhow::Result<()> {
    let (ev, _) = broadcast::channel::<String>(1024);
    // 业务层的事件出口：转成 WebSocket 消息广播出去
    {
        let tx = ev.clone();
        crate::emit::set_sink(move |event, payload| {
            let _ = tx.send(json!({ "event": event, "payload": payload }).to_string());
        });
    }
    let ctx = Ctx {
        ev,
        sessions: Arc::new(SessionManager::default()),
        passwords: Arc::new(Mutex::new(HashMap::new())),
        dist: Arc::new(dist_dir()),
    };
    let dist = ctx.dist.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/shim.js", get(shim))
        .route("/ws", get(ws_upgrade))
        .route("/invoke", post(invoke))
        .fallback_service(
            tower_http::services::ServeDir::new(dist.as_ref().clone())
                .fallback(tower_http::services::ServeFile::new(dist.join("index.html"))),
        )
        .with_state(ctx);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    println!("SSHBox Web 版：http://127.0.0.1:{}", port);
    println!("前端目录：{}", dist.display());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
