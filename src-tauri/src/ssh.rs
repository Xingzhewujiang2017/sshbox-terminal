//! SSH sessions: connect + auth + PTY terminal, one multiplexed connection per
//! session shared by the terminal, the monitor collector (and later SFTP).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, Mutex};

use crate::{hostkey, monitor, store};

pub type SessionId = String;

/// Errors cross the IPC boundary as `SSHBOX_ERR:<json>` so the UI can react to
/// specific conditions (unknown host key, missing password) instead of showing
/// a raw string.
pub const ERR_PREFIX: &str = "SSHBOX_ERR:";

#[derive(Debug, Default, Serialize)]
pub struct ErrPayload {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
}

pub fn err_payload(p: ErrPayload) -> String {
    format!(
        "{}{}",
        ERR_PREFIX,
        serde_json::to_string(&p).unwrap_or_else(|_| "{}".to_string())
    )
}

pub fn err_kind(kind: &str, message: impl Into<String>) -> String {
    err_payload(ErrPayload {
        kind: kind.to_string(),
        message: message.into(),
        ..Default::default()
    })
}

#[derive(Debug, Serialize, Clone)]
pub struct StaticInfo {
    pub hostname: String,
    pub os_pretty: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub mem_total_kb: u64,
    pub uptime_secs: u64,
    pub disks: Vec<DiskUsage>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiskUsage {
    pub mount: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub use_pct: f64,
}

pub struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub policy: String,
    /// Which known_hosts file to trust against (injected so tests stay hermetic).
    pub known_hosts_path: std::path::PathBuf,
    pub outcome: Arc<StdMutex<Option<hostkey::VerifyOutcome>>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let pubkey = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => cert.public_key().clone().into(),
        };
        let outcome = hostkey::verify(
            &self.host,
            self.port,
            &pubkey,
            &self.policy,
            &self.known_hosts_path,
        );
        let accept = outcome.is_accepted();
        log::info!(
            "主机密钥 {}:{} 策略={} 结果={:?} 接受={}",
            self.host,
            self.port,
            self.policy,
            outcome,
            accept
        );
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(outcome);
        }
        Ok(accept)
    }
}

/// Commands sent to the PTY pump task (owns the interactive channel).
pub enum PtyCmd {
    Data(Vec<u8>),
    Resize(u32, u32),
}

pub struct Session {
    pub handle: Arc<client::Handle<ClientHandler>>,
    pub cmd_tx: mpsc::UnboundedSender<PtyCmd>,
    pub info: SessionInfo,
    pub closed: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub sid: SessionId,
    pub host_id: Option<String>,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub connected_at: i64,
    pub closed: bool,
}

#[derive(Default)]
pub struct SessionManager {
    pub sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConnectParams {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    /// Inline PEM/OpenSSH private key content.
    #[serde(default)]
    pub private_key: Option<String>,
    /// Path to a private key file (read server-side).
    #[serde(default)]
    pub key_path: Option<String>,
    /// Passphrase for an encrypted private key.
    #[serde(default)]
    pub key_passphrase: Option<String>,
    /// The user has just approved this host's key (TOFU).
    #[serde(default)]
    pub accept_host_key: bool,
    /// Per-host override: "strict" | "accept_any".
    #[serde(default)]
    pub host_key_policy: Option<String>,
    /// Where this session came from, so the UI can offer "reconnect".
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

impl ConnectParams {
    fn effective_port(&self) -> u16 {
        self.port.unwrap_or(22)
    }
    fn policy(&self) -> String {
        if self.accept_host_key {
            "accept_new".to_string()
        } else {
            self.host_key_policy
                .clone()
                .unwrap_or_else(|| "strict".to_string())
        }
    }
}

#[tauri::command]
pub async fn connect(
    params: ConnectParams,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> std::result::Result<SessionId, String> {
    do_connect(params, state.inner(), app).await
}

/// Connect using a stored host entry; the secret comes from the OS credential
/// store unless the caller supplies one explicitly.
#[tauri::command]
pub async fn connect_host(
    host_id: String,
    password: Option<String>,
    key_passphrase: Option<String>,
    accept_host_key: bool,
    save_password: Option<bool>,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> std::result::Result<SessionId, String> {
    let Some(host) = store::get_host(&host_id) else {
        return Err(err_kind("host_missing", "主机记录不存在"));
    };

    let mut params = ConnectParams {
        host: host.host.clone(),
        port: Some(host.port),
        username: host.username.clone(),
        password: None,
        private_key: None,
        key_path: host.key_path.clone(),
        key_passphrase,
        accept_host_key,
        host_key_policy: host.host_key_policy.clone(),
        host_id: Some(host.id.clone()),
        label: Some(host.name.clone()),
    };

    match host.auth.as_str() {
        "key" => {
            if let Some(p) = params.key_path.as_ref() {
                if !p.trim().is_empty() {
                    match std::fs::read_to_string(p) {
                        Ok(content) => params.private_key = Some(content),
                        Err(e) => {
                            return Err(err_payload(ErrPayload {
                                kind: "key_unreadable".into(),
                                message: format!("读取私钥文件失败: {}", e),
                                host_id: Some(host.id.clone()),
                                ..Default::default()
                            }))
                        }
                    }
                }
            }
        }
        _ => {
            let pw = password.or_else(|| {
                if host.save_password {
                    store::get_secret(&host.id).ok()
                } else {
                    None
                }
            });
            let Some(pw) = pw else {
                return Err(err_payload(ErrPayload {
                    kind: "need_password".into(),
                    message: "需要密码".into(),
                    host_id: Some(host.id.clone()),
                    host: Some(host.host.clone()),
                    port: Some(host.port),
                    ..Default::default()
                }));
            };
            params.password = Some(pw);
        }
    }

    if save_password == Some(true) {
        if let Some(pw) = params.password.as_ref() {
            if let Err(e) = store::set_secret(&host.id, pw) {
                log::warn!("保存密码失败: {:#}", e);
            } else {
                let mut h = host.clone();
                h.save_password = true;
                let _ = store::upsert_host(h);
            }
        }
    }

    let sid = do_connect(params, state.inner(), app).await?;
    store::mark_used(&host_id);
    Ok(sid)
}

async fn do_connect(
    params: ConnectParams,
    state: &SessionManager,
    app: AppHandle,
) -> std::result::Result<SessionId, String> {
    let port = params.effective_port();
    let policy = params.policy();
    let outcome_slot: Arc<StdMutex<Option<hostkey::VerifyOutcome>>> = Arc::new(StdMutex::new(None));

    let handler = ClientHandler {
        host: params.host.clone(),
        port,
        policy: policy.clone(),
        known_hosts_path: store::known_hosts_path(),
        outcome: outcome_slot.clone(),
    };

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..<_>::default()
    });

    let addr = (params.host.as_str(), port);
    let mut handle = match client::connect(config, addr, handler).await {
        Ok(h) => h,
        Err(e) => {
            // A refused host key is the interesting case: report it precisely.
            let outcome = outcome_slot.lock().ok().and_then(|o| o.clone());
            match outcome {
                Some(hostkey::VerifyOutcome::Unknown {
                    fingerprint,
                    key_type,
                }) => {
                    return Err(err_payload(ErrPayload {
                        kind: "host_key_unknown".into(),
                        message: format!(
                            "首次连接 {}:{}，服务器密钥指纹尚未记录",
                            params.host, port
                        ),
                        host: Some(params.host.clone()),
                        port: Some(port),
                        key_type: Some(key_type),
                        fingerprint: Some(fingerprint),
                        host_id: params.host_id.clone(),
                        ..Default::default()
                    }));
                }
                Some(hostkey::VerifyOutcome::Changed {
                    fingerprint,
                    expected,
                    key_type,
                }) => {
                    return Err(err_payload(ErrPayload {
                        kind: "host_key_changed".into(),
                        message: format!(
                            "警告：{}:{} 的主机密钥与上次记录不一致，可能存在中间人攻击",
                            params.host, port
                        ),
                        host: Some(params.host.clone()),
                        port: Some(port),
                        key_type: Some(key_type),
                        fingerprint: Some(fingerprint),
                        expected_fingerprint: Some(expected),
                        host_id: params.host_id.clone(),
                        ..Default::default()
                    }));
                }
                _ => {}
            }
            return Err(err_payload(ErrPayload {
                kind: "connect_failed".into(),
                message: format!("无法连接到 {}:{} ({})", params.host, port, e),
                host: Some(params.host.clone()),
                port: Some(port),
                host_id: params.host_id.clone(),
                ..Default::default()
            }));
        }
    };

    // --- Authentication ---
    log::info!(
        "[连接 {}:{}] TCP+握手完成，开始认证 user={} 方式={}",
        params.host,
        port,
        params.username,
        if params.password.is_some() {
            "password"
        } else {
            "publickey"
        }
    );
    let auth_res = if let Some(pw) = params.password.as_ref().filter(|p| !p.is_empty()) {
        handle
            .authenticate_password(&params.username, pw)
            .await
            .map_err(|e| {
                err_payload(ErrPayload {
                    kind: "auth_error".into(),
                    message: format!("密码认证请求失败: {}", e),
                    host: Some(params.host.clone()),
                    port: Some(port),
                    host_id: params.host_id.clone(),
                    ..Default::default()
                })
            })?
    } else if params.private_key.is_some() || params.key_path.is_some() {
        let pem = match params.private_key.clone() {
            Some(p) => p,
            None => std::fs::read_to_string(params.key_path.clone().unwrap_or_default())
                .map_err(|e| err_kind("key_unreadable", format!("读取私钥文件失败: {}", e)))?,
        };
        let mut key_pair = PrivateKey::from_openssh(pem.trim()).map_err(|e| {
            err_kind(
                "key_invalid",
                format!("私钥格式无效(仅支持 OpenSSH/PEM 格式): {}", e),
            )
        })?;
        if key_pair.is_encrypted() {
            let Some(pass) = params.key_passphrase.as_ref().filter(|p| !p.is_empty()) else {
                return Err(err_payload(ErrPayload {
                    kind: "need_passphrase".into(),
                    message: "私钥已加密，请输入口令".into(),
                    host: Some(params.host.clone()),
                    port: Some(port),
                    host_id: params.host_id.clone(),
                    ..Default::default()
                }));
            };
            key_pair = key_pair
                .decrypt(pass)
                .map_err(|e| err_kind("key_passphrase_wrong", format!("私钥口令错误: {}", e)))?;
        }
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .map_err(|e| err_kind("auth_error", format!("协商 RSA 签名算法失败: {}", e)))?
            .flatten();
        handle
            .authenticate_publickey(
                &params.username,
                PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg),
            )
            .await
            .map_err(|e| err_kind("auth_error", format!("私钥认证请求失败: {}", e)))?
    } else {
        return Err(err_kind("need_password", "需要提供密码或私钥"));
    };

    if !auth_res.success() {
        return Err(err_payload(ErrPayload {
            kind: "auth_failed".into(),
            message: "认证被服务器拒绝(检查用户名/密码/私钥)".into(),
            host: Some(params.host.clone()),
            port: Some(port),
            host_id: params.host_id.clone(),
            ..Default::default()
        }));
    }

    // --- PTY channel ---
    log::info!(
        "[连接 {}:{}] 认证结果 success={}",
        params.host,
        port,
        auth_res.success()
    );
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| err_kind("pty_failed", format!("打开会话通道失败: {}", e)))?;
    log::info!("[连接 {}:{}] 会话通道已打开，请求 PTY", params.host, port);
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await
        .map_err(|e| err_kind("pty_failed", format!("请求 PTY 失败: {}", e)))?;
    log::info!("[连接 {}:{}] PTY 已请求，启动 shell", params.host, port);
    channel
        .request_shell(true)
        .await
        .map_err(|e| err_kind("pty_failed", format!("启动 shell 失败: {}", e)))?;
    log::info!("[连接 {}:{}] shell 请求已发送", params.host, port);

    let sid = uuid::Uuid::new_v4().to_string();
    let closed = Arc::new(AtomicBool::new(false));

    let info = SessionInfo {
        sid: sid.clone(),
        host_id: params.host_id.clone(),
        label: params
            .label
            .clone()
            .unwrap_or_else(|| format!("{}@{}", params.username, params.host)),
        host: params.host.clone(),
        port,
        username: params.username.clone(),
        connected_at: chrono::Utc::now().timestamp(),
        closed: false,
    };

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PtyCmd>();
    {
        let app2 = app.clone();
        let sid2 = sid.clone();
        let closed2 = closed.clone();
        let ev_host_id = info.host_id.clone();
        let ev_label = info.label.clone();
        tokio::spawn(async move {
            let emit = |data: &[u8]| {
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
                let _ = app2.emit(
                    "ssh://data",
                    serde_json::json!({ "sid": sid2, "data": b64 }),
                );
            };
            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            PtyCmd::Data(bytes) => {
                                if channel.data(&bytes[..]).await.is_err() { break; }
                            }
                            PtyCmd::Resize(cols, rows) => {
                                let _ = channel.window_change(cols, rows, 0, 0).await;
                            }
                        }
                    }
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { ref data }) => emit(&data[..]),
                            Some(ChannelMsg::ExtendedData { ref data, ext: 1 }) => emit(&data[..]),
                            Some(ChannelMsg::Eof) | Some(ChannelMsg::ExitStatus { .. }) => {}
                            Some(ChannelMsg::Close) | None => break,
                            _ => {}
                        }
                    }
                }
            }
            closed2.store(true, Ordering::SeqCst);
            let _ = app2.emit(
                "ssh://closed",
                serde_json::json!({ "sid": sid2, "host_id": ev_host_id, "label": ev_label }),
            );
        });
    }

    let handle = Arc::new(handle);
    let session = Session {
        handle: handle.clone(),
        cmd_tx,
        info: info.clone(),
        closed: closed.clone(),
    };
    state
        .sessions
        .lock()
        .await
        .insert(sid.clone(), Arc::new(session));

    // Monitor shares this SSH connection (separate exec channels).
    monitor::spawn(sid.clone(), handle, closed, app.clone());

    Ok(sid)
}

#[tauri::command]
pub async fn term_write(
    sid: SessionId,
    data: String,
    state: State<'_, SessionManager>,
) -> std::result::Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&sid)
        .ok_or_else(|| err_kind("no_session", "会话不存在"))?;
    session
        .cmd_tx
        .send(PtyCmd::Data(data.into_bytes()))
        .map_err(|_| err_kind("session_closed", "会话已关闭"))
}

#[tauri::command]
pub async fn term_resize(
    sid: SessionId,
    cols: u32,
    rows: u32,
    state: State<'_, SessionManager>,
) -> std::result::Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&sid)
        .ok_or_else(|| err_kind("no_session", "会话不存在"))?;
    session
        .cmd_tx
        .send(PtyCmd::Resize(cols, rows))
        .map_err(|_| err_kind("session_closed", "会话已关闭"))
}

#[tauri::command]
pub async fn disconnect(
    sid: SessionId,
    state: State<'_, SessionManager>,
) -> std::result::Result<(), String> {
    let mut sessions = state.sessions.lock().await;
    if let Some(session) = sessions.remove(&sid) {
        let _ = session
            .handle
            .disconnect(Disconnect::ByApplication, "user closed", "en");
    }
    Ok(())
}

#[tauri::command]
pub async fn list_sessions(
    state: State<'_, SessionManager>,
) -> std::result::Result<Vec<SessionInfo>, String> {
    let sessions = state.sessions.lock().await;
    Ok(sessions
        .values()
        .map(|s| {
            let mut i = s.info.clone();
            i.closed = s.closed.load(Ordering::SeqCst);
            i
        })
        .collect())
}

#[tauri::command]
pub async fn session_alive(
    sid: SessionId,
    state: State<'_, SessionManager>,
) -> std::result::Result<bool, String> {
    let sessions = state.sessions.lock().await;
    Ok(sessions
        .get(&sid)
        .map(|s| !s.closed.load(Ordering::SeqCst))
        .unwrap_or(false))
}

#[tauri::command]
pub fn monitor_set_visible(sid: SessionId, visible: bool) -> std::result::Result<(), String> {
    monitor::set_visible(&sid, visible);
    Ok(())
}

/// Pause/resume the monitor task. Resuming triggers an immediate sample.
#[tauri::command]
pub fn monitor_set_paused(sid: SessionId, paused: bool) -> std::result::Result<(), String> {
    monitor::set_paused(&sid, paused);
    Ok(())
}

#[tauri::command]
pub fn monitor_paused(sid: SessionId) -> bool {
    monitor::is_paused(&sid)
}

/// Collect one sample right now instead of waiting out the interval.
#[tauri::command]
pub fn monitor_sample_now(sid: SessionId) -> std::result::Result<(), String> {
    monitor::sample_now(&sid);
    Ok(())
}

/// Restart the monitor task of an existing session (used after a settings change).
#[tauri::command]
pub async fn monitor_restart(
    sid: SessionId,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> std::result::Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&sid)
        .ok_or_else(|| err_kind("no_session", "会话不存在"))?;
    monitor::stop(&sid);
    monitor::spawn(
        sid.clone(),
        session.handle.clone(),
        session.closed.clone(),
        app,
    );
    Ok(())
}
