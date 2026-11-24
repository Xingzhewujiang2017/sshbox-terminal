//! 实验性 Web UI 模式：把后端做成 HTTP + WebSocket 服务，用浏览器当界面。
//!
//! 为什么存在：目标机是 EulerOS V2R12 / openEuler 系，WebKitGTK 冻结在 2.38.2，
//! 而 Tauri v2（wry 0.55 的 `linux-body` → `webkit2gtk/v2_40`）硬性要求 ≥ 2.40，
//! 原生窗口在那台机器上装不起来。浏览器不受这个限制。
//!
//! 这是**验证件**，不是产品：只实现"看主机列表 → 连上去 → 终端能打字"这条主线，
//! 其余命令一律返回 `spike 未实现`。用它回答一个问题：值不值得投 3–4 周改路线。
//!
//! 启动：`sshbox-web [端口]`，默认 8787；`SSHBOX_DIST` 指定前端 dist 目录。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::ssh::{ClientHandler, SessionInfo};
use crate::{hostkey, store};

/// 前端 dist 里 index.html 要插的一行。
const SHIM_TAG: &str = "<script src=\"/shim.js\"></script>";
/// shim 本身：实现 `window.__TAURI_INTERNALS__`，让现有前端不改一行就能跑。
const SHIM_JS: &str = include_str!("../web/shim.js");

#[derive(Clone)]
struct Ctx {
    /// 事件总线：所有 WebSocket 客户端共享（等价于 Tauri 的 app.emit）
    ev: broadcast::Sender<String>,
    sessions: Arc<Mutex<HashMap<String, Sess>>>,
    /// 进程内密码缓存。目标机上没有 keyring 守护进程，`keyring` 会失败；
    /// 验证件里把密码只放内存，重启即失效（正式版必须走凭据管理器）。
    passwords: Arc<Mutex<HashMap<String, String>>>,
    dist: Arc<std::path::PathBuf>,
}

struct Sess {
    tx: mpsc::UnboundedSender<PtyCmd>,
    info: SessionInfo,
}

enum PtyCmd {
    Data(Vec<u8>),
    Resize(u32, u32),
    Close,
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ---------------------------------------------------------------- 静态资源

fn dist_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SSHBOX_DIST") {
        return std::path::PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for cand in [dir.join("dist"), dir.join("../dist"), dir.join("../../dist")] {
        if cand.join("index.html").is_file() {
            return cand;
        }
    }
    std::path::PathBuf::from("dist")
}

/// 在 `<head>` 后立刻插一行 shim —— 必须早于任何应用脚本。
/// （Vite 的入口是 type=module，本来就会延后执行，但把它放最前面更稳：
/// 不依赖"module 一定延后"这个前提，也方便调试时一眼看到 shim 先跑。）
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
                    log::warn!("[web] 事件积压，丢了 {} 条", n);
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

fn emit(ctx: &Ctx, event: &str, payload: Value) {
    let _ = ctx
        .ev
        .send(json!({ "event": event, "payload": payload }).to_string());
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
            "credential_store_ok": store::credential_store_ok(),
        })),
        "known_hosts_list" => Ok(json!([])),
        // AI 面板空着就行：验证阶段不碰模型
        "ai_settings" => Ok(json!({
            "active_profile_id": "",
            "explain_profile_id": "",
            "profiles": [],
            "report_ai_summary": false,
        })),
        "ui_log" => {
            // 前端出问题时唯一能看到的地方：把它打到终端（Electron 里就是主进程日志）
            let msg = arg(a, "msg").and_then(Value::as_str).unwrap_or_default();
            let level = arg(a, "level").and_then(Value::as_str).unwrap_or("info");
            println!("[ui][{}] {}", level, msg);
            Ok(Value::Null)
        }
        "log_set_level" => Ok(json!("info")),

        // --- 终端主线 ---
        "connect_host" => connect(ctx, a).await,
        "list_sessions" => {
            let m = ctx.sessions.lock().map_err(|_| "锁坏了")?;
            Ok(serde_json::to_value(m.values().map(|s| s.info.clone()).collect::<Vec<_>>())
                .unwrap_or(Value::Null))
        }
        "session_alive" => {
            let sid = a["sid"].as_str().unwrap_or_default();
            let m = ctx.sessions.lock().map_err(|_| "锁坏了")?;
            Ok(json!(m.contains_key(sid)))
        }
        "term_write" => {
            let sid = a["sid"].as_str().unwrap_or_default();
            let data = a["data"].as_str().unwrap_or_default();
            let m = ctx.sessions.lock().map_err(|_| "锁坏了")?;
            let s = m.get(sid).ok_or("会话不存在")?;
            s.tx
                .send(PtyCmd::Data(data.as_bytes().to_vec()))
                .map_err(|_| "会话已断开".to_string())?;
            Ok(Value::Null)
        }
        "term_resize" => {
            let sid = a["sid"].as_str().unwrap_or_default();
            let cols = a["cols"].as_u64().unwrap_or(80) as u32;
            let rows = a["rows"].as_u64().unwrap_or(24) as u32;
            let m = ctx.sessions.lock().map_err(|_| "锁坏了")?;
            if let Some(s) = m.get(sid) {
                let _ = s.tx.send(PtyCmd::Resize(cols, rows));
            }
            Ok(Value::Null)
        }
        "disconnect" => {
            let sid = a["sid"].as_str().unwrap_or_default();
            let mut m = ctx.sessions.lock().map_err(|_| "锁坏了")?;
            if let Some(s) = m.remove(sid) {
                let _ = s.tx.send(PtyCmd::Close);
            }
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
                // 目标机没有 keyring 守护进程，密码只放进程内存（重启即失效）
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
            let id = arg(a, "id").and_then(Value::as_str).unwrap_or_default();
            store::delete_host(id).map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "host_touch" => {
            if let Some(id) = arg(a, "id").and_then(Value::as_str) {
                store::mark_used(id);
            }
            Ok(Value::Null)
        }
        "credential_set" => {
            let id = arg(a, "id").and_then(Value::as_str).unwrap_or_default().to_string();
            let pw = arg(a, "password").and_then(Value::as_str).unwrap_or_default();
            if let Ok(mut m) = ctx.passwords.lock() {
                m.insert(id.clone(), pw.to_string());
            }
            let _ = store::set_secret(&id, pw);
            Ok(Value::Null)
        }
        "credential_delete" => {
            let id = arg(a, "id").and_then(Value::as_str).unwrap_or_default();
            if let Ok(mut m) = ctx.passwords.lock() {
                m.remove(id);
            }
            let _ = store::delete_secret(id);
            Ok(Value::Null)
        }

        // --- 让界面别报错的空实现 ---
        "monitor_set_visible" | "monitor_set_paused" | "monitor_restart"
        | "monitor_sample_now" | "sftp_forget" | "forward_stop_all" => Ok(Value::Null),
        "monitor_paused" => Ok(json!(false)),
        "credential_has" => {
            let id = arg(a, "id").and_then(Value::as_str).unwrap_or_default();
            let has = ctx
                .passwords
                .lock()
                .map(|m| m.contains_key(id))
                .unwrap_or(false);
            Ok(json!(has))
        }
        // 验证件不落盘密码，所以"凭据管理器可用"恒为 false
        "credential_store_status" => Ok(json!(false)),
        "forward_list" => Ok(json!([])),

        _ => Err(format!("spike 未实现：{}", cmd)),
    }
}

// ---------------------------------------------------------------- SSH 连接

async fn connect(ctx: &Ctx, a: &Value) -> Result<Value, String> {
    let host_id = arg(a, "host_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut password = arg(a, "password").and_then(Value::as_str).map(str::to_string);
    // 前端没带密码时，回落到进程内缓存（host_save 时存进来的）
    if password.as_deref().unwrap_or("").is_empty() {
        password = ctx
            .passwords
            .lock()
            .ok()
            .and_then(|m| m.get(&host_id).cloned());
    }
    let key_passphrase = arg(a, "key_passphrase")
        .and_then(Value::as_str)
        .map(str::to_string);
    let accept_host_key = arg(a, "accept_host_key")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let host = store::get_host(&host_id).ok_or("主机记录不存在")?;
    let port = host.port;
    let policy = if accept_host_key {
        "accept_any".to_string()
    } else {
        host.host_key_policy
            .clone()
            .unwrap_or_else(|| "strict".to_string())
    };

    let outcome_slot: Arc<Mutex<Option<hostkey::VerifyOutcome>>> = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        host: host.host.clone(),
        port,
        policy,
        known_hosts_path: store::known_hosts_path(),
        outcome: outcome_slot.clone(),
        forwards: Default::default(),
    };
    let config = Arc::new(russh::client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..<_>::default()
    });

    let addr = (host.host.as_str(), port);
    let mut handle = match tokio::time::timeout(
        Duration::from_secs(25),
        russh::client::connect(config, addr, handler),
    )
    .await
    {
        Err(_) => return Err("连接超时（25 秒）".to_string()),
        Ok(Err(e)) => {
            // 主机密钥问题要单独说清楚，否则前端只会显示"连不上"
            let o = outcome_slot.lock().ok().and_then(|x| x.clone());
            return Err(match o {
                Some(hostkey::VerifyOutcome::Unknown { fingerprint, .. }) => format!(
                    "SSHBOX_ERR:{}",
                    json!({"kind":"host_key_unknown","message":format!("首次连接 {}:{}，指纹 {}", host.host, port, fingerprint),"host":host.host,"port":port,"host_id":host_id,"fingerprint":fingerprint})
                ),
                Some(hostkey::VerifyOutcome::Changed { fingerprint, expected, .. }) => format!(
                    "SSHBOX_ERR:{}",
                    json!({
                        "kind": "host_key_changed",
                        "message": format!("{}:{} 的主机密钥变了（记录 {}，现在 {}）", host.host, port, expected, fingerprint),
                        "host": host.host,
                        "port": port,
                        "expected_fingerprint": expected,
                        "fingerprint": fingerprint,
                        "host_id": host_id,
                    })
                ),
                _ => format!("无法连接 {}:{}（{}）", host.host, port, e),
            });
        }
        Ok(Ok(h)) => h,
    };

    // --- 认证 ---
    let auth_res = if let Some(pw) = password.as_ref().filter(|p| !p.is_empty()) {
        handle
            .authenticate_password(&host.username, pw)
            .await
            .map_err(|e| format!("密码认证请求失败: {}", e))?
    } else if let Some(kp) = host.key_path.as_ref().filter(|p| !p.trim().is_empty()) {
        let pem = std::fs::read_to_string(kp).map_err(|e| format!("读取私钥失败: {}", e))?;
        let mut key = russh::keys::PrivateKey::from_openssh(pem.trim())
            .map_err(|e| format!("私钥格式无效: {}", e))?;
        if key.is_encrypted() {
            let pass = key_passphrase
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or("私钥已加密，需要口令")?;
            key = key
                .decrypt(pass)
                .map_err(|e| format!("私钥口令错误: {}", e))?;
        }
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .map_err(|e| format!("协商签名算法失败: {}", e))?
            .flatten();
        handle
            .authenticate_publickey(
                &host.username,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
            )
            .await
            .map_err(|e| format!("私钥认证请求失败: {}", e))?
    } else {
        // 让前端弹出密码框：必须用 SSHBOX_ERR 约定，否则界面只会报一句普通错误
        return Err(format!(
            "SSHBOX_ERR:{}",
            json!({
                "kind": "need_password",
                "message": "需要密码",
                "host": host.host,
                "port": port,
                "host_id": host_id,
            })
        ));
    };
    if !auth_res.success() {
        return Err("认证被拒绝（检查用户名/密码/私钥）".to_string());
    }

    // --- PTY + shell ---
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开会话通道失败: {}", e))?;
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await
        .map_err(|e| format!("请求 PTY 失败: {}", e))?;
    channel
        .request_shell(true)
        .await
        .map_err(|e| format!("启动 shell 失败: {}", e))?;

    let sid = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<PtyCmd>();
    let info = SessionInfo {
        sid: sid.clone(),
        host_id: Some(host_id.clone()),
        label: host.name.clone(),
        host: host.host.clone(),
        port,
        username: host.username.clone(),
        connected_at: chrono::Utc::now().timestamp(),
        closed: false,
    };

    // 读循环：远端输出 → WebSocket；本地按键 → channel
    {
        let ev = ctx.ev.clone();
        let sid2 = sid.clone();
        let label = info.label.clone();
        let hid = info.host_id.clone();
        tokio::spawn(async move {
            let send = |event: &str, payload: Value| {
                let _ = ev.send(json!({ "event": event, "payload": payload }).to_string());
            };
            loop {
                tokio::select! {
                    cmd = rx.recv() => match cmd {
                        Some(PtyCmd::Data(bytes)) => {
                            if channel.data(&bytes[..]).await.is_err() { break }
                        }
                        Some(PtyCmd::Resize(c, r)) => {
                            let _ = channel.window_change(c, r, 0, 0).await;
                        }
                        Some(PtyCmd::Close) | None => break,
                    },
                    msg = channel.wait() => match msg {
                        Some(russh::ChannelMsg::Data { ref data }) => {
                            send("ssh://data", json!({"sid": sid2, "data": b64(&data[..])}))
                        }
                        Some(russh::ChannelMsg::ExtendedData { ref data, ext: 1 }) => {
                            send("ssh://data", json!({"sid": sid2, "data": b64(&data[..])}))
                        }
                        Some(russh::ChannelMsg::Close) | None => break,
                        _ => {}
                    }
                }
            }
            send("ssh://closed", json!({"sid": sid2, "host_id": hid, "label": label}));
        });
    }

    ctx.sessions
        .lock()
        .map_err(|_| "锁坏了")?
        .insert(sid.clone(), Sess { tx, info });
    store::mark_used(&host_id);
    Ok(json!(sid))
}

// ---------------------------------------------------------------- 启动

pub async fn run(port: u16) -> anyhow::Result<()> {
    let (ev, _) = broadcast::channel::<String>(1024);
    let ctx = Ctx {
        ev,
        sessions: Arc::new(Mutex::new(HashMap::new())),
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
            tower_http::services::ServeDir::new(dist.as_ref().clone()).fallback(
                tower_http::services::ServeFile::new(dist.join("index.html")),
            ),
        )
        .with_state(ctx);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    log::info!("[web] dist = {}", dist.display());
    log::info!("[web] 监听 http://{}", addr);
    println!("SSHBox Web 验证版：http://127.0.0.1:{}", port);
    println!("前端目录：{}", dist.display());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
