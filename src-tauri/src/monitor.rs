//! Agentless /proc collector.
//!
//! One exec channel per sample runs a single POSIX shell script that prints
//! `@@NAME@@` section markers; this module parses it into `HashMap<section,
//! Vec<line>>` and turns raw counters into rates by differencing against the
//! previous sample. No dependency on `top`/`vmstat` (format differs per distro,
//! busybox images often lack them) and no disturbance to the user's PTY.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use russh::client;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::ssh::{ClientHandler, DiskUsage, SessionId, StaticInfo};
use crate::store;

/// Per-session monitor task control.
struct TaskHandle {
    visible: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

static REGISTRY: LazyLock<StdMutex<HashMap<SessionId, TaskHandle>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn registry() -> MutexGuard<'static, HashMap<SessionId, TaskHandle>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

static MONO_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Seconds since process start. Used for *all* rate math: the remote clock may
/// be unsynced, and a sampling window is never exactly the nominal interval.
fn mono_secs() -> f64 {
    MONO_START.elapsed().as_secs_f64()
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
/// `date +%s.%N` is GNU-only, so a busybox `date` must not break the sample —
/// the timestamp is display-only and the Rust side falls back to its own clock.
const COLLECT_SCRIPT: &str = r#"
echo "@@TS@@ $(date +%s.%N 2>/dev/null || date +%s)"
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
            Some(russh::ChannelMsg::Data { ref data }) => out.extend_from_slice(&data[..]),
            Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                out.extend_from_slice(&data[..])
            }
            Some(russh::ChannelMsg::Eof) => {}
            Some(russh::ChannelMsg::ExitStatus { .. }) => {}
            Some(russh::ChannelMsg::Close) | None => break,
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

// ---------------------------------------------------------------------------
// df filtering
// ---------------------------------------------------------------------------

/// Pseudo/virtual mounts that are noise in a disk panel.
const SKIP_MOUNT_PREFIXES: &[&str] = &[
    "/mnt/",        // WSL host drives (C:, D: …), USB, network shares
    "/snap/",       // snap squashfs images
    "/run/",        // tmpfs + runtime
    "/sys/",        // kernel pseudo-fs
    "/proc/",       // kernel pseudo-fs
    "/dev/",        // devtmpfs
    "/usr/lib/wsl", // WSL driver mount (can read 90%+ and dominate the panel)
    "/var/lib/docker/",
];

/// Decide whether a `df -P -k` row is a disk the user cares about.
///
/// Filtering on the *device* (old behaviour: `starts_with('/')`) silently
/// dropped the root row in containers (`overlay`) and on WSL (`none`), so the
/// decision is made from the mountpoint plus a real-filesystem check.
fn keep_mount(device: &str, mount: &str) -> bool {
    if mount == "/" {
        return true;
    }
    if SKIP_MOUNT_PREFIXES.iter().any(|p| mount.starts_with(p)) {
        return false;
    }
    match device {
        "tmpfs" | "devtmpfs" | "none" | "rootfs" | "udev" => return false,
        _ => {}
    }
    device.starts_with("/dev/") || device == "overlay" || device.contains("zfs") || device.contains("pool")
}

fn parse_df(lines: Option<&Vec<String>>) -> Vec<DiskUsage> {
    let mut disks = Vec::new();
    if let Some(ls) = lines {
        for line in ls {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 6 && f[0] != "Filesystem" && keep_mount(f[0], f[5]) {
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

// ---------------------------------------------------------------------------
// Whole-disk detection
// ---------------------------------------------------------------------------

/// `/proc/diskstats` lists partitions too. Keeping everything that starts with
/// "sd" renders one Ubuntu disk as `sda` + `sda1` + `sda14` + `sda15`.
fn is_whole_disk(name: &str) -> bool {
    // sd/vd/xvd/hd style: disk letters followed by optional partition digits.
    // `sda` = disk, `sda1`/`sda14` = partitions of it.
    let prefix_style = |prefix: &str| -> Option<bool> {
        let rest = name.strip_prefix(prefix)?;
        let letters: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if letters.is_empty() {
            return Some(false); // "sdb" handled by other prefixes
        }
        let digits = &rest[letters.len()..];
        if digits.is_empty() {
            Some(true) // whole disk
        } else if digits.chars().all(|c| c.is_ascii_digit()) {
            Some(false) // partition
        } else {
            Some(false)
        }
    };
    for p in ["xvd", "sd", "vd", "hd"] {
        if let Some(whole) = prefix_style(p) {
            return whole;
        }
    }
    // nvme0n1 / nvme0n1p1, mmcblk0 / mmcblk0p1
    for p in ["nvme", "mmcblk"] {
        if let Some(rest) = name.strip_prefix(p) {
            if let Some((_, part)) = rest.split_once('p') {
                if !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) {
                    return false; // partition
                }
            }
            return true;
        }
    }
    if name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("dm-")
        || name.starts_with("sr")
    {
        return false;
    }
    name.starts_with("md")
}

// ---------------------------------------------------------------------------
// Static info
// ---------------------------------------------------------------------------

pub async fn collect_static(handle: &client::Handle<ClientHandler>) -> Result<StaticInfo> {
    let raw = exec_capture(handle, STATIC_SCRIPT).await?;
    let s = sections(&raw);

    let hostname = s
        .get("HOST")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    let kernel = s
        .get("KERNEL")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    let arch = s
        .get("ARCH")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();

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

// ---------------------------------------------------------------------------
// Live metrics
// ---------------------------------------------------------------------------

/// Previous-sample state for differential computation.
#[derive(Default)]
struct PrevSample {
    /// Explicit baseline flag — a magic `mono_ts > 0.0` test misreads a
    /// deliberately back-dated timestamp as "no baseline yet".
    has_baseline: bool,
    /// Local monotonic clock at the previous sample — the only clock used for dt.
    mono_ts: f64,
    cpu_total: u64,
    cpu_idle: u64,
    per_core: Vec<(u64, u64)>,          // (total, idle)
    net: HashMap<String, (u64, u64)>,   // rx, tx bytes
    diskio: HashMap<String, (u64, u64)>, // read sectors, write sectors
}

fn parse_remote_ts(s: &HashMap<String, Vec<String>>) -> Option<f64> {
    let raw = s.get("TS").and_then(|v| v.first())?;
    // busybox may emit "1789779039.N" or similar; take the leading number.
    let cleaned: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    cleaned.parse::<f64>().ok().filter(|t| *t > 0.0)
}

fn parse_metrics(raw: &str, prev: &mut PrevSample) -> Option<Metrics> {
    let s = sections(raw);

    let now = mono_secs();
    let dt = if prev.has_baseline {
        (now - prev.mono_ts).max(0.0)
    } else {
        0.0
    };
    // Display timestamp: prefer the VM's own clock, fall back to ours.
    let ts = parse_remote_ts(&s).unwrap_or(now);
    prev.mono_ts = now;
    prev.has_baseline = true;

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
                    let d = tot.saturating_sub(*ptot);
                    let di = idle.saturating_sub(*pidle);
                    if d > 0 {
                        cpu_per_core.push((d - di) as f64 / d as f64 * 100.0);
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
            net.push(NetIf {
                name,
                rx_bps,
                tx_bps,
            });
        }
    }

    // --- Disk I/O from /proc/diskstats (whole disks only) ---
    let mut disk_io = Vec::new();
    if let Some(ds) = s.get("DISKIO") {
        for line in ds {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let name = f[2].to_string();
            if !is_whole_disk(&name) {
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
            disk_io.push(DiskIo {
                name,
                read_bps: rbps,
                write_bps: wbps,
            });
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

// ---------------------------------------------------------------------------
// Task lifecycle
// ---------------------------------------------------------------------------

pub fn spawn(
    sid: SessionId,
    handle: Arc<client::Handle<ClientHandler>>,
    closed: Arc<AtomicBool>,
    app: AppHandle,
) {
    stop(&sid);

    let visible = Arc::new(AtomicBool::new(true));
    let stop_flag = Arc::new(AtomicBool::new(false));
    registry().insert(
        sid.clone(),
        TaskHandle {
            visible: visible.clone(),
            stop: stop_flag.clone(),
        },
    );

    tokio::spawn(async move {
        // One-shot static info
        match collect_static(&handle).await {
            Ok(info) => {
                let _ = app.emit("ssh://static", serde_json::json!({ "sid": sid, "info": info }));
            }
            Err(e) => log::warn!("静态信息采集失败: {:#}", e),
        }

        let mut prev = PrevSample::default();
        loop {
            let interval = store::load_settings().sample_interval_secs.clamp(1, 60);
            // Recompute the wait each pass so hidden sessions slow down without
            // accumulating drift the way `interval.tick()` + extra sleep does.
            let wait = if visible.load(Ordering::SeqCst) {
                interval
            } else {
                interval * 5
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if stop_flag.load(Ordering::SeqCst) || closed.load(Ordering::SeqCst) {
                break;
            }
            let raw = match exec_capture(&handle, COLLECT_SCRIPT).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("采集失败，监控任务退出: {:#}", e);
                    break;
                }
            };
            if let Some(m) = parse_metrics(&raw, &mut prev) {
                let _ = app.emit("ssh://metrics", serde_json::json!({ "sid": sid, "metrics": m }));
            }
        }
        registry().remove(&sid);
    });
}

pub fn stop(sid: &SessionId) {
    if let Some(t) = registry().remove(sid) {
        t.stop.store(true, Ordering::SeqCst);
    }
}

pub fn set_visible(sid: &SessionId, visible: bool) {
    if let Some(t) = registry().get(sid) {
        t.visible.store(visible, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn partitions_are_not_whole_disks() {
        for name in ["sda", "vda", "xvdb", "nvme0n1", "mmcblk0", "md0"] {
            assert!(is_whole_disk(name), "{} should be a whole disk", name);
        }
        for name in [
            "sda1", "sda14", "sda15", "vda2", "xvdb3", "nvme0n1p1", "mmcblk0p1", "loop0", "ram3",
            "dm-0", "sr0",
        ] {
            assert!(!is_whole_disk(name), "{} should be filtered out", name);
        }
    }

    #[test]
    fn df_keeps_root_and_drops_noise() {
        let rows = lines(&[
            "Filesystem     1024-blocks      Used Available Capacity Mounted on",
            "none               4029616         4   4029612       1% /mnt/wsl",
            "drivers          209715196 191965376  17749820      92% /usr/lib/wsl/drivers",
            "/dev/sdc        1055762868   2311604 999747792       1% /",
            "tmpfs                 4096         0      4096       0% /sys/fs/cgroup",
            "C:\\              209715196 191965376  17749820      92% /mnt/c",
            "/dev/sdb1        1048576000  10485760 1038090240       1% /data",
            "overlay          104857600   52428800  52428800      50% /var/lib/docker/overlay2",
            "overlay           52428800   26214400  26214400      50% /var/lib/containers",
        ]);
        let disks = parse_df(Some(&rows));
        let mounts: Vec<&str> = disks.iter().map(|d| d.mount.as_str()).collect();
        assert!(mounts.contains(&"/"), "root must survive: {:?}", mounts);
        assert!(mounts.contains(&"/data"));
        assert!(!mounts.contains(&"/mnt/c"), "WSL host mount must be dropped");
        assert!(!mounts.contains(&"/usr/lib/wsl/drivers"));
        assert!(!mounts.contains(&"/sys/fs/cgroup"));
        assert!(!mounts.iter().any(|m| m.starts_with("/var/lib/docker")));
        assert_eq!(disks.len(), 3, "got {:?}", mounts);
    }

    #[test]
    fn ts_falls_back_to_local_clock_when_remote_is_junk() {
        let mut s: HashMap<String, Vec<String>> = HashMap::new();
        s.insert("TS".into(), lines(&["1789779039.N"]));
        assert!(parse_remote_ts(&s).is_some());
        s.insert("TS".into(), lines(&["not-a-time"]));
        assert!(parse_remote_ts(&s).is_none());
        s.insert("TS".into(), lines(&["0"]));
        assert!(parse_remote_ts(&s).is_none());
    }

    #[test]
    fn rates_use_local_monotonic_clock() {
        let raw1 = "@@TS@@ 100.0\n@@STAT@@\ncpu  100 0 100 800 0 0 0 0 0 0\n@@NET@@\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n@@END@@\n";
        // 2s later: total ticks 1000→3000, idle 800→1800 ⇒ 50% busy.
        let raw2 = "@@TS@@ 102.0\n@@STAT@@\ncpu  600 0 600 1800 0 0 0 0 0 0\n@@NET@@\n  eth0: 3000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n@@END@@\n";
        let mut prev = PrevSample::default();
        let m1 = parse_metrics(raw1, &mut prev).unwrap();
        assert_eq!(m1.cpu_pct, 0.0, "first sample has no baseline");
        assert_eq!(m1.net[0].rx_bps, 0.0);
        // Back-date the baseline so dt is exactly 2s regardless of wall time.
        prev.mono_ts = mono_secs() - 2.0;
        assert!(prev.has_baseline, "first sample must arm the baseline");
        let m2 = parse_metrics(raw2, &mut prev).unwrap();
        assert!((m2.cpu_pct - 50.0).abs() < 1.0, "cpu={}", m2.cpu_pct);
        assert!((m2.net[0].rx_bps - 1000.0).abs() < 1.0, "rx={}", m2.net[0].rx_bps);
        assert!((m2.net[0].tx_bps - 2000.0).abs() < 1.0);
        assert_eq!(m2.ts, 102.0, "display ts stays the remote one");
    }

    #[test]
    fn missing_remote_ts_still_yields_rates() {
        // busybox `date +%s.%N` prints garbage ⇒ no usable @@TS@@ value.
        // Totals 1000→2000 ticks, idle 500→1000 over 1s ⇒ 50% busy.
        let raw1 = "@@TS@@ N\n@@STAT@@\ncpu  250 0 250 500 0 0 0 0 0 0\n@@END@@\n";
        let raw2 = "@@STAT@@\ncpu  500 0 500 1000 0 0 0 0 0 0\n@@END@@\n";
        let mut prev = PrevSample::default();
        let m1 = parse_metrics(raw1, &mut prev).expect("sample must survive a bad timestamp");
        assert!(m1.ts > 0.0, "falls back to the local clock");
        prev.mono_ts = mono_secs() - 1.0;
        let m2 = parse_metrics(raw2, &mut prev).expect("second sample must survive");
        assert!((m2.cpu_pct - 50.0).abs() < 1.0, "cpu={}", m2.cpu_pct);
    }
}

/// End-to-end tests against a real SSH server.
///
/// Skipped (and passing) unless SSHBOX_TEST_HOST + SSHBOX_TEST_PASSWORD are
/// set, so `cargo test` stays green on machines without a VM:
///
/// ```bash
/// SSHBOX_TEST_HOST=127.0.0.1 SSHBOX_TEST_PASSWORD=sshbox123 \
///   cargo test --lib live -- --nocapture
/// ```
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::ssh::ClientHandler;
    use russh::client;

    fn test_target() -> Option<(String, u16, String, String)> {
        let host = std::env::var("SSHBOX_TEST_HOST").ok()?;
        let port = std::env::var("SSHBOX_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22);
        let user = std::env::var("SSHBOX_TEST_USER").unwrap_or_else(|_| "root".to_string());
        let pw = std::env::var("SSHBOX_TEST_PASSWORD").ok()?;
        Some((host, port, user, pw))
    }

    async fn connect_for_test(
        host: &str,
        port: u16,
        user: &str,
        pw: &str,
        policy: &str,
        known_hosts_path: std::path::PathBuf,
    ) -> Result<client::Handle<ClientHandler>, String> {
        let handler = ClientHandler {
            host: host.to_string(),
            port,
            policy: policy.to_string(),
            known_hosts_path,
            outcome: Default::default(),
        };
        let config = Arc::new(client::Config::default());
        let mut h = client::connect(config, (host, port), handler)
            .await
            .map_err(|e| format!("connect: {}", e))?;
        let ok = h
            .authenticate_password(user, pw)
            .await
            .map_err(|e| format!("auth: {}", e))?;
        if !ok.success() {
            return Err("auth rejected".to_string());
        }
        Ok(h)
    }

    #[tokio::test]
    async fn live_vm_static_info_and_metrics() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };

        let kh = std::env::temp_dir().join(format!(
            "sshbox-live-known_hosts-{}-{}",
            std::process::id(),
            port
        ));
        let _ = std::fs::remove_file(&kh);
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.clone())
            .await
            .expect("连接失败（WSL 是否在运行？sshd 是否在监听？）");
        assert!(kh.exists(), "accept_new 应当把主机密钥写入 known_hosts");

        // --- static info ---
        let info = collect_static(&h).await.expect("静态信息采集失败");
        eprintln!(
            "静态: hostname={} os={} kernel={} cores={} mem={}kB disks={:?}",
            info.hostname,
            info.os_pretty,
            info.kernel,
            info.cpu_cores,
            info.mem_total_kb,
            info.disks.iter().map(|d| d.mount.clone()).collect::<Vec<_>>()
        );
        assert!(!info.hostname.is_empty(), "hostname 为空");
        assert!(!info.os_pretty.is_empty(), "发行版信息为空");
        assert!(info.cpu_cores >= 1, "CPU 核数异常");
        assert!(info.mem_total_kb > 0, "内存总量为 0");
        assert!(!info.disks.is_empty(), "没有解析出任何磁盘（根分区必须存在）");
        assert!(
            info.disks.iter().any(|d| d.mount == "/"),
            "根分区缺失: {:?}",
            info.disks.iter().map(|d| &d.mount).collect::<Vec<_>>()
        );

        // --- two live samples → rates ---
        let raw1 = exec_capture(&h, COLLECT_SCRIPT).await.expect("第一次采样失败");
        let mut prev = PrevSample::default();
        let m1 = parse_metrics(&raw1, &mut prev).expect("第一次采样解析失败");
        assert_eq!(m1.cpu_pct, 0.0, "首帧不该有 CPU 速率");

        tokio::time::sleep(Duration::from_millis(1200)).await;
        let raw2 = exec_capture(&h, COLLECT_SCRIPT).await.expect("第二次采样失败");
        let m2 = parse_metrics(&raw2, &mut prev).expect("第二次采样解析失败");

        eprintln!(
            "实时: cpu={:.1}% 每核={:?} mem={:.1}% swap={}/{} net={:?} 磁盘IO={:?} load={:?} ts={}",
            m2.cpu_pct,
            m2.cpu_per_core,
            m2.mem_pct,
            m2.swap_used_kb,
            m2.swap_total_kb,
            m2.net
                .iter()
                .map(|n| format!("{}↓{:.0}↑{:.0}", n.name, n.rx_bps, n.tx_bps))
                .collect::<Vec<_>>(),
            m2.disk_io
                .iter()
                .map(|d| format!("{} r{:.0} w{:.0}", d.name, d.read_bps, d.write_bps))
                .collect::<Vec<_>>(),
            m2.load,
            m2.ts
        );

        assert!(m2.mem_total_kb > 0, "内存总量为 0");
        assert!((0.0..=100.0).contains(&m2.mem_pct), "内存百分比越界: {}", m2.mem_pct);
        assert!(m2.cpu_pct >= 0.0 && m2.cpu_pct <= 100.0, "CPU 百分比越界: {}", m2.cpu_pct);
        assert!(!m2.cpu_per_core.is_empty(), "没有每核数据");
        assert!(!m2.net.is_empty(), "没有网卡数据");
        assert!(!m2.disks.is_empty(), "没有磁盘数据");
        assert!(m2.ts > 0.0, "时间戳无效");
        // Whole disks only — a partition row here means the filter regressed.
        for d in &m2.disk_io {
            assert!(
                is_whole_disk(&d.name),
                "磁盘 I/O 里出现了分区: {}",
                d.name
            );
        }
    }

    #[tokio::test]
    async fn live_host_key_verification_flow() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 主机密钥测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        // Never touch the user's real known_hosts.
        let kh = std::env::temp_dir().join(format!(
            "sshbox-test-known_hosts-{}-{}",
            std::process::id(),
            port
        ));
        let _ = std::fs::remove_file(&kh);

        // 1) strict + no record ⇒ the connect must be refused (Unknown).
        let strict = connect_for_test(&host, port, &user, &pw, "strict", kh.clone()).await;
        assert!(
            strict.is_err(),
            "known_hosts 为空时严格模式必须拒绝连接，实际却成功了"
        );
        assert!(!kh.exists(), "严格模式不该写入 known_hosts");

        // 2) accept_new ⇒ records the key and connects.
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.clone())
            .await
            .expect("接受新密钥后应当连接成功");
        assert!(kh.exists(), "accept_new 应当写入 known_hosts 文件");
        let recorded = std::fs::read_to_string(&kh).unwrap();
        assert!(recorded.contains(&host), "known_hosts 未记录主机: {}", recorded);
        drop(h);

        // 3) strict again ⇒ now trusted, connects fine.
        connect_for_test(&host, port, &user, &pw, "strict", kh.clone())
            .await
            .expect("已记录密钥后严格模式应当通过");

        let entries = crate::hostkey::list(&kh);
        assert_eq!(entries.len(), 1, "应恰好记录一条主机密钥: {:?}", entries);
        assert!(
            entries[0].fingerprint.starts_with("SHA256:"),
            "指纹格式异常: {}",
            entries[0].fingerprint
        );

        // 4) remove ⇒ the record is gone and strict refuses again.
        let removed = crate::hostkey::remove(&kh, &host, port).expect("删除 known_hosts 记录失败");
        assert_eq!(removed, 1);
        assert!(
            connect_for_test(&host, port, &user, &pw, "strict", kh.clone()).await.is_err(),
            "删除记录后严格模式应当重新拒绝"
        );

        let _ = std::fs::remove_file(&kh);
    }
}
