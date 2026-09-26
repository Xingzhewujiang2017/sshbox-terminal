//! 日志与故障记录兜底（v0.7.1）：panic hook / 异常退出标记 / 启动环境信息。
//!
//! 口径与纪律见 `.hermes/plans/日志与故障记录-评估与改造.md`「附二」。
//! 三个原则：
//! - panic hook 里**不 panic、不 unwrap**，直写日志文件绕开 fern（防止 panic
//!   恰发生在持有日志互斥锁的线程内时，`log::error!` 二次抢锁永久挂死）；
//! - 异常退出标记按 PID/实例分文件，多实例互不覆盖，且只对"进程已死"的
//!   残留标记报警（否则第二个实例启动时，还活着的第一个实例会被误报）；
//! - 报过一次的标记就删除（等价于"写 stopped"），既不重复报，也不在日志目录
//!   里积攒一堆 flag 垃圾文件。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, Timelike};

/// 日志目录，setup 里由 `app.path().app_log_dir()` 解出。
/// 用 OnceLock 而不是传参：panic hook 是全局的，退出标记要在 `run()` 返回后
/// 再写，两处都拿不到 build 时的局部变量。
pub static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn log_dir() -> &'static Path {
    LOG_DIR
        .get()
        .expect("logbook: LOG_DIR 未初始化（setup 没跑？）")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 与插件默认格式一致的本地时间 `[2026-09-25][10:30:00]`（括号由调用方补）。
fn local_ts() -> String {
    let n = Local::now();
    format!(
        "{:04}-{:02}-{:02}][{:02}:{:02}:{:02}",
        n.year(),
        n.month(),
        n.day(),
        n.hour(),
        n.minute(),
        n.second()
    )
}

/// A1：Rust panic 兜底。release 是 `panic = "abort"`——hook 在 abort 前仍会
/// 同步执行，所以这一步能保证"日志里留下 panic 行"再死。
pub fn install_panic_hook(log_file: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        // 1) 直写日志文件（不经过 fern/log 锁）。写失败也吞掉：hook 里不许 panic。
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "（非字符串 payload）".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".to_string());
        let line = format!(
            "[{}][sshbox_lib][ERROR] [panic] {} 位置={} pid={}\n",
            local_ts(),
            payload,
            location,
            std::process::id()
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        // 2) 附加一条走正常通道（正常线程 panic 时这条也能进同一文件；
        //    极端情形（持日志锁线程内 panic）下即便死锁，直写行已经在盘上）。
        log::error!("[panic] {} 位置={}", payload, location);
    }));
}

/// A3：异常退出标记 —— 启动时扫一遍残留标记，先报后清；再立自己的。
pub fn scan_and_warn(dir: &Path) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.starts_with("exit-") || !name.ends_with(".flag") {
            continue;
        }
        let pid: u32 = match name["exit-".len()..name.len() - ".flag".len()].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let path = entry.path();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if content.contains("started") && !content.contains("stopped") {
            if pid_alive(pid) {
                // 多实例重叠：这个实例还活着，只是还没退出 —— 不是异常
                continue;
            }
            let started_at = content.lines().next().unwrap_or_default().to_string();
            log::warn!(
                "上次实例未正常退出 pid={}（{}），已记录一次",
                pid,
                started_at
            );
            // 报一次即清除：等价于"写 stopped"，避免每次启动重复报 + 目录里积 flag
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn mark_started(dir: &Path) {
    let pid = std::process::id();
    let _ = std::fs::write(
        dir.join(format!("exit-{pid}.flag")),
        format!("started at {}\npid {pid}\n", unix_now()),
    );
}

/// 正常退出时删自己的标记（内容等价于"stopped"：文件不在了，下次扫描无话可说）。
pub fn mark_stopped(dir: &Path) {
    let pid = std::process::id();
    let _ = std::fs::remove_file(dir.join(format!("exit-{pid}.flag")));
}

/// 进程还活着吗？tasklist CSV 的 pid 列是引号包着的数字，按 `"1234"` 精确匹配，
/// 不会把 1234 当 123。拿不到结果时保守当"活着"（宁漏报不误报）。
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(true)
}

/// B1：OS 版本一行字（`cmd /c ver`，零依赖；失败退化成 "windows"）。
/// `ver` 在 UTF-8 代码页下会先吐一行 “Active code page: 65001”，剥掉。
pub fn os_version() -> String {
    match std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
    {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let parts: Vec<&str> = s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("Active code page"))
                .collect();
            if parts.is_empty() {
                std::env::consts::OS.to_string()
            } else {
                parts.join(" | ")
            }
        }
        Err(_) => std::env::consts::OS.to_string(),
    }
}

/// B1：WebView2 Evergreen 运行时版本，读 EdgeUpdate 注册表 `pv`（毫秒级）。
///
/// 不用 `wry::webview_version()`：那货首次调用会拉起浏览器运行时，
/// 实测要 2–5 秒 —— 放 setup 里会拖死窗口创建，WebView2 初始导航
/// 等不及就永久停在 ERR_CONNECTION_REFUSED（本批踩过的坑）。
pub fn webview2_version() -> String {
    // 注意：x64 机器上 Evergreen 运行时的 Clients 键实测在
    // WOW6432Node（32 位视图）下；KEY 只从 \Microsoft 开始，roots 负责拼前缀。
    const KEY: &str =
        r"\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let roots = [
        r"HKLM\SOFTWARE\WOW6432Node", // 本机实测：x64 运行时注册在 32 位视图下
        r"HKLM\SOFTWARE",
        r"HKCU\Software",
    ];
    for root in roots {
        let key = format!("{root}{KEY}");
        let Ok(o) = std::process::Command::new("reg")
            .args(["query", &key, "/v", "pv"])
            .output()
        else {
            continue;
        };
        let s = String::from_utf8_lossy(&o.stdout);
        for line in s.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // `    pv    REG_SZ    153.0.4234.48`
            if parts.len() >= 3
                && (parts[1] == "REG_SZ" || parts[1] == "REG_EXPAND_SZ")
                && !parts[2].is_empty()
            {
                return parts[2].to_string();
            }
        }
    }
    "未知".to_string()
}
