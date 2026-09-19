//! 端口转发：本地转发（-L）与远程转发（-R）。
//!
//! - 本地转发：本机 `TcpListener` 收到连接 → 在 SSH 连接上开 direct-tcpip 通道
//!   → `copy_bidirectional` 双向桥接。目标由**远端**解析（所以能连到远端内网）。
//! - 远程转发：请求远端监听（`tcpip_forward`），远端有连接进来时通过
//!   `server_channel_open_forwarded_tcpip` 回调把通道交给我们，我们再连**本机**目标。
//!   回调里只有"连到了哪个地址端口"，要转发到本机哪里得靠一张路由表（见 [`ForwardTable`]）。
//!
//! 规则持久化在 `hosts.json` 的 host 上（跟主机一起导入导出），运行态只存内存。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};

use crate::ssh::{err_kind, SessionManager};

/// 远程转发最终用哪个端口号做路由表键。
///
/// RFC 4254 规定：客户端请求了非 0 端口时，服务端接受后**回复里不带端口**
/// （russh 于是返回 0）。早先直接拿返回值当端口，结果路由表键变成
/// `(host, 0)`，远端真的连到 13400 时查表必然落空、被拒——表现为
/// “远端能连上但立刻 connection reset”。
fn resolve_listen_port(requested: u16, reported: u32) -> u32 {
    if requested != 0 {
        requested as u32
    } else {
        reported
    }
}

/// 远程转发的路由表：(远端监听地址, 端口) → "本机目标 host:port"。
///
/// 每个 SSH 连接一张（`Session` 和 `ClientHandler` 共享同一个 `Arc`），
/// 因为同一台主机可能开多个会话、各自的转发互不干扰。
pub type ForwardTable = Arc<StdMutex<HashMap<(String, u32), String>>>;

/// 按 (地址, 端口) 查目标；查不到再退化到"只看端口"。
///
/// 服务端回调里的 `connected_address` 不保证和我们请求时写的字符串一致
/// （`localhost` / `127.0.0.1` / `0.0.0.0` 各种写法），只比端口能少一类玄学失败。
pub fn forward_target(table: &ForwardTable, addr: &str, port: u32) -> Option<String> {
    let map = table.lock().ok()?;
    if let Some(t) = map.get(&(addr.to_string(), port)) {
        return Some(t.clone());
    }
    map.iter()
        .find(|((_, p), _)| *p == port)
        .map(|(_, t)| t.clone())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRule {
    pub id: String,
    /// "local"（本机监听 → 远端目标）| "remote"（远端监听 → 本机目标）
    pub kind: String,
    pub listen_host: String,
    pub listen_port: u16,
    pub target_host: String,
    pub target_port: u16,
    /// 连上主机后自动启动
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardStatus {
    pub id: String,
    pub kind: String,
    pub listen_host: String,
    pub listen_port: u16,
    pub target_host: String,
    pub target_port: u16,
    pub auto_start: bool,
    pub label: Option<String>,
    pub running: bool,
    /// 远端分配的实际端口（本地转发=监听端口；远程转发可能被服务端改写）
    pub actual_port: u16,
    pub error: Option<String>,
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    actual_port: u16,
    error: Option<String>,
}

#[derive(Default)]
pub struct ForwardManager {
    /// key = "{sid}:{rule_id}"
    running: Mutex<HashMap<String, Running>>,
}

impl ForwardManager {
    fn key(sid: &str, id: &str) -> String {
        format!("{sid}:{id}")
    }
}

fn status_of(rule: &ForwardRule, rt: Option<&Running>) -> ForwardStatus {
    ForwardStatus {
        id: rule.id.clone(),
        kind: rule.kind.clone(),
        listen_host: rule.listen_host.clone(),
        listen_port: rule.listen_port,
        target_host: rule.target_host.clone(),
        target_port: rule.target_port,
        auto_start: rule.auto_start,
        label: rule.label.clone(),
        running: rt.map(|r| r.stop.is_some()).unwrap_or(false),
        actual_port: rt.map(|r| r.actual_port).unwrap_or(rule.listen_port),
        error: rt.and_then(|r| r.error.clone()),
    }
}

async fn session_of(
    sid: &str,
    sessions: &SessionManager,
) -> Result<Arc<crate::ssh::Session>, String> {
    let map = sessions.sessions.lock().await;
    map.get(sid)
        .cloned()
        .ok_or_else(|| err_kind("no_session", "会话不存在"))
}

/// 会话对应的主机 id。转发规则挂在 host 上，所以没有 host_id 就没法持久化。
async fn host_id_of(sid: &str, sessions: &SessionManager) -> Result<String, String> {
    let map = sessions.sessions.lock().await;
    map.get(sid)
        .and_then(|s| s.info.host_id.clone())
        .ok_or_else(|| err_kind("no_host", "这条会话不是从主机列表连的，转发规则无法保存"))
}

fn rule_of(host_id: &str, rule_id: &str) -> Result<ForwardRule, String> {
    crate::store::forwards_of(host_id)
        .into_iter()
        .find(|r| r.id == rule_id)
        .ok_or_else(|| err_kind("no_rule", "规则不存在"))
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn forward_list(
    sid: String,
    mgr: State<'_, ForwardManager>,
    sessions: State<'_, SessionManager>,
) -> Result<Vec<ForwardStatus>, String> {
    let host_id = host_id_of(&sid, &sessions).await?;
    let rules = crate::store::forwards_of(&host_id);
    let running = mgr.running.lock().await;
    Ok(rules
        .iter()
        .map(|r| status_of(r, running.get(&ForwardManager::key(&sid, &r.id))))
        .collect())
}

/// 新增/更新一条规则（写进 hosts.json）。
#[tauri::command]
pub async fn forward_save(
    sid: String,
    rule: ForwardRule,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let host_id = host_id_of(&sid, &sessions).await?;
    crate::store::save_forward(&host_id, rule)
        .map_err(|err| err_kind("store_error", format!("{err:#}")))
}

#[tauri::command]
pub async fn forward_delete(
    sid: String,
    id: String,
    mgr: State<'_, ForwardManager>,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let host_id = host_id_of(&sid, &sessions).await?;
    let _ = forward_stop(sid.clone(), id.clone(), mgr, sessions).await;
    crate::store::delete_forward(&host_id, &id)
        .map_err(|err| err_kind("store_error", format!("{err:#}")))
}

#[tauri::command]
pub async fn forward_start(
    sid: String,
    rule: ForwardRule,
    mgr: State<'_, ForwardManager>,
    sessions: State<'_, SessionManager>,
) -> Result<u16, String> {
    let session = session_of(&sid, &sessions).await?;
    let key = ForwardManager::key(&sid, &rule.id);
    // 已经在跑就先停掉，避免同一个端口被绑两次
    if let Some(mut old) = mgr.running.lock().await.remove(&key) {
        if let Some(stop) = old.stop.take() {
            let _ = stop.send(());
        }
    }

    let handle = session.handle.clone();
    let closed = session.closed.clone();

    match rule.kind.as_str() {
        "local" => {
            // 先绑端口：占用/权限问题在这一步就报出来，不用等到有人连
            let listener = TcpListener::bind((rule.listen_host.as_str(), rule.listen_port))
                .await
                .map_err(|err| {
                    err_kind(
                        "port_in_use",
                        format!(
                            "本机监听 {}:{} 失败：{err}（端口被占用，或 <1024 需要管理员权限）",
                            rule.listen_host, rule.listen_port
                        ),
                    )
                })?;
            let actual = listener
                .local_addr()
                .map(|a| a.port())
                .unwrap_or(rule.listen_port);
            let (tx, mut rx) = oneshot::channel::<()>();
            let target_host = rule.target_host.clone();
            let target_port = rule.target_port as u32;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut rx => break,
                        // 会话断了就自己退出，别留一个没人管的监听端口
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {
                            if closed.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        acc = listener.accept() => {
                            let Ok((sock, peer)) = acc else { continue };
                            let h = handle.clone();
                            let (th, tp) = (target_host.clone(), target_port);
                            tokio::spawn(async move {
                                match h
                                    .channel_open_direct_tcpip(th.clone(), tp, peer.ip().to_string(), peer.port() as u32)
                                    .await
                                {
                                    Ok(ch) => {
                                        let mut stream = ch.into_stream();
                                        let mut sock = sock;
                                        let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
                                    }
                                    Err(err) => log::warn!(
                                        "本地转发打开到 {th}:{tp} 的通道失败：{err}"
                                    ),
                                }
                            });
                        }
                    }
                }
                log::info!("本地转发已停止");
            });
            mgr.running.lock().await.insert(
                key,
                Running {
                    stop: Some(tx),
                    actual_port: actual,
                    error: None,
                },
            );
            Ok(actual)
        }
        "remote" => {
            let reported = handle
                .tcpip_forward(rule.listen_host.clone(), rule.listen_port as u32)
                .await
                .map_err(|err| {
                    err_kind(
                        "forward_failed",
                        format!(
                            "请求远端监听 {}:{} 失败：{err}（远端可能禁止 GatewayPorts）",
                            rule.listen_host, rule.listen_port
                        ),
                    )
                })?;
            let port = resolve_listen_port(rule.listen_port, reported);
            // 注册路由表：远端有人连进来时靠它找到本机目标
            let target = format!("{}:{}", rule.target_host, rule.target_port);
            if let Ok(mut table) = session.forwards.lock() {
                table.insert((rule.listen_host.clone(), port), target.clone());
                table.insert(("0.0.0.0".to_string(), port), target.clone());
                table.insert(("localhost".to_string(), port), target.clone());
                table.insert(("127.0.0.1".to_string(), port), target);
            }
            let (tx, rx) = oneshot::channel::<()>();
            let h = handle.clone();
            let addr = rule.listen_host.clone();
            let table = session.forwards.clone();
            tokio::spawn(async move {
                let _ = rx.await;
                let _ = h.cancel_tcpip_forward(addr.clone(), port).await;
                if let Ok(mut t) = table.lock() {
                    t.retain(|(_, p), _| *p != port);
                }
                log::info!("远程转发 {}:{} 已取消", addr, port);
            });
            mgr.running.lock().await.insert(
                key,
                Running {
                    stop: Some(tx),
                    actual_port: port as u16,
                    error: None,
                },
            );
            Ok(port as u16)
        }
        other => Err(err_kind("bad_rule", format!("未知的转发类型：{other}"))),
    }
}

#[tauri::command]
pub async fn forward_stop(
    sid: String,
    id: String,
    mgr: State<'_, ForwardManager>,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let key = ForwardManager::key(&sid, &id);
    let removed = mgr.running.lock().await.remove(&key);
    if let Some(mut rt) = removed {
        if let Some(stop) = rt.stop.take() {
            let _ = stop.send(());
        }
    }
    // 远程转发要顺手清掉路由表项（cancel_tcpip_forward 由 stop 任务负责）
    if let Ok(host_id) = host_id_of(&sid, &sessions).await {
        if let Ok(rule) = rule_of(&host_id, &id) {
            if rule.kind == "remote" {
                if let Ok(session) = session_of(&sid, &sessions).await {
                    if let Ok(mut table) = session.forwards.lock() {
                        table.retain(|(_, p), _| *p != rule.listen_port as u32);
                    }
                }
            }
        }
    }
    Ok(())
}

/// 关标签/断线时调用：把这个会话的所有转发停掉，别留悬空的监听端口。
#[tauri::command]
pub async fn forward_stop_all(
    sid: String,
    mgr: State<'_, ForwardManager>,
    sessions: State<'_, SessionManager>,
) -> Result<usize, String> {
    let prefix = format!("{sid}:");
    let keys: Vec<String> = mgr
        .running
        .lock()
        .await
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    let n = keys.len();
    for key in keys {
        if let Some(mut rt) = mgr.running.lock().await.remove(&key) {
            if let Some(stop) = rt.stop.take() {
                let _ = stop.send(());
            }
        }
    }
    if let Ok(session) = session_of(&sid, &sessions).await {
        if let Ok(mut table) = session.forwards.lock() {
            table.clear();
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with(pairs: &[(&str, u32, &str)]) -> ForwardTable {
        let mut m = HashMap::new();
        for (a, p, t) in pairs {
            m.insert((a.to_string(), *p), t.to_string());
        }
        Arc::new(StdMutex::new(m))
    }

    #[test]
    fn exact_address_match_wins() {
        let t = table_with(&[("localhost", 8080, "127.0.0.1:80")]);
        assert_eq!(
            forward_target(&t, "localhost", 8080).unwrap(),
            "127.0.0.1:80"
        );
    }

    #[test]
    fn falls_back_to_port_when_server_reports_another_address() {
        // 服务端可能回报 127.0.0.1 而请求写的是 localhost —— 只比端口也要能找到
        let t = table_with(&[("localhost", 8080, "127.0.0.1:80")]);
        assert_eq!(
            forward_target(&t, "127.0.0.1", 8080).unwrap(),
            "127.0.0.1:80"
        );
    }

    #[test]
    fn unknown_port_is_none() {
        let t = table_with(&[("localhost", 8080, "127.0.0.1:80")]);
        assert!(forward_target(&t, "localhost", 9999).is_none());
    }

    #[test]
    fn status_reports_stopped_when_not_running() {
        let rule = ForwardRule {
            id: "r1".into(),
            kind: "local".into(),
            listen_host: "127.0.0.1".into(),
            listen_port: 13306,
            target_host: "127.0.0.1".into(),
            target_port: 3306,
            auto_start: false,
            label: None,
        };
        let st = status_of(&rule, None);
        assert!(!st.running);
        assert_eq!(st.actual_port, 13306);
        assert!(st.error.is_none());
    }
}

/// 真机测试：本地转发能不能真的把流量送到远端。
///
/// ```bash
/// SSHBOX_TEST_HOST=127.0.0.1 SSHBOX_TEST_PASSWORD=sshbox123 \
///   cargo test --lib live_forward -- --nocapture --test-threads=1
/// ```
///
/// 目标选 VM 自己的 22 端口：不依赖 VM 上装任何服务，而 SSH banner 是确定性的，
/// 读得到 banner 就说明"本机监听 → direct-tcpip → 远端连接 → 双向数据"整条链路通了。
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::ssh::ClientHandler;
    use russh::client;
    use tokio::io::AsyncReadExt;

    fn target() -> Option<(String, u16, String, String)> {
        let host = std::env::var("SSHBOX_TEST_HOST").ok()?;
        let port = std::env::var("SSHBOX_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22);
        let user = std::env::var("SSHBOX_TEST_USER").unwrap_or_else(|_| "root".to_string());
        let pw = std::env::var("SSHBOX_TEST_PASSWORD").ok()?;
        Some((host, port, user, pw))
    }

    #[test]
    fn remote_forward_port_uses_request_when_server_omits_it() {
        // 请求了非 0 端口 → 服务端回复不带端口，russh 返回 0，必须用请求值
        assert_eq!(resolve_listen_port(13400, 0), 13400);
        // 请求 0（让系统分配）→ 只能用服务端回复的值
        assert_eq!(resolve_listen_port(0, 45678), 45678);
        // 服务端确实回了端口 → 以回复为准
        assert_eq!(resolve_listen_port(13400, 13400), 13400);
    }

    #[tokio::test]
    async fn live_local_forward_reaches_remote_service() {
        let Some((host, port, user, pw)) = target() else {
            eprintln!("跳过 live 测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        let kh = std::env::temp_dir().join(format!("sshbox-fwd-kh-{}", std::process::id()));
        let _ = std::fs::remove_file(&kh);
        let handler = ClientHandler {
            host: host.clone(),
            port,
            policy: "accept_new".to_string(),
            known_hosts_path: kh.clone(),
            outcome: Default::default(),
            forwards: Default::default(),
        };
        let mut h = client::connect(
            Arc::new(client::Config::default()),
            (host.as_str(), port),
            handler,
        )
        .await
        .expect("连接 VM");
        assert!(
            h.authenticate_password(&user, &pw)
                .await
                .expect("认证")
                .success(),
            "认证失败"
        );
        let handle = Arc::new(h);

        // 本机监听（端口 0 = 让系统分配），收到连接就开 direct-tcpip 通道桥接
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("本机监听");
        let local_port = listener.local_addr().unwrap().port();
        let bridge = handle.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, peer)) = listener.accept().await else {
                    break;
                };
                let h = bridge.clone();
                tokio::spawn(async move {
                    match h
                        .channel_open_direct_tcpip(
                            "127.0.0.1",
                            port as u32,
                            peer.ip().to_string(),
                            peer.port() as u32,
                        )
                        .await
                    {
                        Ok(ch) => {
                            let mut stream = ch.into_stream();
                            let mut sock = sock;
                            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
                        }
                        Err(err) => eprintln!("开通道失败：{err}"),
                    }
                });
            }
        });

        // 从本机连转发端口，读远端 sshd 的 banner
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .expect("连本机转发端口");
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
            .await
            .expect("转发读超时")
            .expect("读 banner");
        let banner = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            banner.starts_with("SSH-2.0-"),
            "应拿到远端 sshd 的 banner，实际：{banner:?}"
        );

        // 再来一条并发连接，确认监听循环不是"只接一次就废"
        let mut c2 = tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .expect("第二条连接");
        let mut buf2 = [0u8; 64];
        let n2 = tokio::time::timeout(Duration::from_secs(10), c2.read(&mut buf2))
            .await
            .expect("第二条读超时")
            .expect("读 banner");
        assert!(String::from_utf8_lossy(&buf2[..n2]).starts_with("SSH-2.0-"));

        task.abort();
        let _ = std::fs::remove_file(&kh);
        println!(
            "✅ 本地转发 127.0.0.1:{local_port} → VM:{port} 打通（两条并发连接都读到 banner：{}）",
            banner.trim()
        );
    }
}
