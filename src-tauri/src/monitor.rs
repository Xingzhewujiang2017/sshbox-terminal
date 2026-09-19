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
    /// User pressed pause: stop collecting but keep the task (and its SSH
    /// connection) alive so resuming is instant.
    paused: Arc<AtomicBool>,
    /// Wakes the sleep early so "sample now" is not stuck behind the interval.
    wake: Arc<tokio::sync::Notify>,
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

/// Unix wall-clock seconds — display only, never used for rate math.
fn wall_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
    /// Top processes by instantaneous CPU, sorted descending.
    pub processes: Vec<ProcInfo>,
    /// Total number of processes visible in /proc for this sample.
    pub proc_total: usize,
}

/// One process row.
///
/// CPU is instantaneous — Δ(utime+stime) over Δwall — expressed as a
/// percentage of ONE core, which is what `top` shows, so it can exceed 100%.
#[derive(Debug, Serialize, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub state: String,
    pub cpu_pct: f64,
    pub rss_kb: u64,
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

/// How often the slow service/port/container facts are refreshed.
const SERVICES_EVERY: Duration = Duration::from_secs(15);

/// The shell script that collects everything in one shot. Pure POSIX + /proc.
/// `date +%s.%N` is GNU-only, so a busybox `date` must not break the sample —
/// the timestamp is display-only and the Rust side falls back to its own clock.
const COLLECT_SCRIPT: &str = r#"
echo "@@TS@@ $(date +%s.%N 2>/dev/null || date +%s)"
echo "@@STAT@@"; cat /proc/stat 2>/dev/null
echo "@@MEM@@"; cat /proc/meminfo 2>/dev/null
echo "@@NET@@"; cat /proc/net/dev 2>/dev/null
echo "@@NETSTATE@@"
for f in /sys/class/net/*/operstate; do
  [ -r "$f" ] || continue
  n=${f#/sys/class/net/}; n=${n%/operstate}
  IFS= read -r st < "$f" 2>/dev/null
  echo "$n $st"
done
echo "@@DISKIO@@"; cat /proc/diskstats 2>/dev/null
echo "@@DF@@"; df -P -k 2>/dev/null
echo "@@LOAD@@"; cat /proc/loadavg 2>/dev/null
echo "@@SYS@@ hz=$(getconf CLK_TCK 2>/dev/null || echo 100) pagesize=$(getconf PAGESIZE 2>/dev/null || echo 4096)"
# Processes, builtins only (read/echo) so 200+ procs stay cheap.
# /proc/<pid>/stat fields after "pid (comm) ": 1=state 12=utime 13=stime 22=rss
echo "@@PROC@@"
for d in /proc/[0-9]*; do
  IFS= read -r st < "$d/stat" 2>/dev/null || continue
  comm=${st#*(}; comm=${comm%%)*}
  rest=${st##*) }
  [ -n "$rest" ] || continue
  set -- $rest
  # ${12} braces are mandatory: "$12" is $1 followed by a literal 2 in POSIX sh.
  echo "${d#/proc/}|$comm|$1|${12}|${13}|${22}"
done
echo "@@END@@"
"#;

/// The slow-moving facts: failed units, listening ports, containers. Run on a
/// separate, slower cadence — `systemctl` alone costs more than the whole fast
/// script, and none of this changes second to second.
const SERVICES_SCRIPT: &str = r#"
echo "@@SVC@@"
# UNIT LOAD ACTIVE SUB DESCRIPTION — --plain drops the tree glyphs. No --type
# filter: a failed .mount or .socket is exactly as important as a failed service.
systemctl --failed --no-legend --plain --no-pager 2>/dev/null | head -20
echo "@@PORTS@@"
# Normalised to proto|local|port|state so the Rust parser does not care whether
# this box ships iproute2 (ss) or only net-tools (netstat).
if command -v ss >/dev/null 2>&1; then
  ss -tulnH 2>/dev/null | awk '{n=split($5,a,":"); if (n>1) print $1"|"$5"|"a[n]"|"$2}' | head -80
elif command -v netstat >/dev/null 2>&1; then
  netstat -tuln 2>/dev/null | awk '/^(tcp|udp)/ {n=split($4,a,":"); if (n>1) print $1"|"$4"|"a[n]"|"$6}' | head -80
fi
echo "@@DOCKER@@"
# Socket check first: a missing daemon would otherwise make docker hang.
# The @@DOCKER_YES@@ sentinel is emitted only when `docker ps` really succeeded,
# so "no containers" and "no docker / no permission" stay distinguishable.
if [ -S /var/run/docker.sock ] && command -v docker >/dev/null 2>&1; then
  out=$(docker ps --format '{{.Names}}|{{.Image}}|{{.Status}}' 2>/dev/null)
  if [ $? -eq 0 ]; then
    echo "@@DOCKER_YES@@"
    printf '%s\n' "$out" | head -20
  fi
fi
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
    device.starts_with("/dev/")
        || device == "overlay"
        || device.contains("zfs")
        || device.contains("pool")
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
        let letters: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
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
    per_core: Vec<(u64, u64)>,           // (total, idle)
    net: HashMap<String, (u64, u64)>,    // rx, tx bytes
    diskio: HashMap<String, (u64, u64)>, // read sectors, write sectors
    procs: HashMap<u32, u64>,            // pid -> utime+stime ticks
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

/// Kernel constants from `@@SYS@@` — (CLK_TCK, page size). Both keep sane
/// defaults so a stripped-down busybox image still yields usable numbers.
fn parse_sys(s: &HashMap<String, Vec<String>>) -> (f64, f64) {
    let line = s
        .get("SYS")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    let mut hz = 100.0f64;
    let mut page = 4096.0f64;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("hz=") {
            hz = v.parse::<f64>().unwrap_or(100.0).max(1.0);
        } else if let Some(v) = tok.strip_prefix("pagesize=") {
            page = v.parse::<f64>().unwrap_or(4096.0).max(1.0);
        }
    }
    (hz, page)
}

/// Turn `@@PROC@@` rows into per-process CPU by differencing tick counters
/// against the previous sample. Sorted CPU desc then RSS desc, top N only —
/// a process that just appeared has no baseline and reports 0.
fn parse_procs(
    lines: Option<&Vec<String>>,
    prev: &mut PrevSample,
    dt: f64,
    hz: f64,
    page_size: f64,
    top_n: usize,
) -> (Vec<ProcInfo>, usize) {
    let mut current: HashMap<u32, u64> = HashMap::new();
    let mut rows: Vec<ProcInfo> = Vec::new();
    let mut total = 0usize;

    if let Some(ls) = lines {
        for line in ls {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 6 {
                continue;
            }
            let Ok(pid) = f[0].trim().parse::<u32>() else {
                continue;
            };
            total += 1;
            let ticks =
                f[3].trim().parse::<u64>().unwrap_or(0) + f[4].trim().parse::<u64>().unwrap_or(0);
            let rss_pages = f[5].trim().parse::<u64>().unwrap_or(0);
            let cpu_pct = match prev.procs.get(&pid) {
                Some(&before) if dt > 0.0 => {
                    ticks.saturating_sub(before) as f64 / (dt * hz) * 100.0
                }
                _ => 0.0,
            };
            rows.push(ProcInfo {
                pid,
                name: f[1].to_string(),
                state: f[2].to_string(),
                cpu_pct,
                rss_kb: (rss_pages as f64 * page_size / 1024.0) as u64,
            });
            current.insert(pid, ticks);
        }
    }

    prev.procs = current;
    rows.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.rss_kb.cmp(&a.rss_kb))
    });
    rows.truncate(top_n);
    (rows, total)
}

/// A systemd unit that is not running and should be.
#[derive(Debug, Serialize, Clone)]
pub struct FailedUnit {
    pub name: String,
    pub desc: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PortInfo {
    pub proto: String,
    pub port: u16,
    /// Every local address listening on this port — v4 and v6 are the same
    /// service to a human, so they collapse into one row.
    pub addrs: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerInfo {
    pub name: String,
    pub image: String,
    pub status: String,
}

/// Slow-moving service facts, refreshed on their own cadence.
#[derive(Debug, Serialize, Clone, Default)]
pub struct ServiceInfo {
    pub failed: Vec<FailedUnit>,
    /// Sorted by port; capped for display, `port_total` keeps the real count.
    pub ports: Vec<PortInfo>,
    pub port_total: usize,
    pub containers: Vec<ContainerInfo>,
    /// Whether a docker daemon was reachable at all — "no containers" and
    /// "docker is not installed" must not look the same in the UI.
    pub docker_available: bool,
}

/// `nginx.service` yes, `listed.` or a bare `0` no.
fn looks_like_unit(t: &str) -> bool {
    match t.rsplit_once('.') {
        Some((name, ext)) => {
            !name.is_empty() && ext.len() >= 2 && ext.chars().all(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// Parse the SERVICES_SCRIPT output. Missing sections are not an error: a
/// container without systemd simply has no failed units to report.
pub fn parse_services(s: &HashMap<String, Vec<String>>) -> ServiceInfo {
    let mut out = ServiceInfo::default();

    if let Some(units) = s.get("SVC") {
        for line in units {
            let mut it = line.split_whitespace();
            let Some(name) = it.next() else { continue };
            if !looks_like_unit(name) {
                // "0 loaded units listed." and header leftovers are not units.
                continue;
            }
            // Skip UNIT LOAD ACTIVE SUB, keep the human description.
            let desc = it.skip(3).collect::<Vec<_>>().join(" ");
            out.failed.push(FailedUnit {
                name: name.to_string(),
                desc,
            });
        }
    }

    if let Some(ports) = s.get("PORTS") {
        let mut merged: HashMap<(String, u16), Vec<String>> = HashMap::new();
        for line in ports {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 4 {
                continue;
            }
            let Some(port) = f[2].trim().parse::<u16>().ok() else {
                // "*" from a wildcard listener, or junk.
                continue;
            };
            let proto = f[0].trim().to_string();
            let addr = f[1].trim().to_string();
            let entry = merged.entry((proto, port)).or_default();
            if !addr.is_empty() && !entry.contains(&addr) {
                entry.push(addr);
            }
        }
        out.ports = merged
            .into_iter()
            .map(|((proto, port), addrs)| PortInfo { proto, port, addrs })
            .collect();
        out.ports.sort_by_key(|p| (p.port, p.proto.clone()));
        out.port_total = out.ports.len();
    }

    // Only the sentinel section means the daemon answered.
    if let Some(containers) = s.get("DOCKER_YES") {
        out.docker_available = true;
        for line in containers {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 3 || f[0].trim().is_empty() {
                continue;
            }
            out.containers.push(ContainerInfo {
                name: f[0].trim().to_string(),
                image: f[1].trim().to_string(),
                status: f[2].trim().to_string(),
            });
        }
    }

    out
}

fn parse_metrics(raw: &str, prev: &mut PrevSample) -> Option<Metrics> {
    let s = sections(raw);

    let now = mono_secs();
    let dt = if prev.has_baseline {
        (now - prev.mono_ts).max(0.0)
    } else {
        0.0
    };
    // Display timestamp: prefer the VM's own clock, fall back to *wall* time.
    // The monotonic clock is for dt only — on Windows its resolution is
    // ~15.6 ms, so a fallback taken from it can legitimately read 0.0.
    let ts = parse_remote_ts(&s).unwrap_or_else(wall_secs);
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
    // Interfaces the kernel reports as down are pure noise (WSL ships idle
    // eth2, docker leaves br-* around); /proc/net/dev itself has no state.
    let mut down: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(ns) = s.get("NETSTATE") {
        for line in ns {
            if let Some((n, st)) = line.split_once(' ') {
                if st.trim().eq_ignore_ascii_case("down") {
                    down.insert(n.trim().to_string());
                }
            }
        }
    }
    let mut net = Vec::new();
    if let Some(nd) = s.get("NET") {
        for line in nd {
            if !line.contains(':') {
                continue;
            }
            let (name, rest) = line.split_once(':').unwrap();
            let name = name.trim().to_string();
            if name == "lo" || down.contains(&name) {
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
        // Busiest first: on a real host the active NIC must not sit below
        // half a dozen idle bridges/tunnels.
        net.sort_by(|a, b| {
            (b.rx_bps + b.tx_bps)
                .partial_cmp(&(a.rx_bps + a.tx_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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
            prev.diskio
                .insert(name.clone(), (read_sectors, write_sectors));
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

    // --- processes (top N by instantaneous CPU) ---
    let (hz, page_size) = parse_sys(&s);
    let top_n = store::load_settings().process_top_n.clamp(1, 100) as usize;
    let (processes, proc_total) = parse_procs(s.get("PROC"), prev, dt, hz, page_size, top_n);

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
        processes,
        proc_total,
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
    let paused = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(tokio::sync::Notify::new());
    registry().insert(
        sid.clone(),
        TaskHandle {
            visible: visible.clone(),
            stop: stop_flag.clone(),
            paused: paused.clone(),
            wake: wake.clone(),
        },
    );

    tokio::spawn(async move {
        // One-shot static info
        match collect_static(&handle).await {
            Ok(info) => {
                let _ = app.emit(
                    "ssh://static",
                    serde_json::json!({ "sid": sid, "info": info }),
                );
            }
            Err(e) => log::warn!("静态信息采集失败: {:#}", e),
        }

        let mut prev = PrevSample::default();
        // None = never sampled yet, so the first pass fills the card immediately.
        let mut last_slow: Option<Instant> = None;
        loop {
            let settings = store::load_settings();
            let interval = settings.sample_interval_secs.clamp(1, 60);
            // Idle states (paused / monitor off) poll this often: cheap, no SSH
            // traffic, and it keeps "resume" feeling instant.
            const IDLE_POLL_MS: u64 = 400;

            if !settings.monitor_enabled || paused.load(Ordering::SeqCst) {
                // A stale baseline would turn the first sample after resuming
                // into a bogus spike (delta over an arbitrarily long window).
                prev.has_baseline = false;
                prev.net.clear();
                prev.diskio.clear();
                prev.procs.clear();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(IDLE_POLL_MS)) => {}
                    _ = wake.notified() => {}
                }
                if stop_flag.load(Ordering::SeqCst) || closed.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }

            // Recompute the wait each pass so hidden sessions slow down without
            // accumulating drift the way `interval.tick()` + extra sleep does.
            let wait = if visible.load(Ordering::SeqCst) {
                interval
            } else {
                interval * 5
            };
            // `select!` lets "sample now" cut the wait short.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                _ = wake.notified() => {}
            }
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

            // Slow facts get their own channel and cadence: `systemctl` costs
            // more than the entire fast script and nothing here changes
            // second to second. A failure is logged, never fatal.
            let slow_due = last_slow.map_or(true, |t| t.elapsed() >= SERVICES_EVERY);
            if slow_due {
                last_slow = Some(Instant::now());
                match exec_capture(&handle, SERVICES_SCRIPT).await {
                    Ok(sraw) => {
                        let svc = parse_services(&sections(&sraw));
                        let _ = app.emit(
                            "ssh://services",
                            serde_json::json!({ "sid": sid, "services": svc }),
                        );
                    }
                    Err(e) => log::warn!("服务信息采集失败: {:#}", e),
                }
            }
            if let Some(m) = parse_metrics(&raw, &mut prev) {
                // Evaluate alerts before moving `m` into the event payload.
                let fired = crate::alerts::evaluate(&sid, &m, &settings);
                let _ = app.emit(
                    "ssh://metrics",
                    serde_json::json!({ "sid": sid, "metrics": m }),
                );
                for a in fired {
                    log::info!("告警[{}]: {}", sid, a.body);
                    let _ = app.emit("ssh://alert", serde_json::json!({ "sid": sid, "alert": a }));
                }
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

/// Pause/resume collecting for one session. Resuming samples immediately.
pub fn set_paused(sid: &SessionId, paused: bool) {
    if let Some(t) = registry().get(sid) {
        t.paused.store(paused, Ordering::SeqCst);
        if !paused {
            t.wake.notify_one();
        }
    }
}

pub fn is_paused(sid: &SessionId) -> bool {
    registry()
        .get(sid)
        .map(|t| t.paused.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Ask the loop to collect right now instead of waiting out the interval.
pub fn sample_now(sid: &SessionId) {
    if let Some(t) = registry().get(sid) {
        t.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_services_reads_units_ports_and_containers() {
        let raw = "@@SVC@@\nnginx.service loaded failed failed A high performance web server\nbackup.mount loaded failed failed /backup\n0 loaded units listed.\n@@PORTS@@\ntcp|0.0.0.0:22|22|LISTEN\ntcp|0.0.0.0:22|22|LISTEN\ntcp|[::]:22|22|LISTEN\nudp|127.0.0.53:53|53|UNCONN\ntcp|0.0.0.0:*|*|LISTEN\n@@DOCKER@@\n@@DOCKER_YES@@\nweb|nginx:latest|Up 3 hours\n@@END@@\n";
        let svc = parse_services(&sections(raw));

        // Two real units (service + mount); the "0 loaded units listed."
        // summary line must not be mistaken for one.
        assert_eq!(svc.failed.len(), 2, "失败单元: {:?}", svc.failed);
        assert_eq!(svc.failed[0].name, "nginx.service");
        assert!(
            svc.failed[0].desc.contains("web server"),
            "描述应保留: {}",
            svc.failed[0].desc
        );
        assert_eq!(svc.failed[1].name, "backup.mount", "失败的 mount 同样要报");
        assert!(
            svc.failed.iter().all(|f| f.name.contains('.')),
            "垃圾行混进了失败单元: {:?}",
            svc.failed
        );

        // v4+v6 collapse into one row, the exact-duplicate row is dropped, the
        // wildcard port is dropped, and the list is sorted.
        // 22/tcp (v4+v6 merged) and 53/udp. The exact duplicate row and the
        // wildcard "*" port are both gone.
        assert_eq!(svc.port_total, 2, "端口: {:?}", svc.ports);
        assert_eq!(
            (svc.ports[0].port, svc.ports[0].proto.as_str()),
            (22, "tcp")
        );
        assert_eq!(
            svc.ports[0].addrs,
            vec!["0.0.0.0:22".to_string(), "[::]:22".to_string()],
            "同一端口的 v4/v6 地址应合并到一条"
        );
        assert_eq!(
            (svc.ports[1].port, svc.ports[1].proto.as_str()),
            (53, "udp")
        );
        assert!(svc.ports.iter().all(|p| p.port > 0));

        assert!(svc.docker_available);
        assert_eq!(svc.containers.len(), 1);
        assert_eq!(svc.containers[0].name, "web");
        assert_eq!(svc.containers[0].status, "Up 3 hours");
    }

    #[test]
    fn parse_services_tolerates_missing_sections() {
        // A container with no systemd, no ss, no docker: empty, not an error.
        let svc = parse_services(&sections("@@PORTS@@\n@@DOCKER@@\n@@END@@\n"));
        assert!(svc.failed.is_empty());
        assert!(svc.ports.is_empty());
        assert_eq!(svc.port_total, 0);
        assert!(
            !svc.docker_available,
            "没有 docker 输出时不能声称 docker 可用"
        );
    }

    #[test]
    fn partitions_are_not_whole_disks() {
        for name in ["sda", "vda", "xvdb", "nvme0n1", "mmcblk0", "md0"] {
            assert!(is_whole_disk(name), "{} should be a whole disk", name);
        }
        for name in [
            "sda1",
            "sda14",
            "sda15",
            "vda2",
            "xvdb3",
            "nvme0n1p1",
            "mmcblk0p1",
            "loop0",
            "ram3",
            "dm-0",
            "sr0",
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
        assert!(
            !mounts.contains(&"/mnt/c"),
            "WSL host mount must be dropped"
        );
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
    fn net_drops_down_interfaces_and_sorts_busiest_first() {
        // eth0 idle, eth1 carrying traffic, eth2 down (kernel says so).
        let raw1 = "@@TS@@ 100.0\n@@STAT@@\ncpu  100 0 100 800 0 0 0 0 0 0\n@@NET@@\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n  eth1: 5000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n  eth2: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n@@NETSTATE@@\neth0 up\neth1 up\neth2 down\n@@END@@\n";
        let raw2 = "@@TS@@ 102.0\n@@STAT@@\ncpu  600 0 600 1800 0 0 0 0 0 0\n@@NET@@\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n  eth1: 25000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n  eth2: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n@@NETSTATE@@\neth0 up\neth1 up\neth2 down\n@@END@@\n";
        let mut prev = PrevSample::default();
        let _ = parse_metrics(raw1, &mut prev).unwrap();
        prev.mono_ts = mono_secs() - 2.0;
        let m2 = parse_metrics(raw2, &mut prev).unwrap();
        let names: Vec<&str> = m2.net.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["eth1", "eth0"], "down 网卡应被丢弃，忙的排前");
        assert!(
            (m2.net[0].rx_bps - 10000.0).abs() < 1.0,
            "rx={}",
            m2.net[0].rx_bps
        );
        assert_eq!(m2.net[1].rx_bps, 0.0);
    }

    fn dummy_handle() -> TaskHandle {
        TaskHandle {
            visible: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[test]
    fn pause_state_roundtrips_through_the_registry() {
        let sid = "unit-test-pause".to_string();
        registry().insert(sid.clone(), dummy_handle());
        assert!(!is_paused(&sid));
        set_paused(&sid, true);
        assert!(is_paused(&sid), "暂停状态应写入注册表");
        set_paused(&sid, false);
        assert!(!is_paused(&sid));
        registry().remove(&sid);
        // An unknown session must read as "not paused", not panic.
        assert!(!is_paused(&sid));
        set_paused(&sid, true);
        sample_now(&sid);
    }

    /// "Sample now" must cut a long sleep short — that is the whole mechanism.
    #[tokio::test]
    async fn wake_interrupts_a_long_sleep() {
        let sid = "unit-test-wake".to_string();
        registry().insert(sid.clone(), dummy_handle());
        let wake = registry().get(&sid).unwrap().wake.clone();

        let started = Instant::now();
        let waiter = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => "slept",
                _ = wake.notified() => "woken",
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        sample_now(&sid);

        let out = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("sample_now 未能在 2s 内唤醒循环")
            .unwrap();
        assert_eq!(out, "woken");
        assert!(started.elapsed() < Duration::from_secs(2));
        registry().remove(&sid);
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
        assert!(
            (m2.net[0].rx_bps - 1000.0).abs() < 1.0,
            "rx={}",
            m2.net[0].rx_bps
        );
        assert!((m2.net[0].tx_bps - 2000.0).abs() < 1.0);
        assert_eq!(m2.ts, 102.0, "display ts stays the remote one");
    }

    #[test]
    fn procs_diff_ticks_into_cpu_and_sort() {
        // hz=100 ⇒ 100 ticks over a 1s window is exactly one busy core.
        let raw1 = "@@SYS@@ hz=100 pagesize=4096\n@@PROC@@\n1|systemd|S|10|0|500\n2|bash|S|5|0|100\n@@END@@\n";
        let raw2 = "@@SYS@@ hz=100 pagesize=4096\n@@PROC@@\n1|systemd|S|30|0|500\n2|bash|S|105|0|100\n@@END@@\n";
        let mut prev = PrevSample::default();
        let m1 = parse_metrics(raw1, &mut prev).unwrap();
        assert_eq!(m1.proc_total, 2);
        assert!(
            m1.processes.iter().all(|p| p.cpu_pct == 0.0),
            "首样本无基线应为 0"
        );
        assert_eq!(m1.processes[0].rss_kb, 500 * 4096 / 1024, "RSS 页→KB 换算");

        prev.mono_ts = mono_secs() - 1.0;
        prev.has_baseline = true;
        let m2 = parse_metrics(raw2, &mut prev).unwrap();
        assert!(
            (m2.processes[0].cpu_pct - 100.0).abs() < 1.0,
            "top={:?}",
            m2.processes[0]
        );
        assert_eq!(m2.processes[0].name, "bash", "应按 CPU 降序");
        assert!(
            (m2.processes[1].cpu_pct - 20.0).abs() < 1.0,
            "20 ticks → 20%"
        );
        assert_eq!(m2.processes[1].pid, 1);
    }

    #[test]
    fn proc_top_n_is_respected() {
        let mut raw = String::from("@@SYS@@ hz=100 pagesize=4096\n@@PROC@@\n");
        for pid in 1..=40 {
            raw.push_str(&format!("{pid}|p{pid}|S|0|0|1\n"));
        }
        raw.push_str("@@END@@\n");
        let mut prev = PrevSample::default();
        let m = parse_metrics(&raw, &mut prev).unwrap();
        assert_eq!(m.proc_total, 40);
        assert!(m.processes.len() <= 100, "截断后不应超过上限");
    }

    #[test]
    fn sys_defaults_when_section_missing() {
        let mut s: HashMap<String, Vec<String>> = HashMap::new();
        assert_eq!(parse_sys(&s), (100.0, 4096.0));
        s.insert("SYS".into(), vec!["hz=250 pagesize=16384".into()]);
        assert_eq!(parse_sys(&s), (250.0, 16384.0));
        s.insert("SYS".into(), vec!["hz=abc pagesize=".into()]);
        assert_eq!(parse_sys(&s), (100.0, 4096.0));
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

    /// Temp known_hosts that deletes itself, including when an assert panics —
    /// a leaking file per failed run is how these pile up in %TEMP%.
    struct TempKh(std::path::PathBuf);
    impl TempKh {
        fn new(tag: &str, port: u16) -> Self {
            let p = std::env::temp_dir().join(format!(
                "sshbox-live-{}-{}-{}",
                tag,
                std::process::id(),
                port
            ));
            let _ = std::fs::remove_file(&p);
            TempKh(p)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.clone()
        }
    }
    impl Drop for TempKh {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn live_vm_static_info_and_metrics() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };

        let kh = TempKh::new("known_hosts", port);
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.path())
            .await
            .expect("连接失败（WSL 是否在运行？sshd 是否在监听？）");
        assert!(kh.0.exists(), "accept_new 应当把主机密钥写入 known_hosts");

        // --- static info ---
        let info = collect_static(&h).await.expect("静态信息采集失败");
        eprintln!(
            "静态: hostname={} os={} kernel={} cores={} mem={}kB disks={:?}",
            info.hostname,
            info.os_pretty,
            info.kernel,
            info.cpu_cores,
            info.mem_total_kb,
            info.disks
                .iter()
                .map(|d| d.mount.clone())
                .collect::<Vec<_>>()
        );
        assert!(!info.hostname.is_empty(), "hostname 为空");
        assert!(!info.os_pretty.is_empty(), "发行版信息为空");
        assert!(info.cpu_cores >= 1, "CPU 核数异常");
        assert!(info.mem_total_kb > 0, "内存总量为 0");
        assert!(
            !info.disks.is_empty(),
            "没有解析出任何磁盘（根分区必须存在）"
        );
        assert!(
            info.disks.iter().any(|d| d.mount == "/"),
            "根分区缺失: {:?}",
            info.disks.iter().map(|d| &d.mount).collect::<Vec<_>>()
        );

        // --- two live samples → rates ---
        let raw1 = exec_capture(&h, COLLECT_SCRIPT)
            .await
            .expect("第一次采样失败");
        let mut prev = PrevSample::default();
        let m1 = parse_metrics(&raw1, &mut prev).expect("第一次采样解析失败");
        assert_eq!(m1.cpu_pct, 0.0, "首帧不该有 CPU 速率");

        tokio::time::sleep(Duration::from_millis(1200)).await;
        let raw2 = exec_capture(&h, COLLECT_SCRIPT)
            .await
            .expect("第二次采样失败");
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
        assert!(
            (0.0..=100.0).contains(&m2.mem_pct),
            "内存百分比越界: {}",
            m2.mem_pct
        );
        assert!(
            m2.cpu_pct >= 0.0 && m2.cpu_pct <= 100.0,
            "CPU 百分比越界: {}",
            m2.cpu_pct
        );
        assert!(!m2.cpu_per_core.is_empty(), "没有每核数据");
        assert!(!m2.net.is_empty());
        assert!(!m2.processes.is_empty(), "进程表不应为空");
        assert!(m2.proc_total > 5, "进程总数应 > 5，实际 {}", m2.proc_total);
        assert!(
            m2.processes.iter().all(|p| !p.name.is_empty() && p.pid > 0),
            "每行都应有名字和 pid"
        );
        assert!(
            m2.processes.iter().any(|p| p.rss_kb > 0),
            "至少一个进程应有非零常驻内存"
        );
        eprintln!(
            "进程总数={} Top3={:?}",
            m2.proc_total,
            m2.processes.iter().take(3).collect::<Vec<_>>()
        );
        assert!(!m2.disks.is_empty(), "没有磁盘数据");
        assert!(m2.ts > 0.0, "时间戳无效");
        // Whole disks only — a partition row here means the filter regressed.
        for d in &m2.disk_io {
            assert!(is_whole_disk(&d.name), "磁盘 I/O 里出现了分区: {}", d.name);
        }
    }

    /// The services script must survive on a real box, not just in unit tests:
    /// a distro without `ss`, an empty failed list, no docker — all fine, but a
    /// machine reachable over SSH always has a listening port.
    #[tokio::test]
    async fn live_services_snapshot_is_parseable() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 服务测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        let kh = TempKh::new("kh-svc", port);
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.path())
            .await
            .expect("连接失败（WSL 是否在运行？）");

        let raw = exec_capture(&h, SERVICES_SCRIPT)
            .await
            .expect("服务信息采集失败");
        let svc = parse_services(&sections(&raw));
        eprintln!(
            "服务快照: 失败单元={} 监听端口={} docker={} 容器={}",
            svc.failed.len(),
            svc.port_total,
            svc.docker_available,
            svc.containers.len()
        );
        eprintln!("端口: {:?}", svc.ports.iter().take(6).collect::<Vec<_>>());

        assert!(
            svc.port_total > 0,
            "能 SSH 的机器必然有监听端口（至少 sshd）"
        );
        assert!(
            svc.ports.iter().any(|p| p.port == port),
            "没看到 SSH 自己的端口 {}: {:?}",
            port,
            svc.ports
        );
        assert!(svc.ports.iter().all(|p| p.port > 0));
        assert!(
            svc.failed.iter().all(|f| looks_like_unit(&f.name)),
            "垃圾行混进了失败单元: {:?}",
            svc.failed
        );
    }

    /// Regression guard for the whole process pipeline: a shell loop must show
    /// up as a hot process. This is what catches shell-level field-index bugs
    /// (`$12` vs `${12}`) that synthetic unit tests cannot see.
    #[tokio::test]
    async fn live_busy_process_reports_cpu() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 燃烧测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        let kh = TempKh::new("kh-burn", port);
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.path())
            .await
            .expect("连接失败（WSL 是否在运行？）");

        // Fire-and-forget burner; want_reply=false so this does not block.
        let mut burn = h.channel_open_session().await.expect("打开燃烧通道失败");
        burn.exec(false, "timeout 15 sh -c 'while :; do :; done'")
            .await
            .expect("启动燃烧进程失败");

        let raw1 = exec_capture(&h, COLLECT_SCRIPT).await.expect("采样1失败");
        let mut prev = PrevSample::default();
        let _ = parse_metrics(&raw1, &mut prev);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let raw2 = exec_capture(&h, COLLECT_SCRIPT).await.expect("采样2失败");
        let m2 = parse_metrics(&raw2, &mut prev).expect("解析2失败");

        let top = &m2.processes[0];
        eprintln!("最热进程: {:?}", top);
        assert!(
            top.cpu_pct > 50.0,
            "忙碌进程应 >50% CPU，实际 {:.1}%（说明 tick 字段没取对）",
            top.cpu_pct
        );
        let _ = burn.close().await;
    }

    #[tokio::test]
    async fn live_host_key_verification_flow() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 主机密钥测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        // Never touch the user's real known_hosts.
        let kh = TempKh::new("known_hosts", port);

        // 1) strict + no record ⇒ the connect must be refused (Unknown).
        let strict = connect_for_test(&host, port, &user, &pw, "strict", kh.path()).await;
        assert!(
            strict.is_err(),
            "known_hosts 为空时严格模式必须拒绝连接，实际却成功了"
        );
        assert!(!kh.0.exists(), "严格模式不该写入 known_hosts");

        // 2) accept_new ⇒ records the key and connects.
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.path())
            .await
            .expect("接受新密钥后应当连接成功");
        assert!(kh.0.exists(), "accept_new 应当写入 known_hosts 文件");
        let recorded = std::fs::read_to_string(&kh.0).unwrap();
        assert!(
            recorded.contains(&host),
            "known_hosts 未记录主机: {}",
            recorded
        );
        drop(h);

        // 3) strict again ⇒ now trusted, connects fine.
        connect_for_test(&host, port, &user, &pw, "strict", kh.path())
            .await
            .expect("已记录密钥后严格模式应当通过");

        let entries = crate::hostkey::list(&kh.0);
        assert_eq!(entries.len(), 1, "应恰好记录一条主机密钥: {:?}", entries);
        assert!(
            entries[0].fingerprint.starts_with("SHA256:"),
            "指纹格式异常: {}",
            entries[0].fingerprint
        );

        // 4) remove ⇒ the record is gone and strict refuses again.
        let removed =
            crate::hostkey::remove(&kh.0, &host, port).expect("删除 known_hosts 记录失败");
        assert_eq!(removed, 1);
        assert!(
            connect_for_test(&host, port, &user, &pw, "strict", kh.path())
                .await
                .is_err(),
            "删除记录后严格模式应当重新拒绝"
        );
    }
}
