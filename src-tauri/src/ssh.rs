use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::client;
use russh::{ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

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
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO(M6): host key fingerprint verification / known_hosts
        Ok(true)
    }
}

pub struct Session {
    pub handle: client::Handle<ClientHandler>,
    pub pty_channel_id: russh::ChannelId,
    pub host: String,
    pub username: String,
    pub closed: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct SessionManager {
    pub sessions: Mutex<HashMap<SessionId, Arc<Mutex<Session>>>>,
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

fn err_str(e: &anyhow::Error) -> String {
    format!("{:#}", e)
}

#[tauri::command]
pub async fn connect(
    params: ConnectParams,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> Result<SessionId, String> {
    let sid = do_connect(params, state, app).await.map_err(err_str)?;
    Ok(sid)
}

async fn do_connect(
    params: ConnectParams,
    state: State<'_, SessionManager>,
    app: AppHandle,
) -> Result<SessionId> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
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
            .context("密码认证失败")?
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
            .context("私钥认证失败")?
    } else {
        bail!("需要提供密码或私钥");
    };
    if !auth_res.success() {
        bail!("认证被服务器拒绝");
    }

    // Open PTY channel
    let mut channel = handle.channel_open_session().await.context("打开会话通道失败")?;
    channel.request_pty(false, "xterm-256color", 80, 24, 0.0, 0.0, &[]).await?;
    channel.request_shell(true).await?;
    let pty_id = channel.id();

    let sid = uuid::Uuid::new_v4().to_string();
    let closed = Arc::new(AtomicBool::new(false));

    // Pump PTY output -> frontend event
    {
        let app2 = app.clone();
        let sid2 = sid.clone();
        let closed2 = closed.clone();
        tokio::spawn(async move {
            loop {
                match channel.wait().await {
                    Ok(Some(ChannelMsg::Data { ref data })) => {
                        let b64 = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &data[..],
                        );
                        let _ = app2.emit(
                            "ssh://data",
                            serde_json::json!({ "sid": sid2, "data": b64 }),
                        );
                    }
                    Ok(Some(ChannelMsg::ExtendedData { ref data, ext: 1 })) => {
                        let b64 = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &data[..],
                        );
                        let _ = app2.emit(
                            "ssh://data",
                            serde_json::json!({ "sid": sid2, "data": b64 }),
                        );
                    }
                    Ok(Some(ChannelMsg::Eof)) | Ok(Some(ChannelMsg::ExitStatus { .. })) => {}
                    Ok(Some(ChannelMsg::Close)) | Ok(None) | Err(_) => break,
                    _ => {}
                }
            }
            closed2.store(true, Ordering::SeqCst);
            let _ = app2.emit("ssh://closed", serde_json::json!({ "sid": sid2 }));
        });
    }

    let session = Session {
        handle: handle.clone(),
        pty_channel_id: pty_id,
        host: params.host.clone(),
        username: params.username.clone(),
        closed: closed.clone(),
    };
    let session_arc = Arc::new(Mutex::new(session));
    state.sessions.lock().await.insert(sid.clone(), session_arc.clone());

    // Spawn monitor task
    monitor::spawn(
        sid.clone(),
        handle.clone(),
        closed,
        app.clone(),
        Duration::from_secs(2),
    );

    Ok(sid)
}

#[tauri::command]
pub async fn term_write(sid: SessionId, data: String, state: State<'_, SessionManager>) -> Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions.get(&sid).ok_or("会话不存在")?;
    let session = session.lock().await;
    session
        .handle
        .data(session.pty_channel_id, data.into_bytes().into())
        .await
        .map_err(|e| format!("写入失败: {}", e))
}

#[tauri::command]
pub async fn term_resize(sid: SessionId, cols: u32, rows: u32, state: State<'_, SessionManager>) -> Result<(), String> {
    let sessions = state.sessions.lock().await;
    let session = sessions.get(&sid).ok_or("会话不存在")?;
    let session = session.lock().await;
    session
        .handle
        .window_change(session.pty_channel_id, cols, rows, 0, 0)
        .await
        .map_err(|e| format!("resize 失败: {}", e))
}

#[tauri::command]
pub async fn disconnect(sid: SessionId, state: State<'_, SessionManager>) -> Result<(), String> {
    let mut sessions = state.sessions.lock().await;
    if let Some(session) = sessions.remove(&sid) {
        let session = session.lock().await;
        let _ = session
            .handle
            .disconnect(Disconnect::ByApplication, "user closed", "en");
    }
    Ok(())
}

#[tauri::command]
pub fn monitor_set_visible(sid: SessionId, visible: bool) -> Result<(), String> {
    monitor::set_visible(&sid, visible);
    Ok(())
}
