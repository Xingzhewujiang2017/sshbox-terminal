//! Persistence layer: hosts.json / settings.json on disk + secrets in the OS
//! credential store (Windows Credential Manager via `keyring`).
//!
//! Layout (Windows): `%APPDATA%\sshbox\`
//!   hosts.json     — host list + groups (never contains passwords)
//!   settings.json  — UI + monitor preferences
//!   known_hosts    — trusted host keys (OpenSSH format, our own file)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "sshbox";
const KEYRING_SERVICE: &str = "sshbox";

pub fn data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

pub fn ensure_dir() -> Result<PathBuf> {
    let d = data_dir();
    fs::create_dir_all(&d).with_context(|| format!("创建配置目录失败: {}", d.display()))?;
    Ok(d)
}

pub fn known_hosts_path() -> PathBuf {
    data_dir().join("known_hosts")
}

fn hosts_path() -> PathBuf {
    data_dir().join("hosts.json")
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Write via a temp file + rename so a crash mid-write cannot truncate config.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    ensure_dir()?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content).with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("替换文件失败: {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

fn default_group() -> String {
    "默认".to_string()
}
fn default_port() -> u16 {
    22
}
fn default_auth() -> String {
    "password".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub name: String,
    #[serde(default = "default_group")]
    pub group: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// "password" | "key"
    #[serde(default = "default_auth")]
    pub auth: String,
    /// Path to a private key file (auth = "key"); used when the frontend
    /// passes no inline key content.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Whether the password lives in the OS credential store.
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub auto_reconnect: bool,
    /// "strict" (default) or "accept_any" (skip host key verification).
    #[serde(default)]
    pub host_key_policy: Option<String>,
    /// 端口转发规则（v0.4）：跟着主机走，导入导出时一起搬
    #[serde(default)]
    pub forwards: Vec<crate::forward::ForwardRule>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub last_used: Option<i64>,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostsFile {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<Host>,
}

fn one() -> u32 {
    1
}

impl Default for HostsFile {
    fn default() -> Self {
        HostsFile {
            version: 1,
            groups: vec![default_group()],
            hosts: Vec::new(),
        }
    }
}

// --- 端口转发规则（挂在 host 上，跟主机一起导入导出）----------------------

pub fn forwards_of(host_id: &str) -> Vec<crate::forward::ForwardRule> {
    load_hosts()
        .hosts
        .into_iter()
        .find(|h| h.id == host_id)
        .map(|h| h.forwards)
        .unwrap_or_default()
}

/// 新增或按 id 更新一条规则。
pub fn save_forward(host_id: &str, rule: crate::forward::ForwardRule) -> Result<()> {
    let mut file = load_hosts();
    let host = file
        .hosts
        .iter_mut()
        .find(|h| h.id == host_id)
        .ok_or_else(|| anyhow::anyhow!("主机不存在: {host_id}"))?;
    match host.forwards.iter_mut().find(|r| r.id == rule.id) {
        Some(slot) => *slot = rule,
        None => host.forwards.push(rule),
    }
    save_hosts(&file)
}

pub fn delete_forward(host_id: &str, id: &str) -> Result<()> {
    let mut file = load_hosts();
    if let Some(host) = file.hosts.iter_mut().find(|h| h.id == host_id) {
        host.forwards.retain(|r| r.id != id);
    }
    save_hosts(&file)
}

pub fn load_hosts() -> HostsFile {
    let p = hosts_path();
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("hosts.json 解析失败({}), 以空列表启动: {}", p.display(), e);
            HostsFile::default()
        }),
        Err(_) => HostsFile::default(),
    }
}

pub fn save_hosts(file: &HostsFile) -> Result<()> {
    write_atomic(
        &hosts_path(),
        &serde_json::to_string_pretty(file).context("序列化 hosts 失败")?,
    )
}

pub fn get_host(id: &str) -> Option<Host> {
    load_hosts().hosts.into_iter().find(|h| h.id == id)
}

/// 给还没有 id 的主机补一个 uuid。
///
/// 单独抽出来是因为**调用顺序有意义**：凭据要按 id 存，所以必须先补 id 再写密码。
/// 以前 `host_save` 直接 `set_secret(&host.id, …)`，新建主机的 id 还是空串，
/// 密码被写到一个空 key 上，主机本体随后拿到新 uuid —— 名下没有密码，
/// 于是「刚输入过密码，双击又让输一次」。
pub fn ensure_host_id(host: &mut Host) {
    if host.id.trim().is_empty() {
        host.id = uuid::Uuid::new_v4().to_string();
    }
}

/// Insert or update by id, keeping list order stable for existing entries.
pub fn upsert_host(mut host: Host) -> Result<Host> {
    let mut file = load_hosts();
    ensure_host_id(&mut host);
    if host.created_at == 0 {
        host.created_at = chrono::Utc::now().timestamp();
    }
    if host.group.trim().is_empty() {
        host.group = default_group();
    }
    match file.hosts.iter_mut().find(|h| h.id == host.id) {
        Some(existing) => *existing = host.clone(),
        None => file.hosts.push(host.clone()),
    }
    if !file.groups.iter().any(|g| g == &host.group) {
        file.groups.push(host.group.clone());
    }
    save_hosts(&file)?;
    Ok(host)
}

pub fn delete_host(id: &str) -> Result<()> {
    let mut file = load_hosts();
    file.hosts.retain(|h| h.id != id);
    save_hosts(&file)?;
    let _ = delete_secret(id);
    Ok(())
}

/// Touch `last_used` without rewriting unrelated fields.
pub fn mark_used(id: &str) {
    let mut file = load_hosts();
    if let Some(h) = file.hosts.iter_mut().find(|h| h.id == id) {
        h.last_used = Some(chrono::Utc::now().timestamp());
        let _ = save_hosts(&file);
    }
}

// ---------------------------------------------------------------------------
// Secrets (OS credential store)
// ---------------------------------------------------------------------------

fn cred_entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, id).context("打开系统凭据存储失败")
}

pub fn set_secret(id: &str, secret: &str) -> Result<()> {
    cred_entry(id)?
        .set_password(secret)
        .context("写入系统凭据存储失败")
}

pub fn get_secret(id: &str) -> Result<String> {
    cred_entry(id)?
        .get_password()
        .context("读取系统凭据存储失败")
}

pub fn has_secret(id: &str) -> bool {
    get_secret(id).is_ok()
}

pub fn delete_secret(id: &str) -> Result<()> {
    match cred_entry(id)?.delete_credential() {
        Ok(()) => Ok(()),
        // Deleting a credential that was never stored is not an error for us.
        Err(e) => {
            log::debug!("删除凭据 {} 时: {}", id, e);
            Ok(())
        }
    }
}

/// Whether the OS credential store is usable at all (surfaced in settings UI).
pub fn credential_store_ok() -> bool {
    matches!(keyring::Entry::store_status(), Ok(()))
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn default_interval() -> u64 {
    2
}
fn default_true() -> bool {
    true
}
fn default_attempts() -> u32 {
    3
}
fn default_top_n() -> u32 {
    12
}
fn default_cpu_alert() -> f64 {
    90.0
}
fn default_mem_alert() -> f64 {
    90.0
}
fn default_disk_alert() -> f64 {
    90.0
}

/// 一条快捷命令（存 settings.json，跟其他设置一起走）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    /// 发送后是否自动回车（想"先填进终端再改"就关掉）
    #[serde(default = "default_true")]
    pub enter: bool,
}

/// 默认三条：装好就能用，覆盖最常见的三件事。
fn default_quick_commands() -> Vec<QuickCommand> {
    vec![
        QuickCommand {
            id: "qc-disk".into(),
            label: "磁盘".into(),
            command: "df -h".into(),
            enter: true,
        },
        QuickCommand {
            id: "qc-mem".into(),
            label: "内存".into(),
            command: "free -h".into(),
            enter: true,
        },
        QuickCommand {
            id: "qc-top".into(),
            label: "CPU Top".into(),
            command: "ps aux --sort=-%cpu | head -15".into(),
            enter: true,
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_interval")]
    pub sample_interval_secs: u64,
    #[serde(default = "default_true")]
    pub monitor_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_attempts")]
    pub reconnect_max_attempts: u32,
    #[serde(default = "default_top_n")]
    pub process_top_n: u32,
    #[serde(default = "default_true")]
    pub alerts_enabled: bool,
    #[serde(default = "default_cpu_alert")]
    pub cpu_alert_pct: f64,
    #[serde(default = "default_mem_alert")]
    pub mem_alert_pct: f64,
    #[serde(default = "default_disk_alert")]
    pub disk_alert_pct: f64,
    #[serde(default = "default_true")]
    pub confirm_on_close_tab: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 快捷命令面板（v0.4）
    #[serde(default = "default_quick_commands")]
    pub quick_commands: Vec<QuickCommand>,
    /// Persist samples to the SQLite history file.
    #[serde(default = "default_true")]
    pub history_enabled: bool,
    /// History cadence, independent of the live sample interval: the live
    /// monitor can run at 2s while history records every 10s.
    #[serde(default = "default_history_interval")]
    pub history_interval_secs: u64,
    #[serde(default = "default_retention")]
    pub history_retention_days: u64,
}

fn default_history_interval() -> u64 {
    2
}

fn default_retention() -> u64 {
    30
}

fn default_theme() -> String {
    "dark".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sample_interval_secs: default_interval(),
            monitor_enabled: true,
            auto_reconnect: true,
            reconnect_max_attempts: default_attempts(),
            process_top_n: default_top_n(),
            alerts_enabled: true,
            cpu_alert_pct: default_cpu_alert(),
            mem_alert_pct: default_mem_alert(),
            disk_alert_pct: default_disk_alert(),
            confirm_on_close_tab: true,
            theme: default_theme(),
            quick_commands: default_quick_commands(),
            history_enabled: true,
            history_interval_secs: default_history_interval(),
            history_retention_days: default_retention(),
        }
    }
}

pub fn load_settings() -> Settings {
    match fs::read_to_string(settings_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(s: &Settings) -> Result<()> {
    write_atomic(
        &settings_path(),
        &serde_json::to_string_pretty(s).context("序列化设置失败")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_roundtrip_keeps_order_and_ids() {
        let mut file = HostsFile::default();
        let a = Host {
            id: "a".into(),
            name: "A".into(),
            group: "默认".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: "password".into(),
            key_path: None,
            save_password: false,
            auto_reconnect: false,
            host_key_policy: None,
            forwards: Vec::new(),
            color: None,
            note: None,
            last_used: None,
            created_at: 1,
        };
        file.hosts.push(a.clone());
        let json = serde_json::to_string(&file).unwrap();
        let back: HostsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hosts[0].id, "a");
        assert_eq!(back.hosts[0].port, 22);
    }

    #[test]
    fn ensure_host_id_fills_only_empty_ids() {
        // 凭据按 id 存，所以新建主机必须先有 id 再写密码 —— 这条钉住那个顺序契约。
        let mk = |id: &str| Host {
            id: id.into(),
            name: "新机器".into(),
            group: "默认".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: "password".into(),
            key_path: None,
            save_password: true,
            auto_reconnect: true,
            host_key_policy: None,
            forwards: Vec::new(),
            color: None,
            note: None,
            last_used: None,
            created_at: 0,
        };

        let mut h = mk("");
        ensure_host_id(&mut h);
        assert!(!h.id.trim().is_empty(), "空 id 要被补成 uuid");
        assert_eq!(h.id.len(), 36, "补出来的应该是标准 uuid: {}", h.id);

        let mut keep = mk("wsl-ubuntu-test");
        ensure_host_id(&mut keep);
        assert_eq!(keep.id, "wsl-ubuntu-test", "已有 id 不能被改写");
    }

    #[test]
    fn defaults_survive_missing_fields() {
        // Old hosts.json files predate save_password / host_key_policy.
        let json = r#"{"version":1,"groups":[],"hosts":[{"id":"x","name":"n","host":"h","username":"u"}]}"#;
        let f: HostsFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.hosts[0].port, 22);
        assert_eq!(f.hosts[0].auth, "password");
        assert_eq!(f.hosts[0].group, "默认");
        assert!(!f.hosts[0].save_password);
    }

    #[test]
    fn quick_commands_default_when_missing() {
        // 老版本写的 settings.json 里没有这个字段
        let json = r#"{"sample_interval_secs":2,"theme":"light"}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.quick_commands.len(), 3, "缺字段时要给默认三条");
        assert!(s.quick_commands.iter().all(|q| q.enter && !q.command.is_empty()));
    }

    #[test]
    fn settings_defaults_are_sane() {
        let s = Settings::default();
        assert_eq!(s.sample_interval_secs, 2);
        assert!(s.auto_reconnect);
        assert_eq!(s.cpu_alert_pct, 90.0);
    }
}
