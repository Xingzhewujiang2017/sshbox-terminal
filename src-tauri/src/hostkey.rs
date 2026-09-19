//! Host key verification against our own `known_hosts` file.
//!
//! Trust model (OpenSSH-like, trust-on-first-use):
//!   * `strict`      — unknown key ⇒ refuse and ask the user; changed key ⇒ refuse loudly.
//!   * `accept_new`  — unknown key ⇒ record it and continue (user clicked "信任并保存").
//!   * `accept_any`  — skip verification entirely (explicit per-host opt-in only).
//!
//! A key that *changed* is never accepted automatically: that is the
//! man-in-the-middle signal, and only an explicit "forget the old key" action
//! can clear it.

use std::fs;
use std::path::Path;

use russh::keys::known_hosts::{
    check_known_hosts_path, known_host_keys_path, learn_known_hosts_path,
};
use russh::keys::{HashAlg, PublicKey};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyOutcome {
    /// Key matches the recorded one.
    Trusted,
    /// Key was unknown and has just been written to known_hosts.
    Learned {
        fingerprint: String,
        key_type: String,
    },
    /// Key is unknown and the policy forbids learning it.
    Unknown {
        fingerprint: String,
        key_type: String,
    },
    /// A different key is already recorded for this host — possible MITM.
    Changed {
        fingerprint: String,
        expected: String,
        key_type: String,
    },
    /// Policy `accept_any`: verification skipped (reported so the UI can warn).
    Skipped {
        fingerprint: String,
        key_type: String,
    },
}

impl VerifyOutcome {
    pub fn is_accepted(&self) -> bool {
        matches!(
            self,
            VerifyOutcome::Trusted | VerifyOutcome::Learned { .. } | VerifyOutcome::Skipped { .. }
        )
    }
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

pub fn key_type(key: &PublicKey) -> String {
    key.algorithm().to_string()
}

/// The token OpenSSH uses for a host in known_hosts: `host` on port 22,
/// `[host]:port` otherwise.
pub fn host_token(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{}]:{}", host, port)
    }
}

fn try_learn(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: &Path,
    fp: &str,
    kt: &str,
) -> VerifyOutcome {
    match learn_known_hosts_path(host, port, key, path) {
        Ok(()) => VerifyOutcome::Learned {
            fingerprint: fp.to_string(),
            key_type: kt.to_string(),
        },
        Err(e) => {
            log::warn!("写入 known_hosts 失败 ({}): {}", path.display(), e);
            VerifyOutcome::Unknown {
                fingerprint: fp.to_string(),
                key_type: kt.to_string(),
            }
        }
    }
}

pub fn verify(host: &str, port: u16, key: &PublicKey, policy: &str, path: &Path) -> VerifyOutcome {
    let fp = fingerprint(key);
    let kt = key_type(key);

    if policy == "accept_any" {
        return VerifyOutcome::Skipped {
            fingerprint: fp,
            key_type: kt,
        };
    }

    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => VerifyOutcome::Trusted,
        Ok(false) => {
            if policy == "accept_new" {
                try_learn(host, port, key, path, &fp, &kt)
            } else {
                VerifyOutcome::Unknown {
                    fingerprint: fp,
                    key_type: kt,
                }
            }
        }
        Err(russh::keys::Error::KeyChanged { line }) => {
            let expected = known_host_keys_path(host, port, path)
                .ok()
                .and_then(|v| v.into_iter().next().map(|(_, k)| fingerprint(&k)))
                .unwrap_or_else(|| "(无法读取)".to_string());
            log::warn!(
                "主机密钥与 known_hosts 不符 ({}:{} 第 {} 行): 记录={} 实际={}",
                host,
                port,
                line,
                expected,
                fp
            );
            VerifyOutcome::Changed {
                fingerprint: fp,
                expected,
                key_type: kt,
            }
        }
        Err(e) => {
            // known_hosts missing or unreadable (first run) — treat as unknown.
            log::debug!("known_hosts 不可用 ({}): {}", path.display(), e);
            if policy == "accept_new" {
                try_learn(host, port, key, path, &fp, &kt)
            } else {
                VerifyOutcome::Unknown {
                    fingerprint: fp,
                    key_type: kt,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KnownHostEntry {
    pub host: String,
    pub key_type: String,
    pub fingerprint: String,
    pub line: usize,
}

/// Parse our known_hosts for display. Hashed entries (`|1|...`) cannot be
/// reversed, so they are listed with an empty `host` and skipped for removal
/// by name.
pub fn list(path: &Path) -> Vec<KnownHostEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let key = PublicKey::from_openssh(&f[1..].join(" "));
        out.push(KnownHostEntry {
            host: f[0].to_string(),
            key_type: f[1].to_string(),
            fingerprint: key
                .map(|k| fingerprint(&k))
                .unwrap_or_else(|_| "(解析失败)".to_string()),
            line: i + 1,
        });
    }
    out
}

/// Drop every plain (non-hashed) entry for `host:port`. Used after the user
/// explicitly accepts a changed key.
pub fn remove(path: &Path, host: &str, port: u16) -> std::io::Result<usize> {
    let token = host_token(host, port);
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        let matches = first
            .split(',')
            .any(|h| h == token || (port == 22 && h == host));
        if !line.trim().is_empty() && !line.trim_start().starts_with('#') && matches {
            removed += 1;
            continue;
        }
        kept.push(line);
    }
    if removed > 0 {
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        fs::write(path, body)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_token_matches_openssh_convention() {
        assert_eq!(host_token("example.com", 22), "example.com");
        assert_eq!(host_token("example.com", 2222), "[example.com]:2222");
    }

    #[test]
    fn remove_drops_only_the_matching_entry() {
        let dir = std::env::temp_dir().join(format!("sshbox-kh-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("known_hosts");
        fs::write(
            &p,
            "a.example ssh-ed25519 AAAA\n\
             # comment\n\
             [b.example]:2222 ssh-rsa BBBB\n\
             c.example,d.example ssh-ed25519 CCCC\n",
        )
        .unwrap();

        // port 22 for a.example → drops only that line
        assert_eq!(remove(&p, "a.example", 22).unwrap(), 1);
        let after = fs::read_to_string(&p).unwrap();
        assert!(!after.contains("a.example"));
        assert!(after.contains("# comment"));
        assert!(after.contains("[b.example]:2222"));

        // comma-separated list entry matches on the exact token
        assert_eq!(remove(&p, "c.example", 22).unwrap(), 1);
        let after = fs::read_to_string(&p).unwrap();
        assert!(!after.contains("c.example"));

        // non-default port needs the bracket form
        assert_eq!(remove(&p, "b.example", 22).unwrap(), 0);
        assert_eq!(remove(&p, "b.example", 2222).unwrap(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skips_comments_and_short_lines() {
        let dir = std::env::temp_dir().join(format!("sshbox-kh-list-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("known_hosts");
        fs::write(&p, "# header\n\ngarbage\nhost ssh-ed25519 not-a-key\n").unwrap();
        let entries = list(&p);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "host");
        assert_eq!(entries[0].line, 4);
        let _ = fs::remove_dir_all(&dir);
    }
}
