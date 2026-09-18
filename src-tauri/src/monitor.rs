use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use russh::client;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::ssh::{ClientHandler, DiskUsage, SessionId, StaticInfo};

/// Visibility registry: monitor tasks poll slower when their tab is hidden.
static VISIBILITY_MAP: StdMutex<HashMap<SessionId, Arc<AtomicBool>>> =
    StdMutex::new(HashMap::new());

fn vis_lock() -> MutexGuard<'static, HashMap<SessionId, Arc<AtomicBool>>> {
    VISIBILITY_MAP.lock().unwrap()
}

#[derive(Debug, Serialize, Clone)]
pub struct Metrics {
    pub ts: f64,
    pub cpu_pct: f64,
    pub cpu_per_core: Vec<f64>,
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
    pub mem_pct: f64,
    pub swap_total_kb: u64,
    pub swap_used_kb: u64,
    /// interface -> (rx_bytes_per_sec, tx_bytes_per_sec)
    pub net: Vec<NetIf>,
    pub disk_io: Vec<DiskIo>,
    pub disks: Vec<DiskUsage>,
    pub load: Vec<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct NetIf {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiskIo {
    pub name: String,
    pub read_bps: f64,
    pub write_bps: f64,
}

/// The shell script that collects everything in one shot. Pure POSIX + /proc.
const COLLECT_SCRIPT: &str = r#"
echo "@@TS@@ $(date +%s.%N)"
echo "@@STAT@@"; cat /proc/stat 2>/dev/null
echo "@@MEM@@"; cat /proc/meminfo 2>/dev/null
echo "@@NET@@"; cat /proc/net/dev 2>/dev/null
echo "@@DISKIO@@"; cat /proc/diskstats 2>/dev/null
echo "@@DF@@"; df -P -k 2>/dev/null
echo "@@LOAD@@"; cat /proc/loadavg 2>/dev/null
echo "@@END@@"
"#;

const STATIC_SCRIPT: &str = r#"
echo "@@HOST@@ $(hostname 2>/dev/null)"
echo "@@OS@@"; cat /etc/os-release 2>/dev/null
echo "@@KERNEL@@ $(uname -r 2>/dev/null)"
echo "@@ARCH@@ $(uname -m 2>/dev/null)"
echo "@@LSCPU@@"; lscpu 2>/dev/null
echo "@@CPUINFO@@"; head -40 /proc/cpuinfo 2>/dev/null
echo "@@MEMTOT@@ $(grep MemTotal /proc/meminfo 2>/dev/null)"
echo "@@UPTIME@@ $(cat /proc/uptime 2>/dev/null)"
echo "@@DF@@"; df -P -k 2>/dev/null
echo "@@END@@"
"#;

/// Run a one-shot exec command over the SSH connection, return combined stdout.
async fn exec_capture(handle: &client::Handle<ClientHandler>, cmd: &str) -> Result<String> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;

    let mut out = Vec::new();
    loop {
        match channel.wait().await {
            Ok(Some(russh::ChannelMsg::Data { ref data })) => out.extend_from_slice(&data[..]),
            Ok(Some(russh::ChannelMsg::ExtendedData { ref data, .. })) => {
                out.extend_from_slice(&data[..])
            }
            Ok(Some(russh::ChannelMsg::Eof)) => {}
            Ok(Some(russh::ChannelMsg::ExitStatus { .. })) => {}
            Ok(Some(russh::ChannelMsg::Close)) | Ok(None) | Err(_) => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// Split the KEYED script output into sections by @@NAME@@ markers.
fn sections(text: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_end();
        if t.starts_with("@@") && t.ends_with("@@") {
            current = Some(t[2..t.len() - 2].to_string());
            map.entry(current.clone().unwrap()).or_default();
        } else if t.starts_with("@@") {
            // "@@NAME@@ value" form
            let rest = &t[2..];
            if let Some(idx) = rest.find("@@") {
                let name = rest[..idx].to_string();
                let val = rest[idx + 2..].trim().to_string();
                map.entry(name).or_default().push(val);
                current = None;
            }
        } else if let Some(c) = &current {
            map.entry(c.clone()).or_default().push(t.to_string());
        }
    }
    map
}

pub async fn collect_static(handle: &client::Handle<ClientHandler>) -> Result<StaticInfo> {
    let raw = exec_capture(handle, STATIC_SCRIPT).await?;
    let s = sections(&raw);

    let hostname = s.get("HOST").and_then(|v| v.first()).cloned().unwrap_or_default();
    let kernel = s.get("KERNEL").and_then(|v| v.first()).cloned().unwrap_or_default();
    let arch = s.get("ARCH").and_then(|v| v.first()).cloned().unwrap_or_default();

    let mut os_pretty = String::new();
    if let Some(os) = s.get("OS") {
        for line in os {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                os_pretty = v.trim_matches('"').to_string();
            }
        }
    }

    let mut cpu_model = String::new();
    let mut cpu_cores = 0u32;
    if let Some(ls) = s.get("LSCPU") {
        for line in ls {
            if let Some(v) = line.strip_prefix("Model name:") {
                cpu_model = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("CPU(s):") {
                cpu_cores = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if cpu_cores == 0 {
        if let Some(ci) = s.get("CPUINFO") {
            for line in ci {
                if line.starts_with("processor") {
                    cpu_cores += 1;
                }
                if cpu_model.is_empty() {
                    if let Some(v) = line.strip_prefix("model name") {
                        if let Some(after) = v.split_once(':') {
                            cpu_model = after.1.trim().to_string();
                        }
                    }
                }
            }
        }
    }

    let mem_total_kb = s
        .get("MEMTOT")
        .and_then(|v| v.first())
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);

    let uptime_secs = s
        .get("UPTIME")
        .and_then(|v| v.first())
        .and_then(|line| line.split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(0.0) as u64;

    let disks = parse_df(s.get("DF"));

    Ok(StaticInfo {
        hostname,
        os_pretty,
        kernel,
        arch,
        cpu_model,
        cpu_cores,
        mem_total_kb,
        uptime_secs,
        disks,
    })
}

fn parse_df(lines: Option<&Vec<String>>) -> Vec<DiskUsage> {
    let mut disks = Vec::new();
    if let Some(ls) = lines {
        for line in ls {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 6 && f[0] != "Filesystem" && f[0].starts_with('/') {
                let total = f[1].parse::<u64>().unwrap_or(0);
                let used = f[2].parse::<u64>().unwrap_or(0);
                let pct = if total > 0 {
                    used as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                disks.push(DiskUsage {
                    mount: f[5].to_string(),
                    total_kb: total,
                    used_kb: used,
                    use_pct: pct,
                });
            }
        }
    }
    disks
}

/// Previous-sample state for differential computation.
#[derive(Default)]
struct PrevSample {
    ts: f64,
    cpu_total: u64,
    cpu_idle: u64,
    per_core: Vec<(u64, u64)>, // (total, idle)
    net: HashMap<String, (u64, u64)>, // rx, tx bytes
    diskio: HashMap<String, (u64, u64)>, // read sectors, write sectors
}

fn parse_metrics(raw: &str, prev: &mut PrevSample) -> Option<Metrics> {
    let s = sections(raw);

    let ts = s
        .get("TS")
        .and_then(|v| v.first())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(0.0);
    if ts == 0.0 {
        return None;
    }

    let dt = if prev.ts > 0.0 { ts - prev.ts } else { 0.0 };

    // --- CPU from /proc/stat ---
    let mut cpu_pct = 0.0;
    let mut cpu_per_core = Vec::new();
    if let Some(stat) = s.get("STAT") {
        let mut total_now = 0u64;
        let mut idle_now = 0u64;
        let mut per_now: Vec<(u64, u64)> = Vec::new();
        for line in stat {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.is_empty() {
                continue;
            }
            if f[0] == "cpu" {
                let vals: Vec<u64> = f[1..].iter().filter_map(|x| x.parse().ok()).collect();
                let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
                let tot: u64 = vals.iter().sum();
                total_now = tot;
                idle_now = idle;
            } else if f[0].starts_with("cpu") {
                let vals: Vec<u64> = f[1..].iter().filter_map(|x| x.parse().ok()).collect();
                let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
                let tot: u64 = vals.iter().sum();
                per_now.push((tot, idle));
            }
        }
        if dt > 0.0 && prev.cpu_total > 0 {
            let dtot = total_now.saturating_sub(prev.cpu_total);
            let didle = idle_now.saturating_sub(prev.cpu_idle);
            if dtot > 0 {
                cpu_pct = (dtot - didle) as f64 / dtot as f64 * 100.0;
            }
            for (i, (tot, idle)) in per_now.iter().enumerate() {
                if let Some((ptot, pidle)) = prev.per_core.get(i) {
                    let dt = tot.saturating_sub(*ptot);
                    let di = idle.saturating_sub(*pidle);
                    if dt > 0 {
                        cpu_per_core.push((dt - di) as f64 / dt as f64 * 100.0);
                    }
                }
            }
        }
        prev.cpu_total = total_now;
        prev.cpu_idle = idle_now;
        prev.per_core = per_now;
    }

    // --- Memory ---
    let (mut mem_total, mut mem_avail, mut buffers, mut cached) = (0u64, 0u64, 0u64, 0u64);
    let (mut swap_total, mut swap_free) = (0u64, 0u64);
    let mut has_avail = false;
    if let Some(mem) = s.get("MEM") {
        for line in mem {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 2 {
                continue;
            }
            let v = f[1].parse::<u64>().unwrap_or(0);
            match f[0] {
                "MemTotal:" => mem_total = v,
                "MemAvailable:" => {
                    mem_avail = v;
                    has_avail = true;
                }
                "Buffers:" => buffers = v,
                "Cached:" => cached = v,
                "SwapTotal:" => swap_total = v,
                "SwapFree:" => swap_free = v,
                _ => {}
            }
        }
    }
    let mem_used = if has_avail {
        mem_total.saturating_sub(mem_avail)
    } else {
        mem_total.saturating_sub(buffers + cached)
    };
    let mem_pct = if mem_total > 0 {
        mem_used as f64 / mem_total as f64 * 100.0
    } else {
        0.0
    };
    let swap_used = swap_total.saturating_sub(swap_free);

    // --- Network from /proc/net/dev ---
    let mut net = Vec::new();
    if let Some(nd) = s.get("NET") {
        for line in nd {
            if !line.contains(':') {
                continue;
            }
            let (name, rest) = line.split_once(':').unwrap();
            let name = name.trim().to_string();
            if name == "lo" {
                continue;
            }
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 16 {
                continue;
            }
            let rx = f[0].parse::<u64>().unwrap_or(0);
            let tx = f[8].parse::<u64>().unwrap_or(0);
            let (rx_bps, tx_bps) = if dt > 0.0 {
                if let Some((prx, ptx)) = prev.net.get(&name) {
                    (
                        rx.saturating_sub(*prx) as f64 / dt,
                        tx.saturating_sub(*ptx) as f64 / dt,
                    )
                } else {
                    (0.0, 0.0)
                }
            } else {
                (0.0, 0.0)
            };
            prev.net.insert(name.clone(), (rx, tx));
            net.push(NetIf { name, rx_bps, tx_bps });
        }
    }

    // --- Disk I/O from /proc/diskstats ---
    let mut disk_io = Vec::new();
    if let Some(ds) = s.get("DISKIO") {
        for line in ds {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let name = f[2].to_string();
            // Skip partitions (keep whole disks: no trailing digit, or nvmeXnY)
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("dm-") {
                continue;
            }
            let read_sectors = f[5].parse::<u64>().unwrap_or(0);
            let write_sectors = f[9].parse::<u64>().unwrap_or(0);
            let (rbps, wbps) = if dt > 0.0 {
                if let Some((pr, pw)) = prev.diskio.get(&name) {
                    (
                        read_sectors.saturating_sub(*pr) as f64 * 512.0 / dt,
                        write_sectors.saturating_sub(*pw) as f64 * 512.0 / dt,
                    )
                } else {
                    (0.0, 0.0)
                }
            } else {
                (0.0, 0.0)
            };
            prev.diskio.insert(name.clone(), (read_sectors, write_sectors));
            // Only report real block devices with activity or physical disks
            if name.starts_with("sd") || name.starts_with("vd") || name.starts_with("nvme") || name.starts_with("xvd") {
                disk_io.push(DiskIo { name, read_bps: rbps, write_bps: wbps });
            }
        }
    }

    // --- df ---
    let disks = parse_df(s.get("DF"));

    // --- loadavg ---
    let load = s
        .get("LOAD")
        .and_then(|v| v.first())
        .map(|line| {
            line.split_whitespace()
                .take(3)
                .filter_map(|x| x.parse::<f64>().ok())
                .collect()
        })
        .unwrap_or_default();

    Some(Metrics {
        ts,
        cpu_pct,
        cpu_per_core,
        mem_total_kb: mem_total,
        mem_used_kb: mem_used,
        mem_pct,
        swap_total_kb: swap_total,
        swap_used_kb: swap_used,
        net,
        disk_io,
        disks,
        load,
    })
}

pub fn spawn(
    sid: SessionId,
    handle: client::Handle<ClientHandlerPub>,
    closed: Arc<AtomicBool>,
    app: AppHandle,
    interval: Duration,
) {
    let visible = Arc::new(AtomicBool::new(true));
    vis_lock().insert(sid.clone(), visible.clone());

    tokio::spawn(async move {
        // One-shot static info
        match collect_static(&handle).await {
            Ok(info) => {
                let _ = app.emit("ssh://static", serde_json::json!({ "sid": sid, "info": info }));
            }
            Err(e) => log::warn!("static collect failed: {}", e),
        }

        let mut prev = PrevSample::default();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if closed.load(Ordering::SeqCst) {
                break;
            }
            // Throttle invisible sessions to 1/5 frequency
            if !visible.load(Ordering::SeqCst) {
                tokio::time::sleep(interval * 4).await;
            }
            let raw = match exec_capture(&handle, COLLECT_SCRIPT).await {
                Ok(r) => r,
                Err(_) => break, // connection likely dead
            };
            if let Some(m) = parse_metrics(&raw, &mut prev) {
                let _ = app.emit("ssh://metrics", serde_json::json!({ "sid": sid, "metrics": m }));
            }
        }
        vis_lock().remove(&sid);
    });
}

pub fn set_visible(sid: &SessionId, visible: bool) {
    if let Some(flag) = vis_lock().get(sid) {
        flag.store(visible, Ordering::SeqCst);
    }
}
