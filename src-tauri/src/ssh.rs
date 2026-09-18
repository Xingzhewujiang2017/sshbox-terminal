use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::client;
use russh::{ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, Mutex};

use crate::monitor;

pub type SessionId = String;

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

pub struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // TODO(M6): host key fingerprint verification / known_hosts
        Ok(true)
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
    pub host: String,
    pub username: String,
    pub closed: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct SessionManager {
    pub sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub private_key: Option<String>, // PEM content
    pub key_passphrase: Option<String>,
}

#[tauri::command]
pub async fn connect(
    params: ConnectParams,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> std::result::Result<SessionId, String> {
    do_connect(params, state.inner(), app).await.map_err(|e| format!("{:#}", e))
}

async fn do_connect(
    params: ConnectParams,
    state: &SessionManager,
    app: AppHandle,
) -> Result<SessionId> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..<_>::default()
    });

    let addr = (params.host.as_str(), params.port);
    let mut handle = client::connect(config, addr, ClientHandler)
        .await
        .with_context(|| format!("无法连接到 {}:{}", params.host, params.port))?;

    // Authenticate
    let auth_res = if let Some(pw) = &params.password {
        handle
            .authenticate_password(&params.username, pw)
            .await
            .context("密码认证请求失败")?
    } else if let Some(key_pem) = &params.private_key {
        let key_pair = russh::keys::ssh_key::PrivateKey::from_openssh(key_pem)
            .context("私钥格式无效(仅支持 OpenSSH 格式)")?;
        let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
        handle
            .authenticate_publickey(
                &params.username,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg),
            )
            .await
            .context("私钥认证请求失败")?
    } else {
        bail!("需要提供密码或私钥");
    };
    if !auth_res.success() {
        bail!("认证被服务器拒绝(检查用户名/密码/私钥)");
    }

    // Open PTY channel
    let mut channel = handle
        .channel_open_session()
        .await
        .context("打开会话通道失败")?;
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;

    let sid = uuid::Uuid::new_v4().to_string();
    let closed = Arc::new(AtomicBool::new(false));

    // PTY pump task: owns the channel; forwards output to frontend events and
    // accepts Data/Resize commands.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PtyCmd>();
    {
        let app2 = app.clone();
        let sid2 = sid.clone();
        let closed2 = closed.clone();
        tokio::spawn(async move {
            let emit = |data: &[u8]| {
                let b64 =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
                let _ = app2.emit("ssh://data", serde_json::json!({ "sid": sid2, "data": b64 }));
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
            let _ = app2.emit("ssh://closed", serde_json::json!({ "sid": sid2 }));
        });
    }

    let handle = Arc::new(handle);
    let session = Session {
        handle: handle.clone(),
        cmd_tx,
        host: params.host.clone(),
        username: params.username.clone(),
        closed: closed.clone(),
    };
    state
        .sessions
        .lock()
        .await
        .insert(sid.clone(), Arc::new(session));

    // Spawn monitor task (shares the same SSH connection)
    monitor::spawn(
        sid.clone(),
        handle,
        closed,
        app.clone(),
        Duration::from_secs(2),
    );

    Ok(sid)
}

#[tauri::command]
pub async fn term_write(
    sid: SessionId,
    data: String,
    state: State<'_, SessionManager>,
) -> std::result::Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions.get(&sid).ok_or("会话不存在")?;
    session
        .cmd_tx
        .send(PtyCmd::Data(data.into_bytes()))
        .map_err(|_| "会话已关闭".to_string())
}

#[tauri::command]
pub async fn term_resize(
    sid: SessionId,
    cols: u32,
    rows: u32,
    state: State<'_, SessionManager>,
) -> std::result::Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions.get(&sid).ok_or("会话不存在")?;
    session
        .cmd_tx
        .send(PtyCmd::Resize(cols, rows))
        .map_err(|_| "会话已关闭".to_string())
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
) -> std::result::Result<Vec<String>, String> {
    let sessions = state.sessions.lock().await;
    Ok(sessions.keys().cloned().collect())
}

#[tauri::command]
pub fn monitor_set_visible(sid: SessionId, visible: bool) -> std::result::Result<(), String> {
    monitor::set_visible(&sid, visible);
    Ok(())
}
