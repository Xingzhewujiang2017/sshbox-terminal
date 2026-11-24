//! 凭据保险箱：主密码 + PBKDF2 派生密钥 + AES-256-GCM 加密落盘。
//!
//! 为什么需要它：桌面版（Windows）用系统凭据管理器存密码；Linux 目标机上
//! 没有 keyring 守护进程（`keyring` 直接失败），所以 web/Electron 壳必须自带
//! 一套。这里刻意不发明新的信任模型：
//!
//! - 主密码**从不落盘**，只用于派生密钥；派生用 PBKDF2-HMAC-SHA256，20 万轮。
//! - 落盘文件里只有 `salt` / `nonce` / 密文，没有主密码也没有明文密码。
//! - 解锁后密钥与明文只活在进程内存，`vault_lock()` 或进程退出即消失。
//! - 每次写盘换一个新 nonce（AES-GCM 下 nonce 复用会毁掉机密性）。
//!
//! 与 Windows 版的关系：那边的 `credential_*` 走 keyring，这套只在
//! `credential_store_ok() == false` 时启用，互不干扰。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::store;

/// PBKDF2 迭代次数。20 万轮在目标机上约 0.2–0.5 秒，够慢也够用。
const PBKDF2_ROUNDS: u32 = 200_000;
const SALT_LEN: usize = 16;

#[derive(Serialize, Deserialize)]
struct FileFormat {
    v: u32,
    salt: String,
    nonce: String,
    data: String,
}

struct Unlocked {
    key: [u8; 32],
    salt: Vec<u8>,
    entries: HashMap<String, String>,
}

static VAULT: Mutex<Option<Unlocked>> = Mutex::new(None);

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

pub fn path() -> PathBuf {
    store::data_dir().join("credentials.enc")
}

fn derive_key(master: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    let rounds = std::num::NonZeroU32::new(PBKDF2_ROUNDS).ok_or("轮数非法")?;
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        rounds,
        salt,
        master.as_bytes(),
        &mut key,
    );
    Ok(key)
}

fn seal(key: &[u8; 32], salt: &[u8], entries: &HashMap<String, String>) -> Result<FileFormat, String> {
    let plain = serde_json::to_vec(entries).map_err(|e| format!("序列化失败: {}", e))?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "随机数失败".to_string())?;
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| "密钥非法".to_string())?;
    let sealing = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut buf = plain;
    sealing
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
        .map_err(|_| "加密失败".to_string())?;
    Ok(FileFormat {
        v: 1,
        salt: b64().encode(salt),
        nonce: b64().encode(nonce_bytes),
        data: b64().encode(&buf),
    })
}

fn open(key: &[u8; 32], f: &FileFormat) -> Result<HashMap<String, String>, String> {
    let nonce_raw = b64().decode(&f.nonce).map_err(|_| "nonce 损坏".to_string())?;
    if nonce_raw.len() != NONCE_LEN {
        return Err("nonce 长度不对".into());
    }
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&nonce_raw);
    let mut buf = b64().decode(&f.data).map_err(|_| "密文损坏".to_string())?;
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| "密钥非法".to_string())?;
    let opening = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let plain = opening
        .open_in_place(nonce, Aad::empty(), &mut buf)
        .map_err(|_| "主密码错误，或文件已损坏".to_string())?;
    serde_json::from_slice(plain).map_err(|e| format!("内容损坏: {}", e))
}

fn write_file(f: &FileFormat) -> Result<(), String> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {}", e))?;
    }
    let tmp = p.with_extension("enc.tmp");
    let json = serde_json::to_vec_pretty(f).map_err(|e| format!("序列化失败: {}", e))?;
    {
        let mut fh = std::fs::File::create(&tmp).map_err(|e| format!("写临时文件失败: {}", e))?;
        fh.write_all(&json).map_err(|e| format!("写失败: {}", e))?;
        fh.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &p).map_err(|e| format!("落盘失败: {}", e))?;
    Ok(())
}

pub fn exists() -> bool {
    path().is_file()
}

pub fn is_unlocked() -> bool {
    VAULT.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 首次创建：要求主密码非空，且不能覆盖已有文件。
pub fn init(master: &str) -> Result<(), String> {
    if master.trim().len() < 6 {
        return Err("主密码至少 6 位".into());
    }
    if exists() {
        return Err("保险箱已存在，请直接解锁".into());
    }
    let mut salt = vec![0u8; SALT_LEN];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "随机数失败".to_string())?;
    let key = derive_key(master, &salt)?;
    let entries = HashMap::new();
    write_file(&seal(&key, &salt, &entries)?)?;
    *VAULT.lock().map_err(|_| "锁坏了")? = Some(Unlocked { key, salt, entries });
    Ok(())
}

/// 解锁：解密失败一律报"主密码错误"（不区分文件损坏，避免泄露信息）。
pub fn unlock(master: &str) -> Result<(), String> {
    let raw = std::fs::read(path()).map_err(|_| "保险箱还不存在".to_string())?;
    let f: FileFormat = serde_json::from_slice(&raw).map_err(|_| "保险箱文件损坏".to_string())?;
    let salt = b64().decode(&f.salt).map_err(|_| "盐损坏".to_string())?;
    let key = derive_key(master, &salt)?;
    let entries = open(&key, &f)?;
    *VAULT.lock().map_err(|_| "锁坏了")? = Some(Unlocked { key, salt, entries });
    Ok(())
}

pub fn lock() {
    if let Ok(mut g) = VAULT.lock() {
        *g = None;
    }
}

pub fn get(id: &str) -> Option<String> {
    VAULT
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|u| u.entries.get(id).cloned()))
}

pub fn set(id: &str, secret: &str) -> Result<(), String> {
    let mut g = VAULT.lock().map_err(|_| "锁坏了")?;
    let Some(u) = g.as_mut() else {
        return Err("保险箱未解锁".into());
    };
    u.entries.insert(id.to_string(), secret.to_string());
    let f = seal(&u.key, &u.salt, &u.entries)?;
    write_file(&f)
}

pub fn remove(id: &str) -> Result<(), String> {
    let mut g = VAULT.lock().map_err(|_| "锁坏了")?;
    let Some(u) = g.as_mut() else {
        return Err("保险箱未解锁".into());
    };
    u.entries.remove(id);
    let f = seal(&u.key, &u.salt, &u.entries)?;
    write_file(&f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加解密往返：用同一个 key/salt，密文能解回原样。
    #[test]
    fn seal_then_open_roundtrip() {
        let salt = vec![7u8; SALT_LEN];
        let key = derive_key("master-pw", &salt).unwrap();
        let mut m = HashMap::new();
        m.insert("h1".to_string(), "secret".to_string());
        let f = seal(&key, &salt, &m).unwrap();
        let back = open(&key, &f).unwrap();
        assert_eq!(back.get("h1").map(String::as_str), Some("secret"));
    }

    /// 换一个主密码（=换密钥）就解不开，且报的是"主密码错误"。
    #[test]
    fn wrong_master_fails() {
        let salt = vec![1u8; SALT_LEN];
        let key = derive_key("right", &salt).unwrap();
        let mut m = HashMap::new();
        m.insert("h1".to_string(), "x".to_string());
        let f = seal(&key, &salt, &m).unwrap();
        let bad = derive_key("wrong", &salt).unwrap();
        assert!(open(&bad, &f).is_err());
    }

    /// 同一个明文加密两次，密文必须不同（nonce 每次新生成）。
    #[test]
    fn nonce_is_fresh_each_time() {
        let salt = vec![2u8; SALT_LEN];
        let key = derive_key("pw", &salt).unwrap();
        let m = HashMap::new();
        let a = seal(&key, &salt, &m).unwrap();
        let b = seal(&key, &salt, &m).unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.data, b.data);
    }
}
