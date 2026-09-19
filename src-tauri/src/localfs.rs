//! 本地侧文件浏览（SFTP 双栏的左半）。
//!
//! 刻意不引 Tauri 的 fs 插件：只需要列目录 + 建目录/改名/删除，标准库够用，
//! 而且本地路径的错误文案能和远端走同一套 `err_kind` 前缀，UI 处理逻辑一致。

use serde::Serialize;
use std::path::PathBuf;

use crate::ssh::err_kind;

fn e(err: impl std::fmt::Display) -> String {
    err_kind("local_fs_error", format!("{:#}", err))
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// unix 秒；取不到为 0
    pub modified: i64,
    /// POSIX 权限位（Windows 上为空串，UI 显示占位符）
    pub permissions: String,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<LocalEntry>,
    /// Windows 盘符（"C:\\"）；非 Windows 恒为 ["/"]
    pub drives: Vec<String>,
    pub home: String,
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// 盘符列表。
///
/// 用 `GetLogicalDrives` 一次位图调用拿到，而不是逐个 `A:\` 试 metadata：
/// 后者碰到空光驱/断开的网络盘会卡住，而且会去碰盘。
#[cfg(windows)]
fn drives() -> Vec<String> {
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i as u8) as char))
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<String> {
    vec!["/".to_string()]
}

/// 去掉 Windows 扩展长度前缀。
///
/// `canonicalize` 会返回 `\\?\C:\Users\Z`（以及 UNC 的 `\\?\UNC\server\share`），
/// 直接显示给用户很难看，拼给用户看/再传回来也容易出错；两种写法 Windows 都认，
/// 所以只在边界处剥掉前缀。
fn strip_extended(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

fn modified_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn is_hidden(md: &std::fs::Metadata, name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x2 != 0 || name.starts_with('.')
}

#[cfg(not(windows))]
fn is_hidden(_md: &std::fs::Metadata, name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(windows)]
fn permissions(_md: &std::fs::Metadata) -> String {
    String::new()
}

#[cfg(not(windows))]
fn permissions(md: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = md.permissions().mode();
    let mut s = String::with_capacity(9);
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        s.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

#[tauri::command]
pub fn local_list(path: Option<String>) -> Result<LocalListing, String> {
    let home = home();
    let raw = path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.clone());
    // 规范化：让 UI 显示的路径永远是绝对路径（.. 也一并展开）
    let dir = std::fs::canonicalize(&raw).unwrap_or(raw);
    let rd = std::fs::read_dir(&dir).map_err(e)?;

    let mut entries: Vec<LocalEntry> = Vec::new();
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let target = item.path();
        // 先用 symlink_metadata 判断"本身是不是软链"，再用 metadata 看目标；
        // 坏软链（目标不存在）时 metadata 会失败，此时退回软链自身的信息。
        let link_md = std::fs::symlink_metadata(&target).ok();
        let is_symlink = link_md
            .as_ref()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let md = match std::fs::metadata(&target) {
            Ok(md) => md,
            // 坏软链（目标不存在）：metadata 失败，退回软链自身的信息
            Err(_) => match link_md {
                Some(md) => md,
                None => continue,
            },
        };
        entries.push(LocalEntry {
            name: name.clone(),
            path: strip_extended(&target.to_string_lossy()),
            is_dir: md.is_dir(),
            is_symlink,
            size: if md.is_dir() { 0 } else { md.len() },
            modified: modified_secs(&md),
            permissions: permissions(&md),
            hidden: is_hidden(&md, &name),
        });
    }

    // 目录在前，然后按名字（忽略大小写）—— 和远端列表保持同一排序规则
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(LocalListing {
        path: strip_extended(&dir.to_string_lossy()),
        parent: dir.parent().map(|p| strip_extended(&p.to_string_lossy())),
        entries,
        drives: drives(),
        home: home.to_string_lossy().to_string(),
    })
}

/// 批量判断类型："dir" | "file" | "missing"。
///
/// 从资源管理器拖进来的路径前端拿不到类型信息（Tauri 的拖放事件只给路径），
/// 而文件要走单文件传输、目录要走整目录传输，所以得问一下后端。
#[tauri::command]
pub fn local_kinds(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| match std::fs::symlink_metadata(&p) {
            Ok(md) if md.is_dir() => "dir".to_string(),
            Ok(_) => "file".to_string(),
            Err(_) => "missing".to_string(),
        })
        .collect()
}

#[tauri::command]
pub fn local_mkdir(path: String) -> Result<(), String> {
    std::fs::create_dir(&path).map_err(e)
}

#[tauri::command]
pub fn local_rename(from: String, to: String) -> Result<(), String> {
    std::fs::rename(&from, &to).map_err(e)
}

#[tauri::command]
pub fn local_remove(path: String, is_dir: bool, recursive: bool) -> Result<(), String> {
    // 软链按文件删（即使它指向目录）——否则会删掉链接目标的内容
    let md = std::fs::symlink_metadata(&path).map_err(e)?;
    let real_dir = md.is_dir() && !md.file_type().is_symlink();
    if is_dir && real_dir {
        if recursive {
            std::fs::remove_dir_all(&path).map_err(e)
        } else {
            std::fs::remove_dir(&path).map_err(|err| {
                if err.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                    err_kind("dir_not_empty", "目录非空（勾选“递归删除”可整目录删除）")
                } else {
                    e(err)
                }
            })
        }
    } else {
        std::fs::remove_file(&path).map_err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_absolute() {
        assert!(home().is_absolute(), "home 必须是绝对路径: {:?}", home());
    }

    #[test]
    fn list_without_path_returns_home_and_drives() {
        let l = local_list(None).expect("列 home 目录");
        assert!(l.path.to_lowercase().contains("users") || !l.path.is_empty());
        assert!(!l.drives.is_empty(), "至少有一个盘符");
        assert_eq!(l.home, home().to_string_lossy());
    }

    #[test]
    fn list_sorts_dirs_first() {
        let l = local_list(Some(home().to_string_lossy().to_string())).expect("列 home");
        let first_file = l.entries.iter().position(|x| !x.is_dir);
        let last_dir = l.entries.iter().rposition(|x| x.is_dir);
        if let (Some(f), Some(d)) = (first_file, last_dir) {
            assert!(d < f, "目录必须排在文件前面");
        }
    }

    #[test]
    fn extended_prefix_is_stripped() {
        assert_eq!(strip_extended(r"\\?\C:\Users\Z"), r"C:\Users\Z");
        assert_eq!(
            strip_extended(r"\\?\UNC\server\share"),
            r"\\server\share"
        );
        assert_eq!(strip_extended(r"C:\plain"), r"C:\plain");
    }

    #[test]
    fn missing_dir_reports_kind() {
        let err = local_list(Some("Q:\\definitely-missing-dir".into())).unwrap_err();
        assert!(
            err.starts_with(crate::ssh::ERR_PREFIX),
            "错误要带前缀: {err}"
        );
    }
}
