//! SFTP：远端文件浏览 + 上传下载（v0.4）。
//!
//! 一条 SSH 连接懒开一个 SFTP 通道（`subsystem sftp`），复用 `Session.handle`，
//! 不动连接层。传输是独立的 tokio 任务 + 64KB 分块流式，进度事件节流后推给前端。
//!
//! 取消 = 置 `AtomicBool`，循环下一轮退出并 `shutdown()` 远端文件句柄——半截文件
//! 保留，下次可以续传（`File` 实现 `AsyncSeek`，从远端 size 处接着写）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;

use crate::ssh::{err_kind, SessionId, SessionManager};

/// 传输块大小。SFTP 单包上限通常是 32KB–256KB，64KB 是稳妥值。
const CHUNK: usize = 64 * 1024;
/// 进度节流：每 1MB 或 200ms 推一次，防止刷屏时事件把 UI 卡死。
const PROGRESS_BYTES: u64 = 1024 * 1024;
const PROGRESS_MS: u128 = 200;

fn e(err: impl std::fmt::Display) -> String {
    err_kind("sftp_error", format!("{:#}", err))
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn unix_secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// 每个会话一个懒创建的 SFTP 通道 + 在跑的传输任务表。
///
/// `Clone` 是给传输任务用的：任务里要能把完成的自己从表里摘掉。
#[derive(Default, Clone)]
pub struct SftpManager {
    sessions: Arc<Mutex<HashMap<SessionId, Arc<SftpSession>>>>,
    transfers: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl SftpManager {
    async fn acquire(
        &self,
        sid: &str,
        sessions: &SessionManager,
    ) -> Result<Arc<SftpSession>, String> {
        if let Some(s) = self.sessions.lock().await.get(sid) {
            return Ok(s.clone());
        }
        let handle = {
            let map = sessions.sessions.lock().await;
            map.get(sid)
                .ok_or_else(|| err_kind("no_session", "会话不存在"))?
                .handle
                .clone()
        };
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|err| err_kind("sftp_unavailable", format!("打开 SFTP 通道失败：{err}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|err| {
                err_kind(
                    "sftp_unavailable",
                    format!("服务器拒绝 SFTP 子系统（可能没装/没启用 sftp 服务）：{err}"),
                )
            })?;
        let session = SftpSession::new(channel.into_stream())
            .await
            .map_err(|err| err_kind("sftp_unavailable", format!("SFTP 握手失败：{err}")))?;
        let arc = Arc::new(session);
        self.sessions
            .lock()
            .await
            .insert(sid.to_string(), arc.clone());
        Ok(arc)
    }

    /// 通道坏了就摘掉缓存，下次自动重开（服务端重启 / 网络抖动后不用重启应用）。
    async fn forget(&self, sid: &str) {
        self.sessions.lock().await.remove(sid);
    }
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: i64,
    /// "rwxr-xr-x"（不含类型位，类型由 UI 按 is_dir/is_symlink 加前缀）
    pub permissions: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<RemoteEntry>,
    /// 登录后的家目录（`canonicalize(".")`）
    pub home: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteStat {
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferProgress {
    pub task_id: String,
    pub sid: String,
    /// "up" | "down"
    pub direction: String,
    pub name: String,
    pub local: String,
    pub remote: String,
    pub done: u64,
    pub total: u64,
    /// bytes/s（按上次上报的间隔算，不是全程均值）
    pub rate: f64,
    /// running | done | cancelled | error
    pub state: String,
    pub message: Option<String>,
}

fn emit(app: &AppHandle, p: &TransferProgress) {
    let _ = app.emit("sftp://progress", p);
}

// ---------------------------------------------------------------------------
// 浏览与文件操作
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sftp_list(
    sid: String,
    path: Option<String>,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<RemoteListing, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    // 家目录：SFTP 的 "." 解析成绝对路径，用它做初始目录
    let home = sftp.canonicalize(".").await.map_err(e)?;
    let dir = path
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(home.clone());

    let rd = match sftp.read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) => {
            // 通道级故障（比如服务端重启）→ 摘掉缓存让下次重开，错误照常返回
            if format!("{err:?}").contains("ChannelClosed")
                || format!("{err:?}").contains("UnexpectedEof")
            {
                mgr.forget(&sid).await;
            }
            return Err(err_kind(
                "sftp_error",
                format!("读取目录 {dir} 失败：{err}"),
            ));
        }
    };

    let mut entries: Vec<RemoteEntry> = Vec::new();
    for entry in rd {
        let md = entry.metadata();
        let name = entry.file_name();
        // "." / ".." 由 UI 用 parent 导航，不混进列表
        if name == "." || name == ".." {
            continue;
        }
        entries.push(RemoteEntry {
            path: join_path(&dir, &name),
            name,
            is_dir: md.is_dir(),
            is_symlink: md.is_symlink(),
            size: if md.is_dir() { 0 } else { md.len() },
            modified: unix_secs(md.modified()),
            permissions: md.permissions().to_string(),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let parent = if dir == "/" {
        None
    } else {
        Some(
            Path::new(&dir)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "/".to_string()),
        )
    };

    Ok(RemoteListing {
        path: dir,
        parent,
        entries,
        home,
    })
}

#[tauri::command]
pub async fn sftp_stat(
    sid: String,
    path: String,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<Option<RemoteStat>, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    match sftp.metadata(&path).await {
        Ok(md) => Ok(Some(RemoteStat {
            size: md.len(),
            modified: unix_secs(md.modified()),
            is_dir: md.is_dir(),
        })),
        Err(_) => Ok(None), // 不存在 → None（不是错误，前端用它判断冲突）
    }
}

#[tauri::command]
pub async fn sftp_mkdir(
    sid: String,
    path: String,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    sftp.create_dir(&path).await.map_err(e)
}

#[tauri::command]
pub async fn sftp_rename(
    sid: String,
    from: String,
    to: String,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    sftp.rename(&from, &to).await.map_err(e)
}

/// 递归删树：先删孩子再删自己。
///
/// 软链一律按文件删——否则软链指向目录时会顺着删到链接目标里去。
fn remove_tree<'a>(
    sftp: &'a SftpSession,
    path: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let rd = sftp.read_dir(path).await.map_err(e)?;
        for entry in rd {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = join_path(path, &name);
            let md = entry.metadata();
            if md.is_dir() && !md.is_symlink() {
                remove_tree(sftp, &child).await?;
            } else {
                sftp.remove_file(&child).await.map_err(e)?;
            }
        }
        sftp.remove_dir(path).await.map_err(e)
    })
}

#[tauri::command]
pub async fn sftp_remove(
    sid: String,
    path: String,
    is_dir: bool,
    recursive: bool,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<(), String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    let md = sftp.symlink_metadata(&path).await.map_err(e)?;
    let real_dir = md.is_dir() && !md.is_symlink();
    if is_dir && real_dir {
        if recursive {
            remove_tree(&sftp, &path).await
        } else {
            sftp.remove_dir(&path).await.map_err(|err| {
                let text = format!("{err}");
                if text.contains("Failure") || text.contains("not empty") {
                    err_kind("dir_not_empty", "目录非空（勾选“递归删除”可整目录删除）")
                } else {
                    e(err)
                }
            })
        }
    } else {
        sftp.remove_file(&path).await.map_err(e)
    }
}

/// 标签关闭时调用：丢掉这条连接的 SFTP 通道。
#[tauri::command]
pub async fn sftp_forget(sid: String, mgr: State<'_, SftpManager>) -> Result<(), String> {
    mgr.forget(&sid).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 传输
// ---------------------------------------------------------------------------

enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

/// 进度推送口子。
///
/// 刻意不直接吃 `AppHandle`：传输核心（分块/续传/取消）是这版最容易出错的
/// 地方，解耦之后就能在 live 测试里拿一个收集器当 sink，对着真机跑完整传输。
pub type Sink<'a> = &'a (dyn Fn(&TransferProgress) + Send + Sync);

/// 把一次传输的收尾统一在一处：定终态 → 推最后一帧 → 从任务表里摘掉自己。
fn finish(
    app: AppHandle,
    mgr: SftpManager,
    task_id: String,
    outcome: Outcome,
    mut p: TransferProgress,
) {
    p.state = match outcome {
        Outcome::Done => "done".to_string(),
        Outcome::Cancelled => "cancelled".to_string(),
        Outcome::Failed(msg) => {
            p.message = Some(msg);
            "error".to_string()
        }
    };
    emit(&app, &p);
    tokio::spawn(async move {
        mgr.transfers.lock().await.remove(&task_id);
    });
}

fn begin(sid: &str, direction: &str, local: &str, remote: &str, total: u64) -> TransferProgress {
    TransferProgress {
        task_id: String::new(),
        sid: sid.to_string(),
        direction: direction.to_string(),
        name: Path::new(remote)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| remote.to_string()),
        local: local.to_string(),
        remote: remote.to_string(),
        done: 0,
        total,
        rate: 0.0,
        state: "running".to_string(),
        message: None,
    }
}

/// 起一个上传任务。命令和"整目录上传"都走这里，避免同一段 spawn 写三遍。
async fn spawn_upload(
    app: &AppHandle,
    mgr: &SftpManager,
    sid: &str,
    sftp: &Arc<SftpSession>,
    local: String,
    remote: String,
    resume: bool,
) -> String {
    let task_id = uuid::Uuid::new_v4().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    mgr.transfers
        .lock()
        .await
        .insert(task_id.clone(), cancel.clone());
    let total = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
    let (app2, mgr2, sid2, tid) = (app.clone(), mgr.clone(), sid.to_string(), task_id.clone());
    let sftp2 = sftp.clone();
    tokio::spawn(async move {
        let mut p = begin(&sid2, "up", &local, &remote, total);
        p.task_id = tid.clone();
        let outcome = {
            let sink = |pr: &TransferProgress| {
                let _ = app2.emit("sftp://progress", pr);
            };
            upload_task(&sink, &mut p, &sftp2, &local, &remote, resume, &cancel).await
        };
        finish(app2, mgr2, tid, outcome, p);
    });
    task_id
}

/// 起一个下载任务。
async fn spawn_download(
    app: &AppHandle,
    mgr: &SftpManager,
    sid: &str,
    sftp: &Arc<SftpSession>,
    remote: String,
    local: String,
    total: u64,
    resume: bool,
) -> String {
    let task_id = uuid::Uuid::new_v4().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    mgr.transfers
        .lock()
        .await
        .insert(task_id.clone(), cancel.clone());
    let (app2, mgr2, sid2, tid) = (app.clone(), mgr.clone(), sid.to_string(), task_id.clone());
    let sftp2 = sftp.clone();
    tokio::spawn(async move {
        let mut p = begin(&sid2, "down", &local, &remote, total);
        p.task_id = tid.clone();
        let outcome = {
            let sink = |pr: &TransferProgress| {
                let _ = app2.emit("sftp://progress", pr);
            };
            download_task(&sink, &mut p, &sftp2, &remote, &local, resume, &cancel).await
        };
        finish(app2, mgr2, tid, outcome, p);
    });
    task_id
}

#[tauri::command]
pub async fn sftp_upload(
    app: AppHandle,
    sid: String,
    local: String,
    remote: String,
    resume: bool,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<String, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    std::fs::metadata(&local)
        .map_err(|err| err_kind("local_fs_error", format!("读取本地文件失败：{err}")))?;
    Ok(spawn_upload(&app, mgr.inner(), &sid, &sftp, local, remote, resume).await)
}

/// 传输主体。返回 `Outcome`：正常完成 / 用户取消 / 出错。
async fn upload_task(
    emit: Sink<'_>,
    p: &mut TransferProgress,
    sftp: &SftpSession,
    local: &str,
    remote: &str,
    resume: bool,
    cancel: &AtomicBool,
) -> Outcome {
    match upload_inner(emit, p, sftp, local, remote, resume, cancel).await {
        Ok(o) => o,
        Err(msg) => Outcome::Failed(msg),
    }
}

async fn upload_inner(
    emit: Sink<'_>,
    p: &mut TransferProgress,
    sftp: &SftpSession,
    local: &str,
    remote: &str,
    resume: bool,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    // 续传：远端文件已存在 → 打开时保留内容、从它的 size 处 seek 续写；
    // 否则带 TRUNCATE 覆盖，不用额外 set_len。
    let remote_len = if resume {
        sftp.metadata(remote).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let flags = if remote_len > 0 {
        OpenFlags::WRITE | OpenFlags::CREATE
    } else {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
    };
    let mut rf = sftp.open_with_flags(remote, flags).await.map_err(e)?;

    let mut start = 0u64;
    if remote_len > 0 {
        rf.seek(SeekFrom::Start(remote_len)).await.map_err(e)?;
        start = remote_len;
    }
    let mut lf = tokio::fs::File::open(local).await.map_err(e)?;
    if start > 0 {
        lf.seek(SeekFrom::Start(start)).await.map_err(e)?;
    }

    p.done = start;
    let mut buf = vec![0u8; CHUNK];
    let mut last_bytes = start;
    let mut last_tick = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            // 关掉远端句柄让已写入的部分落盘（保住半截文件，下次可续传）
            let _ = rf.shutdown().await;
            return Ok(Outcome::Cancelled);
        }
        let n = lf.read(&mut buf).await.map_err(e)?;
        if n == 0 {
            break;
        }
        rf.write_all(&buf[..n]).await.map_err(e)?;
        p.done += n as u64;
        let elapsed = last_tick.elapsed();
        if p.done - last_bytes >= PROGRESS_BYTES || elapsed.as_millis() >= PROGRESS_MS {
            p.rate = (p.done - last_bytes) as f64 / elapsed.as_secs_f64().max(1e-6);
            emit(p);
            last_bytes = p.done;
            last_tick = Instant::now();
        }
    }

    rf.flush().await.map_err(e)?;
    rf.sync_all().await.map_err(e)?;
    rf.shutdown().await.map_err(e)?;
    p.rate = 0.0;
    Ok(Outcome::Done)
}

#[tauri::command]
pub async fn sftp_download(
    app: AppHandle,
    sid: String,
    remote: String,
    local: String,
    resume: bool,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<String, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    let total = sftp.metadata(&remote).await.map_err(e)?.len();
    Ok(spawn_download(&app, mgr.inner(), &sid, &sftp, remote, local, total, resume).await)
}

/// 整目录上传：本地递归走一遍，远端按需建目录，每个文件一条独立任务。
///
/// 返回文件条数；目录结构本身不占进度条（秒建），只有文件才推进度。
#[tauri::command]
pub async fn sftp_upload_dir(
    app: AppHandle,
    sid: String,
    local: String,
    remote: String,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<usize, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    let root = std::path::PathBuf::from(&local);
    if !root.is_dir() {
        return Err(err_kind("local_fs_error", format!("不是目录：{local}")));
    }
    // 目录遍历是同步阻塞的，丢到 blocking 池里，别卡住 tokio 线程
    let files = tokio::task::spawn_blocking(move || collect_local_files(&root))
        .await
        .map_err(|err| err_kind("local_fs_error", format!("遍历本地目录失败：{err}")))?;

    let mgr2 = mgr.inner().clone();
    let mut made: Vec<String> = vec![remote.clone()];
    if sftp.metadata(&remote).await.is_err() {
        sftp.create_dir(&remote).await.map_err(e)?;
    }

    for (abs, rel) in &files {
        let remote_file = join_path(&remote, &rel.replace('\\', "/"));
        if let Some(parent) = remote_file.rsplit_once('/').map(|(p, _)| p.to_string()) {
            ensure_remote_dir(&sftp, &parent, &mut made).await?;
        }
        spawn_upload(
            &app,
            &mgr2,
            &sid,
            &sftp,
            abs.to_string_lossy().to_string(),
            remote_file,
            true, // 续传：大目录中断后重跑不会从头传
        )
        .await;
    }
    Ok(files.len())
}

/// 整目录下载：远端递归列一遍，本地按需建目录。
#[tauri::command]
pub async fn sftp_download_dir(
    app: AppHandle,
    sid: String,
    remote: String,
    local: String,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<usize, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    let md = sftp.metadata(&remote).await.map_err(e)?;
    if !md.is_dir() {
        return Err(err_kind("sftp_error", format!("不是目录：{remote}")));
    }
    let files = collect_remote_files(&sftp, &remote, "").await?;
    std::fs::create_dir_all(&local)
        .map_err(|err| err_kind("local_fs_error", format!("建本地目录失败：{err}")))?;

    let mgr2 = mgr.inner().clone();
    let sep = if local.contains('\\') { "\\" } else { "/" };
    for (remote_file, rel) in &files {
        let local_file = format!(
            "{}{}{}",
            local.trim_end_matches(['/', '\\']),
            sep,
            rel.replace('/', sep)
        );
        if let Some((parent, _)) = local_file.rsplit_once(sep) {
            std::fs::create_dir_all(parent)
                .map_err(|err| err_kind("local_fs_error", format!("建本地目录失败：{err}")))?;
        }
        let total = sftp
            .metadata(remote_file)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        spawn_download(
            &app,
            &mgr2,
            &sid,
            &sftp,
            remote_file.clone(),
            local_file,
            total,
            true,
        )
        .await;
    }
    Ok(files.len())
}

/// 本地递归收集文件：(绝对路径, 相对根目录的路径)。软链目录不跟随，避免成环。
fn collect_local_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(md) = std::fs::symlink_metadata(item.path()) else {
                continue;
            };
            if md.file_type().is_symlink() {
                continue;
            }
            if md.is_dir() {
                stack.push((item.path(), rel));
            } else {
                out.push((item.path(), rel));
            }
        }
    }
    out
}

/// 远端递归收集文件：(远端绝对路径, 相对路径)。
fn collect_remote_files<'a>(
    sftp: &'a SftpSession,
    root: &'a str,
    prefix: &'a str,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<(String, String)>, String>> + Send + 'a>,
> {
    Box::pin(async move {
        let mut out = Vec::new();
        let rd = sftp.read_dir(root).await.map_err(e)?;
        for entry in rd {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = join_path(root, &name);
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let md = entry.metadata();
            if md.is_dir() && !md.is_symlink() {
                out.extend(collect_remote_files(sftp, &child, &rel).await?);
            } else if !md.is_dir() {
                out.push((child, rel));
            }
        }
        Ok(out)
    })
}

/// 逐级建远端目录；`made` 记住已经建过的，避免重复 syscall 和"已存在"报错。
async fn ensure_remote_dir(
    sftp: &SftpSession,
    dir: &str,
    made: &mut Vec<String>,
) -> Result<(), String> {
    if made.iter().any(|m| m == dir) {
        return Ok(());
    }
    let mut acc = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        if made.iter().any(|m| m == &acc) {
            continue;
        }
        if sftp.metadata(&acc).await.is_err() {
            sftp.create_dir(&acc).await.map_err(e)?;
        }
        made.push(acc.clone());
    }
    Ok(())
}

async fn download_task(
    emit: Sink<'_>,
    p: &mut TransferProgress,
    sftp: &SftpSession,
    remote: &str,
    local: &str,
    resume: bool,
    cancel: &AtomicBool,
) -> Outcome {
    match download_inner(emit, p, sftp, remote, local, resume, cancel).await {
        Ok(o) => o,
        Err(msg) => Outcome::Failed(msg),
    }
}

async fn download_inner(
    emit: Sink<'_>,
    p: &mut TransferProgress,
    sftp: &SftpSession,
    remote: &str,
    local: &str,
    resume: bool,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    let mut rf = sftp.open(remote).await.map_err(e)?;

    let local_len = if resume {
        std::fs::metadata(local).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let mut start = 0u64;
    if local_len > 0 {
        // 续传：本地已有部分，且不比远端长（比远端长说明本地更新，直接覆盖重下）
        let remote_total = rf.metadata().await.map(|m| m.len()).unwrap_or(0);
        if local_len <= remote_total {
            start = local_len;
        }
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true);
    if start == 0 {
        opts.truncate(true);
    }
    let mut lf = opts.open(local).await.map_err(e)?;
    if start > 0 {
        rf.seek(SeekFrom::Start(start)).await.map_err(e)?;
        lf.seek(SeekFrom::Start(start)).await.map_err(e)?;
    }

    p.done = start;
    let mut buf = vec![0u8; CHUNK];
    let mut last_bytes = start;
    let mut last_tick = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = lf.flush().await;
            return Ok(Outcome::Cancelled);
        }
        let n = rf.read(&mut buf).await.map_err(e)?;
        if n == 0 {
            break;
        }
        lf.write_all(&buf[..n]).await.map_err(e)?;
        p.done += n as u64;
        let elapsed = last_tick.elapsed();
        if p.done - last_bytes >= PROGRESS_BYTES || elapsed.as_millis() >= PROGRESS_MS {
            p.rate = (p.done - last_bytes) as f64 / elapsed.as_secs_f64().max(1e-6);
            emit(p);
            last_bytes = p.done;
            last_tick = Instant::now();
        }
    }

    lf.flush().await.map_err(e)?;
    lf.sync_all().await.map_err(e)?;
    p.rate = 0.0;
    Ok(Outcome::Done)
}

#[tauri::command]
pub async fn sftp_cancel(task_id: String, mgr: State<'_, SftpManager>) -> Result<bool, String> {
    let map = mgr.transfers.lock().await;
    match map.get(&task_id) {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// 拖文件到终端：上传到 `<家目录>/sshbox-uploads/`，返回远端路径供前端粘贴。
///
/// 固定目录而不是"当前 pwd"：拿远端 shell 的 cwd 要往 PTY 里塞命令、还得解析
/// 提示符，脆且会污染终端输出；固定目录可预测、可复用。
#[tauri::command]
pub async fn sftp_upload_drop(
    app: AppHandle,
    sid: String,
    paths: Vec<String>,
    mgr: State<'_, SftpManager>,
    sessions: State<'_, SessionManager>,
) -> Result<Vec<String>, String> {
    let sftp = mgr.acquire(&sid, &sessions).await?;
    let home = sftp.canonicalize(".").await.map_err(e)?;
    let dir = join_path(&home, "sshbox-uploads");
    if sftp.metadata(&dir).await.is_err() {
        sftp.create_dir(&dir).await.map_err(e)?;
    }

    let mut out = Vec::new();
    for path in paths {
        let Some(name) = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
        else {
            continue;
        };
        let remote = join_path(&dir, &name);
        let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let task_id = uuid::Uuid::new_v4().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        mgr.transfers
            .lock()
            .await
            .insert(task_id.clone(), cancel.clone());

        let (app2, mgr2, sid2, remote2, tid) = (
            app.clone(),
            mgr.inner().clone(),
            sid.clone(),
            remote.clone(),
            task_id.clone(),
        );
        let sftp2 = sftp.clone();
        let local2 = path.clone();
        tokio::spawn(async move {
            let mut p = begin(&sid2, "up", &local2, &remote2, total);
            p.task_id = tid.clone();
            let outcome = {
                let sink = |pr: &TransferProgress| {
                    let _ = app2.emit("sftp://progress", pr);
                };
                upload_task(&sink, &mut p, &sftp2, &local2, &remote2, true, &cancel).await
            };
            finish(app2, mgr2, tid, outcome, p);
        });
        out.push(remote);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn join_path_handles_root_and_trailing_slash() {
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("/etc", "hosts"), "/etc/hosts");
        assert_eq!(join_path("/etc/", "hosts"), "/etc/hosts");
        assert_eq!(join_path("/home/z", ".bashrc"), "/home/z/.bashrc");
    }

    #[test]
    fn transfer_name_comes_from_remote_path() {
        let p = begin("sid", "up", "C:/tmp/a.txt", "/root/a.txt", 10);
        assert_eq!(p.name, "a.txt");
        assert_eq!(p.direction, "up");
        assert_eq!(p.state, "running");
    }

    #[test]
    fn transfer_name_falls_back_to_full_path() {
        let p = begin("sid", "down", "C:/tmp/x", "/", 0);
        assert_eq!(p.name, "/");
    }

    #[test]
    fn unix_secs_is_zero_on_error() {
        assert_eq!(unix_secs(Err(std::io::Error::other("x"))), 0);
    }
}

/// 真机测试：连一台真的 sshd 跑完整传输。
///
/// 只在设置了环境变量时执行，所以没 VM 的机器上 `cargo test` 照样全绿：
///
/// ```bash
/// SSHBOX_TEST_HOST=127.0.0.1 SSHBOX_TEST_PASSWORD=sshbox123 \
///   cargo test --lib live_sftp -- --nocapture --test-threads=1
/// ```
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::ssh::ClientHandler;
    use russh::client;
    use std::sync::Mutex as StdMutex;

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

    /// 连上并开一条 SFTP 通道；返回 (session, 家目录, 测试用临时目录)。
    async fn open() -> Option<(SftpSession, String, String)> {
        let (host, port, user, pw) = target()?;
        let kh = std::env::temp_dir().join(format!("sshbox-sftp-kh-{}", std::process::id()));
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
        .ok()?;
        let ok = h.authenticate_password(&user, &pw).await.ok()?;
        if !ok.success() {
            eprintln!("认证被拒");
            return None;
        }
        let ch = h.channel_open_session().await.ok()?;
        ch.request_subsystem(true, "sftp").await.ok()?;
        let sftp = SftpSession::new(ch.into_stream()).await.ok()?;
        let home = sftp.canonicalize(".").await.ok()?;
        let dir = join_path(&home, &format!("sshbox-live-{}", std::process::id()));
        if sftp.metadata(&dir).await.is_ok() {
            remove_tree(&sftp, &dir).await.ok();
        }
        sftp.create_dir(&dir).await.ok()?;
        Some((sftp, home, dir))
    }

    fn same_bytes(a: &std::path::Path, b: &std::path::Path) -> bool {
        match (std::fs::read(a), std::fs::read(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    /// 确定性内容，避免随机数据掩盖"少写了一块但长度相同"这类 bug。
    fn write_pattern(path: &std::path::Path, len: usize) {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(path, data).expect("写本地测试文件");
    }

    /// 收集进度帧，代替 Tauri 的 AppHandle。
    #[derive(Default)]
    struct Collector(StdMutex<Vec<(u64, u64, f64)>>);
    impl Collector {
        fn sink(&self) -> impl Fn(&TransferProgress) + Send + Sync + '_ {
            move |p: &TransferProgress| {
                self.0.lock().unwrap().push((p.done, p.total, p.rate));
            }
        }
        fn frames(&self) -> Vec<(u64, u64, f64)> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn live_sftp_browse_and_file_ops() {
        let Some((sftp, _home, dir)) = open().await else {
            eprintln!("跳过 live 测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };

        // 1) 列根目录
        let rd = sftp.read_dir("/").await.expect("列 /");
        let names: Vec<String> = rd.map(|e| e.file_name()).collect();
        assert!(
            names.contains(&"etc".to_string()),
            "/ 里应该有 etc：{names:?}"
        );

        // 2) 家目录能解析成绝对路径
        let home = sftp.canonicalize(".").await.expect("canonicalize .");
        assert!(home.starts_with('/'), "家目录必须是绝对路径：{home}");

        // 3) 建目录 → 列表能看到 → 改名 → 删掉
        let sub = join_path(&dir, "子目录 with space");
        sftp.create_dir(&sub).await.expect("建目录");
        let listed: Vec<String> = sftp
            .read_dir(&dir)
            .await
            .expect("列测试目录")
            .map(|e| e.file_name())
            .collect();
        assert!(
            listed.contains(&"子目录 with space".to_string()),
            "列表：{listed:?}"
        );

        let renamed = join_path(&dir, "renamed");
        sftp.rename(&sub, &renamed).await.expect("改名");
        assert!(sftp.metadata(&renamed).await.is_ok(), "改名后应存在");
        assert!(sftp.metadata(&sub).await.is_err(), "旧名字应不存在");

        // 4) 写文件、看属性、删掉
        let file = join_path(&dir, "a.txt");
        {
            let mut f = sftp.create(&file).await.expect("建文件");
            f.write_all(b"hello sshbox").await.expect("写内容");
            f.shutdown().await.expect("关闭文件");
        }
        let md = sftp.metadata(&file).await.expect("stat 文件");
        assert_eq!(md.len(), 12);
        assert!(!md.is_dir());
        assert!(
            md.permissions().to_string().starts_with('r'),
            "权限位：{}",
            md.permissions()
        );

        // 5) 非空目录不递归删要报"目录非空"，递归删要成功
        let err = sftp.remove_dir(&dir).await.expect_err("非空目录不该删成功");
        assert!(
            format!("{err}").to_lowercase().contains("fail") || format!("{err}").contains("非空")
        );
        remove_tree(&sftp, &dir).await.expect("递归删除");
        assert!(sftp.metadata(&dir).await.is_err(), "删完应不存在");

        // 收尾：清掉测试 known_hosts
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("sshbox-sftp-kh-{}", std::process::id())),
        );
        println!("✅ 浏览 + 文件操作（含中文/空格路径、递归删除）通过");
    }

    #[tokio::test]
    async fn live_sftp_transfer_roundtrip_cancel_and_resume() {
        let Some((sftp, _home, dir)) = open().await else {
            eprintln!("跳过 live 测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };

        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let src = tmp.join(format!("sshbox-src-{pid}.bin"));
        let dst = tmp.join(format!("sshbox-dst-{pid}.bin"));
        let dst2 = tmp.join(format!("sshbox-dst2-{pid}.bin"));
        let size = 4 * 1024 * 1024 + 12345; // 故意不是 64KB 的整数倍
        write_pattern(&src, size);

        let remote = join_path(&dir, "payload.bin");
        let remote2 = join_path(&dir, "payload2.bin");
        let local_src = src.to_string_lossy().to_string();

        // --- 1) 正常上传，收集进度帧 ---
        let col = Collector::default();
        let mut p = begin("test", "up", &local_src, &remote, size as u64);
        let cancel = AtomicBool::new(false);
        let outcome = upload_task(
            &col.sink(),
            &mut p,
            &sftp,
            &local_src,
            &remote,
            false,
            &cancel,
        )
        .await;
        assert!(matches!(outcome, Outcome::Done), "上传应完成");
        assert_eq!(p.done, size as u64, "上传字节数要等于本地大小");
        let frames = col.frames();
        assert!(!frames.is_empty(), "4MB 上传至少推一帧进度");
        assert!(frames.iter().all(|(d, t, _)| *d <= *t && *t == size as u64));

        let md = sftp.metadata(&remote).await.expect("远端文件应存在");
        assert_eq!(md.len(), size as u64, "远端大小必须等于本地");

        // --- 2) 下载回来逐字节比对（比 sha256 更直接，不需要新依赖）---
        let dst_s = dst.to_string_lossy().to_string();
        let mut p2 = begin("test", "down", &dst_s, &remote, size as u64);
        let outcome2 =
            download_task(&col.sink(), &mut p2, &sftp, &remote, &dst_s, false, &cancel).await;
        assert!(matches!(outcome2, Outcome::Done), "下载应完成");
        assert_eq!(p2.done, size as u64);
        assert!(same_bytes(&src, &dst), "下载内容必须与源文件逐字节一致");

        // --- 3) 取消：先置位再跑，远端应只留下半截（这里刚好 0 字节）---
        let cancel2 = AtomicBool::new(true);
        let mut p3 = begin("test", "up", &local_src, &remote2, size as u64);
        let outcome3 = upload_task(
            &col.sink(),
            &mut p3,
            &sftp,
            &local_src,
            &remote2,
            false,
            &cancel2,
        )
        .await;
        assert!(
            matches!(outcome3, Outcome::Cancelled),
            "取消应返回 Cancelled"
        );
        let partial = sftp
            .metadata(&remote2)
            .await
            .expect("取消后远端文件仍在")
            .len();
        assert!(partial < size as u64, "取消后不该是完整文件：{partial}");

        // --- 4) 续传：从半截处接着写，最终内容必须与源一致 ---
        let cancel3 = AtomicBool::new(false);
        let mut p4 = begin("test", "up", &local_src, &remote2, size as u64);
        let outcome4 = upload_task(
            &col.sink(),
            &mut p4,
            &sftp,
            &local_src,
            &remote2,
            true,
            &cancel3,
        )
        .await;
        assert!(matches!(outcome4, Outcome::Done), "续传应完成");
        let full = sftp.metadata(&remote2).await.expect("续传后文件").len();
        assert_eq!(full, size as u64, "续传后大小应等于本地");

        let dst2_s = dst2.to_string_lossy().to_string();
        let mut p5 = begin("test", "down", &dst2_s, &remote2, size as u64);
        download_task(
            &col.sink(),
            &mut p5,
            &sftp,
            &remote2,
            &dst2_s,
            false,
            &cancel3,
        )
        .await;
        assert!(same_bytes(&src, &dst2), "续传后的内容必须与源一致");

        // 收尾
        remove_tree(&sftp, &dir).await.expect("清理远端测试目录");
        for f in [&src, &dst, &dst2] {
            let _ = std::fs::remove_file(f);
        }
        println!("✅ 上传/下载/取消/续传（{size} 字节）全部通过，内容逐字节一致");
    }
}
