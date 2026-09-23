//! Agentless /proc collector.
//!
//! One exec channel per sample runs a single POSIX shell script that prints
//! `@@NAME@@` section markers; this module parses it into `HashMap<section,
//! Vec<line>>` and turns raw counters into rates by differencing against the
//! previous sample. No dependency on `top`/`vmstat` (format differs per distro,
//! busybox images often lack them) and no disturbance to the user's PTY.

use std::collections::{HashMap, HashSet, VecDeque};
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

/// Last static snapshot per session.
///
/// `ssh://static` is a one-shot event that fires milliseconds after the
/// connection, while the UI needs a full IPC round trip just to register a
/// listener — whoever loses that race used to stare at "采集系统信息中…" until
/// the session was reconnected (the password-dialog path loses it every time,
/// because the panel mounts later). The panel now asks for this cache on mount.
static STATIC_CACHE: LazyLock<StdMutex<HashMap<SessionId, StaticInfo>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// 每个会话最近采到的**原始点**（内存环形缓冲，不是历史库）。
///
/// 监控面板按会话重建（`App.vue` 的 `:key="activeTab.sid"`，为的是不让五个单-sid ref
/// 串台），重建后面板自己的 `history` 从空攒：实测切过去 4s 内连图表容器都没渲染
/// （`canvas=0`、数字全空），第一个点要等 5.8–48.2s，而攒满 150 点（默认 10s 一档 =
/// 25 分钟）根本等不到 —— 用户看到的就是"切过去图是空的，过很久才慢慢长出来"。
/// 所以采集循环顺手把这些点留一份在内存里，面板挂载时用 `monitor_recent` 一次补齐。
///
/// 为什么不用历史库补（`history_range`）：它是**按时间桶降采样**过的（历史 2s 一档），
/// 和面板的原始点混接会出现同一条曲线前段桶平均、后段瞬时值，x 轴时间标签重复/回退；
/// 快速连接（临时 sid）在库里根本没有行；`history_enabled=false` 时更是直接空。
/// 内存缓冲天然同源同口径，进程活着才有（不会给出陈旧数据），断档的轮次本就没进缓冲
/// （不会把"采失败"画成 0），也不碰 DB / 锁。
static RECENT: LazyLock<StdMutex<HashMap<SessionId, RecentBuffer>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// 与面板侧的 `MAX_POINTS` / `PING_POINTS` 对齐：缓冲比面板能画的还长没意义，
/// 短了则 hydrate 出来的曲线会被面板自己截掉（看起来像"少了一段"）。
const RECENT_MAX: usize = 150;
const RECENT_PING_MAX: usize = 48;

#[derive(Default)]
struct RecentBuffer {
    metrics: VecDeque<Metrics>,
    ping: VecDeque<RecentPing>,
}

/// 缓冲里的一条 ping。`PingInfo` 自身没有时间戳，而面板画 x 轴要时间标签，
/// 所以这里单独记一个（用**本地**墙钟，和面板实时收到时打标签的口径一致）。
#[derive(Debug, Serialize, Clone)]
pub struct RecentPing {
    pub ts: f64,
    pub rtt_avg: f64,
    pub loss_pct: f64,
}

/// `monitor_recent` 的返回值：面板挂载时补齐曲线用。老的在前，与面板数组同序。
#[derive(Debug, Serialize)]
pub struct Recent {
    pub metrics: Vec<Metrics>,
    pub ping: Vec<RecentPing>,
}

/// 满了丢最老的 —— 环形缓冲的唯一规则，单独抽出来是为了能单测。
fn push_capped<T>(q: &mut VecDeque<T>, v: T, max: usize) {
    q.push_back(v);
    while q.len() > max {
        q.pop_front();
    }
}

fn push_recent(sid: &SessionId, m: &Metrics) {
    let mut g = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    let b = g.entry(sid.clone()).or_default();
    push_capped(&mut b.metrics, m.clone(), RECENT_MAX);
}

fn push_recent_ping(sid: &SessionId, p: &PingInfo) {
    let mut g = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    let b = g.entry(sid.clone()).or_default();
    b.ping.push_back(RecentPing {
        ts: wall_secs(),
        rtt_avg: p.rtt_avg,
        loss_pct: p.loss_pct,
    });
    while b.ping.len() > RECENT_PING_MAX {
        b.ping.pop_front();
    }
}

/// 该会话最近采到的点。空 = 还没采到过，或会话已经断开（`forget` 清过）。
pub fn recent(sid: &SessionId) -> Recent {
    let g = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    match g.get(sid) {
        Some(b) => Recent {
            metrics: b.metrics.iter().cloned().collect(),
            ping: b.ping.iter().cloned().collect(),
        },
        None => Recent {
            metrics: Vec::new(),
            ping: Vec::new(),
        },
    }
}

/// 会话 → SSH 客户端句柄。演练台（lab）的组播执行 / 快 ping 要从 tauri 命令侧
/// 直接用句柄开一次性 channel，而采集任务内部才有句柄 —— 所以 spawn 时登记、stop/forget 时清除。
static HANDLES: LazyLock<StdMutex<HashMap<SessionId, Arc<client::Handle<ClientHandler>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

pub fn register_handle(sid: SessionId, handle: Arc<client::Handle<ClientHandler>>) {
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(sid, handle);
}

pub fn unregister_handle(sid: &SessionId) {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner()).remove(sid);
}

pub fn get_handle(sid: &SessionId) -> Option<Arc<client::Handle<ClientHandler>>> {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner()).get(sid).cloned()
}

/// The static snapshot for a session, if it has been collected yet.
pub fn cached_static(sid: &SessionId) -> Option<StaticInfo> {
    STATIC_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(sid)
        .cloned()
}

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
    /// `uname -s` 结果（Linux/Darwin/…）。空 = 采集脚本没跑出结果（目标可能非类 Unix）。
    pub platform: String,
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

/// 时延 / 丢包。测的是到**默认网关**的质量：不依赖公网，网关不通本身就该报警。
#[derive(Debug, Serialize, Clone, Default)]
pub struct PingInfo {
    /// 被 ping 的目标（网关地址）。空表示这次没测到。
    pub target: String,
    pub sent: u32,
    pub recv: u32,
    pub loss_pct: f64,
    pub rtt_min: f64,
    pub rtt_avg: f64,
    pub rtt_max: f64,
    /// 抖动（mdev）。iputils 才给，busybox 没有就留 0。
    pub jitter: f64,
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

/// How often the slow facts — services, ports, containers, sensors — refresh.
const SLOW_EVERY: Duration = Duration::from_secs(15);

/// The shell script that collects everything in one shot. Pure POSIX + /proc.
/// `date +%s.%N` is GNU-only, so a busybox `date` must not break the sample —
/// the timestamp is display-only and the Rust side falls back to its own clock.
const COLLECT_SCRIPT: &str = r#"
echo "@@TS@@ $(date +%s.%N 2>/dev/null || date +%s)"
echo "@@PLATFORM@@ $(uname -s 2>/dev/null)"
echo "@@STAT@@"; cat /proc/stat 2>/dev/null
echo "@@MEM@@"; cat /proc/meminfo 2>/dev/null
echo "@@NET@@"; cat /proc/net/dev 2>/dev/null
echo "@@NETSTATE@@"
for f in /sys/class/net/*/operstate; do
  [ -r "$f" ] || continue
  n=${f#/sys/class/net/}; n=${n%/operstate}
  IFS= read -r st < "$f" 2>/dev/null
  # type 772 = ARPHRD_LOOPBACK：WSL 的环回叫 loopback0 而不是 lo，光比名字会漏
  IFS= read -r ty < "/sys/class/net/$n/type" 2>/dev/null
  echo "$n $st ${ty:-0}"
done
echo "@@DISKIO@@"; cat /proc/diskstats 2>/dev/null
echo "@@DF@@"; df -P -T -k 2>/dev/null
echo "@@LOAD@@"; cat /proc/loadavg 2>/dev/null
echo "@@SYS@@ hz=$(getconf CLK_TCK 2>/dev/null || echo 100) pagesize=$(getconf PAGESIZE 2>/dev/null || echo 4096)"
# Processes, builtins only (read/echo) so 200+ procs stay cheap.
# /proc/<pid>/stat fields after "pid (comm) ": 1=state 12=utime 13=stime 22=rss
echo "@@PROC@@"
for d in /proc/[0-9]*; do
  IFS= read -r st < "$d/stat" 2>/dev/null || continue
  comm=${st#*(}; comm=${comm%%)*}
  rest=${st##*) }
  [ ext4 -n "$rest" ] || continue

  set -- $rest
  # ${12} braces are mandatory: "$12" is $1 followed by a literal 2 in POSIX sh.
  echo "${d#/proc/}|$comm|$1|${12}|${13}|${22}"
done
echo "@@END@@"
"#;

/// Where the remote's sysfs is mounted. Overridable so a test — or a container
/// with an unusual layout — can point the collector at a fabricated tree. The
/// value is baked into the script text, so it must be set on the *client* side.
fn sysfs_root() -> String {
    std::env::var("SSHBOX_SYSFS").unwrap_or_else(|_| "/sys".to_string())
}

/// The slow-moving facts: failed units, listening ports, containers, hardware
/// sensors. Run on a separate, slower cadence — `systemctl` alone costs more
/// than the whole fast script, and none of this changes second to second.
/// `__SYS__` is replaced with the sysfs root at call time.
const SLOW_SCRIPT: &str = r#"
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
echo "@@PING@@"
# 时延/丢包测的是**到默认网关**的链路质量：不依赖公网、不受墙影响，
# 而且网关不通本身就是最该报警的事。默认网关拿不到就整段跳过。
GW=$(ip route show default 2>/dev/null | awk '{print $3; exit}')
[ -z "$GW" ] && GW=$(route -n 2>/dev/null | awk '$1=="0.0.0.0" {print $2; exit}')
if [ -n "$GW" ] && command -v ping >/dev/null 2>&1; then
  # -c 3 -i 0.3 -W 1 ≈ 1 秒；外面再套 timeout，网关黑洞时也不会拖住 15 秒的慢采集。
  # 目标必须**显式打出来**：下面 tail 只留 3 行统计，会把 ping 自己的
  # "PING 192.168.1.1 (...)" 头部截掉，解析端就拿不到测的是谁。
  echo "TARGET $GW"
  if command -v timeout >/dev/null 2>&1; then
    timeout 5 ping -c 3 -i 0.3 -W 1 "$GW" 2>&1 | tail -3
  else
    ping -c 3 -i 0.3 -W 1 "$GW" 2>&1 | tail -3
  fi
fi
echo "@@HW_TEMP@@"
# hwmon is what lm-sensors reads anyway — going through `sensors` would add a
# package dependency and a human-formatted text parser for no extra data.
# temp*_input is millidegrees Celsius; temp*_label exists on most chips.
for h in __SYS__/class/hwmon/hwmon*; do
  [ -r "$h/name" ] || continue
  chip=$(cat "$h/name" 2>/dev/null)
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16; do
    [ -r "$h/temp${i}_input" ] || continue
    lbl=$(cat "$h/temp${i}_label" 2>/dev/null)
    [ -n "$lbl" ] || lbl="temp$i"
    # ${h##*/} = hwmon 目录名。芯片名会重（两块 NVMe 都叫 nvme），目录名不会，
    # 所以去重要靠它 —— 按读数条数去重会把一颗多核 CPU 拆成一堆假芯片。
    echo "${h##*/}|$chip|$lbl|$(cat "$h/temp${i}_input" 2>/dev/null)"
  done
done | head -40
# Thermal zones are the fallback for boxes with no hwmon (ARM boards, VMs).
for z in __SYS__/class/thermal/thermal_zone*; do
  [ -r "$z/temp" ] || continue
  echo "${z##*/}|thermal|$(cat "$z/type" 2>/dev/null)|$(cat "$z/temp" 2>/dev/null)"
done | head -8
echo "@@HW_FAN@@"
for h in __SYS__/class/hwmon/hwmon*; do
  [ -r "$h/name" ] || continue
  chip=$(cat "$h/name" 2>/dev/null)
  for i in 1 2 3 4 5 6 7 8; do
    [ -r "$h/fan${i}_input" ] || continue
    lbl=$(cat "$h/fan${i}_label" 2>/dev/null)
    [ -n "$lbl" ] || lbl="fan$i"
    echo "${h##*/}|$chip|$lbl|$(cat "$h/fan${i}_input" 2>/dev/null)"
  done
done | head -12
echo "@@HW_GPU@@"
# NVIDIA has no sysfs util counter, so it needs its own tool. `nounits` keeps
# the numbers plain; missing fields come back as [N/A], which the parser reads
# as unknown rather than zero.
if command -v nvidia-smi >/dev/null 2>&1; then
  nvidia-smi --query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits 2>/dev/null | head -8 | sed 's/^/n|/'
fi
# AMD exposes utilisation and VRAM straight in sysfs; Intel does not, so it is
# simply absent rather than guessed at.
for c in __SYS__/class/drm/card[0-9]; do
  [ -r "$c/device/gpu_busy_percent" ] || continue
  d=$(basename "$(readlink -f "$c/device/driver" 2>/dev/null)" 2>/dev/null)
  echo "a|${d:-gpu}|$(cat "$c/device/gpu_busy_percent" 2>/dev/null)|$(cat "$c/device/mem_info_vram_used" 2>/dev/null)|$(cat "$c/device/mem_info_vram_total" 2>/dev/null)"
done | head -4
echo "@@END@@"
"#;

/// The slow script with the sysfs root resolved.
fn slow_script() -> String {
    SLOW_SCRIPT.replace("__SYS__", &sysfs_root())
}

const STATIC_SCRIPT: &str = r#"
echo "@@HOST@@ $(hostname 2>/dev/null)"
echo "@@OS@@"; cat /etc/os-release 2>/dev/null
echo "@@KERNEL@@ $(uname -r 2>/dev/null)"
echo "@@ARCH@@ $(uname -m 2>/dev/null)"
echo "@@LSCPU@@"; lscpu 2>/dev/null
echo "@@CPUINFO@@"; head -40 /proc/cpuinfo 2>/dev/null
echo "@@MEMTOT@@ $(grep MemTotal /proc/meminfo 2>/dev/null)"
echo "@@UPTIME@@ $(cat /proc/uptime 2>/dev/null)"
echo "@@DF@@"; df -P -T -k 2>/dev/null
echo "@@END@@"
"#;

/// Run a one-shot exec command over the SSH connection, return combined stdout.
async fn exec_capture(handle: &client::Handle<ClientHandler>, cmd: &str) -> Result<String> {
    exec_capture_code(handle, cmd).await.map(|(o, _)| o)
}

/// 同 exec_capture，但额外返回退出码（演练台组播执行用；缺 ExitStatus 时给 -1）。
/// 全局采集并发节流：N 标签 = N 路独立采样循环，30 台全开可见会成 exec 风暴。
/// 这里统一限 8 路并发，其余排队（Tokio semaphore，permit 持到命令收发完）。
static EXEC_SEM: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(8));

pub async fn exec_capture_code(
    handle: &client::Handle<ClientHandler>,
    cmd: &str,
) -> Result<(String, i32)> {
    // 全局节流：permit 在函数返回时释放（它在 async 块里 drop）
    let _permit = EXEC_SEM.acquire().await.map_err(|_| anyhow::anyhow!("采集并发闸门关闭"))?;
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;

    let mut out = Vec::new();
    let mut code: i32 = -1;
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { ref data }) => out.extend_from_slice(&data[..]),
            Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                out.extend_from_slice(&data[..])
            }
            Some(russh::ChannelMsg::Eof) => {}
            Some(russh::ChannelMsg::ExitStatus { exit_status }) => code = exit_status as i32,
            Some(russh::ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
    Ok((String::from_utf8_lossy(&out).to_string(), code))
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
fn keep_mount(device: &str, fstype: &str, mount: &str) -> bool {
    if mount == "/" {
        return true;
    }
    // 网络盘（NFS/CIFS/SMB/9p）不显示：statfs 对挂死的网络盘会阻塞整个采样循环
    if matches!(fstype, "nfs" | "nfs4" | "cifs" | "smb" | "smb3" | "9p" | "fuse.sshfs" | "fuse.glusterfs") {
        return false;
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
            if f.len() >= 7 && f[0] != "Filesystem" && keep_mount(f[0], f[1], f[6]) {
                let total = f[2].parse::<u64>().unwrap_or(0);
                let used = f[3].parse::<u64>().unwrap_or(0);
                let pct = if total > 0 {
                    used as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                disks.push(DiskUsage {
                    mount: f[6].to_string(),
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
/// 解析 ping 输出。
///
/// 两种格式都要认：iputils 给 `rtt min/avg/max/mdev = a/b/c/d ms`，
/// busybox（Alpine 之类）给 `round-trip min/avg/max = a/b/c ms` 且没有 mdev。
/// 只认一种的话，一类发行版上时延会永远是空 —— 这种"功能静默失效"最难查。
fn parse_ping(sec: &HashMap<String, Vec<String>>) -> Option<PingInfo> {
    let lines = sec.get("PING")?;
    let mut info = PingInfo::default();
    let mut got_stats = false;

    for l in lines {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("TARGET ") {
            info.target = rest.trim().to_string();
        }
        if t.starts_with("PING ") {
            if let Some(host) = t.split_whitespace().nth(1) {
                info.target = host.to_string();
            }
        }
        if let Some(i) = t.find("% packet loss") {
            // "3 packets transmitted, 3 received, 0% packet loss, time 605ms"
            let head = &t[..i];
            if let Some(pct) = head.split_whitespace().last() {
                info.loss_pct = pct.parse().unwrap_or(0.0);
            }
            if let Some(n) = head.split(" packets transmitted").next() {
                info.sent = n.trim().parse().unwrap_or(0);
            }
            if let Some(seg) = head.split(", ").nth(1) {
                let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
                info.recv = digits.parse().unwrap_or(0);
            }
            got_stats = true;
        }
        // rtt min/avg/max/mdev = 0.045/0.052/0.061/0.007 ms
        // round-trip min/avg/max = 0.045/0.052/0.061 ms
        if t.starts_with("rtt ") || t.starts_with("round-trip ") {
            if let Some((_, rhs)) = t.split_once('=') {
                let nums: Vec<f64> = rhs
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .split('/')
                    .filter_map(|x| x.parse().ok())
                    .collect();
                if nums.len() >= 3 {
                    info.rtt_min = nums[0];
                    info.rtt_avg = nums[1];
                    info.rtt_max = nums[2];
                    info.jitter = nums.get(3).copied().unwrap_or(0.0);
                    got_stats = true;
                }
            }
        }
    }

    if !got_stats || info.target.is_empty() {
        return None;
    }
    Some(info)
}

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
    if top_n > 0 {
        rows.truncate(top_n);
    }
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

// ---------------------------------------------------------------------------
// Hardware sensors (temperatures, fans, GPUs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone)]
pub struct TempReading {
    pub chip: String,
    pub label: String,
    pub celsius: f64,
}

#[derive(Debug, Serialize, Clone)]
pub struct FanReading {
    pub chip: String,
    pub label: String,
    pub rpm: u32,
}

/// A GPU as far as it can be seen from the host: NVIDIA through `nvidia-smi`,
/// AMD through sysfs. Every measurement is optional — a card that does not
/// report power draw must show nothing there, not a fabricated 0 W.
#[derive(Debug, Serialize, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub util_pct: Option<f64>,
    pub mem_used_mb: Option<u64>,
    pub mem_total_mb: Option<u64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct HardwareInfo {
    /// Hottest first — the one number worth seeing is the top of the list.
    pub temps: Vec<TempReading>,
    pub fans: Vec<FanReading>,
    pub gpus: Vec<GpuInfo>,
}

/// Plausible die/board temperature. Drivers report junk for unconnected
/// sensors — a flat 0, or 100000000 after a millidegree mix-up — and a wrong
/// number in a monitoring panel is worse than a missing one.
fn plausible_temp(c: f64) -> bool {
    c.is_finite() && (5.0..=125.0).contains(&c)
}

/// `nvidia-smi` prints `[N/A]` / `[Not Supported]` for a field the card does
/// not expose. That is "unknown", never zero.
fn opt_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('[') || t.eq_ignore_ascii_case("n/a") {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn opt_int(s: &str) -> Option<u64> {
    opt_num(s).map(|v| v.max(0.0) as u64)
}

/// 显示名：芯片名重复时（两块 NVMe 都叫 nvme）加 #1/#2，否则保持原名。
///
/// 去重必须按 **hwmon 目录**，不能按读数条数：一颗 8 核 CPU 的 coretemp 有 9
/// 个温度读数，按条数去重会把它拆成 coretemp#1..#9 —— 面板上就成了 9 颗芯片。
fn unique_names(rows: &[(&str, &str)]) -> HashMap<String, String> {
    // dir, chip
    let mut dirs_per_chip: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (dir, chip) in rows {
        dirs_per_chip.entry(chip).or_default().insert(dir);
    }
    let mut per_dir: HashMap<String, String> = HashMap::new();
    let mut seq: HashMap<&str, u32> = HashMap::new();
    for (dir, chip) in rows {
        if per_dir.contains_key(*dir) {
            continue;
        }
        let shared = dirs_per_chip.get(chip).map_or(1, |s| s.len()) > 1;
        let name = if shared {
            let n = seq.entry(chip).or_insert(0);
            *n += 1;
            format!("{chip}#{n}")
        } else {
            (*chip).to_string()
        };
        per_dir.insert((*dir).to_string(), name);
    }
    per_dir
}

/// Parse the HW_TEMP / HW_FAN / HW_GPU sections of the slow script. Absent
/// sections are normal — plenty of machines (containers, VMs, WSL) expose no
/// sensors at all, and that must stay silent rather than look like an error.
pub fn parse_hardware(s: &HashMap<String, Vec<String>>) -> HardwareInfo {
    let mut out = HardwareInfo::default();

    // (dir, chip, label, value)
    let mut raw: Vec<(String, String, String, f64)> = Vec::new();
    if let Some(lines) = s.get("HW_TEMP") {
        for line in lines {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 4 {
                continue;
            }
            let Some(milli) = f[3].trim().parse::<f64>().ok() else {
                continue;
            };
            // sysfs is millidegrees for hwmon and thermal zones alike.
            let c = milli / 1000.0;
            if !plausible_temp(c) {
                continue;
            }
            raw.push((
                f[0].trim().to_string(),
                f[1].trim().to_string(),
                f[2].trim().to_string(),
                c,
            ));
        }
    }

    let keys: Vec<(&str, &str)> = raw
        .iter()
        .map(|(d, c, _, _)| (d.as_str(), c.as_str()))
        .collect();
    let names = unique_names(&keys);
    for (dir, _, label, celsius) in raw {
        out.temps.push(TempReading {
            chip: names.get(&dir).cloned().unwrap_or_default(),
            label,
            celsius,
        });
    }
    out.temps.sort_by(|a, b| {
        b.celsius
            .partial_cmp(&a.celsius)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut raw_fans: Vec<(String, String, String, u32)> = Vec::new();
    if let Some(lines) = s.get("HW_FAN") {
        for line in lines {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 4 {
                continue;
            }
            let Some(rpm) = f[3].trim().parse::<u32>().ok() else {
                continue;
            };
            if rpm == 0 {
                // A stopped fan is a real state, but reporting "0 RPM" for a
                // header that is simply not wired up would be noise.
                continue;
            }
            raw_fans.push((
                f[0].trim().to_string(),
                f[1].trim().to_string(),
                f[2].trim().to_string(),
                rpm,
            ));
        }
    }
    let fan_keys: Vec<(&str, &str)> = raw_fans
        .iter()
        .map(|(d, c, _, _)| (d.as_str(), c.as_str()))
        .collect();
    let fan_names = unique_names(&fan_keys);
    for (dir, _, label, rpm) in raw_fans {
        out.fans.push(FanReading {
            chip: fan_names.get(&dir).cloned().unwrap_or_default(),
            label,
            rpm,
        });
    }

    if let Some(lines) = s.get("HW_GPU") {
        for line in lines {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 2 {
                continue;
            }
            match f[0].trim() {
                // name, util%, mem.used MB, mem.total MB, temp C, power W
                "n" if f.len() >= 7 => {
                    let name = f[1].trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    out.gpus.push(GpuInfo {
                        name,
                        vendor: "nvidia".to_string(),
                        util_pct: opt_num(f[2]),
                        mem_used_mb: opt_int(f[3]),
                        mem_total_mb: opt_int(f[4]),
                        temp_c: opt_num(f[5]).filter(|c| plausible_temp(*c)),
                        power_w: opt_num(f[6]),
                    });
                }
                // driver, busy%, vram used bytes, vram total bytes
                "a" if f.len() >= 5 => {
                    let driver = f[1].trim();
                    let name = match driver {
                        "amdgpu" => "AMD GPU (amdgpu)".to_string(),
                        "radeon" => "AMD GPU (radeon)".to_string(),
                        "" => "GPU".to_string(),
                        other => format!("GPU ({other})"),
                    };
                    out.gpus.push(GpuInfo {
                        name,
                        vendor: "amd".to_string(),
                        util_pct: opt_num(f[2]),
                        // sysfs reports VRAM in bytes; nvidia-smi already gave MB.
                        mem_used_mb: opt_int(f[3]).map(|b| b / 1_048_576),
                        mem_total_mb: opt_int(f[4]).map(|b| b / 1_048_576),
                        temp_c: None,
                        power_w: None,
                    });
                }
                _ => {}
            }
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
    // Loopback is noise too — and it must be detected by *type*, not by name:
    // WSL's loopback is `loopback0`, so a literal `name == "lo"` check lets it
    // through while it carries the localhost-relayed traffic (i.e. this app's
    // own SSH session), which then dominates the header totals.
    let mut hidden: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(ns) = s.get("NETSTATE") {
        for line in ns {
            let mut it = line.split_whitespace();
            let Some(name) = it.next() else { continue };
            let state = it.next().unwrap_or("");
            let ty = it.next().unwrap_or("0");
            let is_loopback = ty == "772" || name == "lo" || name.starts_with("loopback");
            if is_loopback || state.eq_ignore_ascii_case("down") {
                hidden.insert(name.to_string());
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
            if hidden.contains(&name) {
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
    // 0 = 全部进程（前端「全部」选项）；上限 2000，防止畸形主机把每帧 payload 撑爆
    let top_n = store::load_settings().process_top_n.min(2000) as usize;
    let (processes, proc_total) = parse_procs(s.get("PROC"), prev, dt, hz, page_size, top_n);

    Some(Metrics {
        ts,
        platform: s.get("PLATFORM").and_then(|v| v.first()).cloned().unwrap_or_default(),
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
// 一次性快照（巡检报告用）
// ---------------------------------------------------------------------------

/// 巡检报告要的一整套现场数据。
pub struct Snapshot {
    pub static_info: StaticInfo,
    pub metrics: Metrics,
    pub services: ServiceInfo,
    pub hardware: HardwareInfo,
    /// 进程表（报告里的「进程」小节用）。`parse_procs` 顺带按 CPU 降序排好。
    pub processes: Vec<ProcInfo>,
    /// 时延/丢包（到默认网关）。拿不到就是 None，报告里那一节直接不显示。
    pub ping: Option<PingInfo>,
}

/// 采一次完整快照，给报告用。
///
/// **走的是监控循环同一条采集与解析路径**，所以报告里的数字和面板不会打架。
/// 代价是这里要采两次（CPU%、网速、磁盘 IO 都是两次读数之差，只有一次就没有基线），
/// 中间隔 1.2 秒 —— 报告多花一秒，换的是"数字是真测出来的"。
pub async fn collect_snapshot(handle: &client::Handle<ClientHandler>) -> Result<Snapshot> {
    let static_info = collect_static(handle).await?;

    let raw1 = exec_capture(handle, COLLECT_SCRIPT).await?;
    let mut prev = PrevSample::default();
    let _ = parse_metrics(&raw1, &mut prev);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let raw2 = exec_capture(handle, COLLECT_SCRIPT).await?;
    // 进程表要在 parse_metrics 之前解析：两者共用 prev 里的增量基线，
    // 而进程用的是 prev.procs（pid → jiffies）。放在后面会拿 raw2 和自己比，CPU% 全 0。
    let processes = {
        let s2 = sections(&raw2);
        let (hz2, page2) = parse_sys(&s2);
        let (rows, _total) = parse_procs(s2.get("PROC"), &mut prev, 1.2, hz2, page2, 0);
        rows
    };

    let metrics = parse_metrics(&raw2, &mut prev)
        .ok_or_else(|| anyhow::anyhow!("指标解析失败（采集脚本输出异常）"))?;

    // 服务/硬件失败不致命：报告少两节，总比整份出不来强。
    let (services, hardware, ping) = match tokio::time::timeout(Duration::from_secs(25), exec_capture(handle, &slow_script())).await {
        Ok(Ok(sraw)) => {
            let sec = sections(&sraw);
            (parse_services(&sec), parse_hardware(&sec), parse_ping(&sec))
        }
        Ok(Err(e)) => {
            log::warn!("报告：服务/硬件采集失败: {:#}", e);
            (
                ServiceInfo {
                    failed: Vec::new(),
                    ports: Vec::new(),
                    port_total: 0,
                    containers: Vec::new(),
                    docker_available: false,
                },
                HardwareInfo {
                    temps: Vec::new(),
                    fans: Vec::new(),
                    gpus: Vec::new(),
                },
                None,
            )
        }
        Err(_) => {
            log::warn!("报告：服务/硬件采集超时（25s 上限），跳过该节");
            (
                ServiceInfo {
                    failed: Vec::new(),
                    ports: Vec::new(),
                    port_total: 0,
                    containers: Vec::new(),
                    docker_available: false,
                },
                HardwareInfo {
                    temps: Vec::new(),
                    fans: Vec::new(),
                    gpus: Vec::new(),
                },
                None,
            )
        }
    };

    Ok(Snapshot {
        static_info,
        metrics,
        services,
        hardware,
        processes,
        ping,
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
    register_handle(sid.clone(), handle.clone());

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
                STATIC_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(sid.clone(), info.clone());
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
        // ts of the last sample written to history (0.0 = none yet).
        let mut last_history: f64 = 0.0;
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
            let raw = match tokio::time::timeout(Duration::from_secs(8), exec_capture(&handle, COLLECT_SCRIPT)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    log::warn!("采集失败，监控任务退出: {:#}", e);
                    break;
                }
                Err(_) => {
                    // 单轮超时（目标机卡死/docker 假死）不杀任务：跳过本轮，下轮再来
                    log::warn!("采集超时（8s 上限），跳过本轮采样");
                    continue;
                }
            };

            // Slow facts get their own channel and cadence: `systemctl` costs
            // more than the entire fast script and nothing here changes
            // second to second. A failure is logged, never fatal.
            let slow_due = last_slow.map_or(true, |t| t.elapsed() >= SLOW_EVERY);
            if slow_due {
                last_slow = Some(Instant::now());
                match tokio::time::timeout(Duration::from_secs(20), exec_capture(&handle, &slow_script())).await {
                    Ok(Ok(sraw)) => {
                        // One round trip carries both: the sensor reads are a
                        // few file cats, the GPU query is the only real cost.
                        let sec = sections(&sraw);
                        let svc = parse_services(&sec);
                        let _ = app.emit(
                            "ssh://services",
                            serde_json::json!({ "sid": sid, "services": svc }),
                        );
                        // Temps and GPU load move faster than systemd units,
                        // but this shares the slow cadence deliberately: an
                        // extra SSH exec per second costs more than the
                        // resolution it would buy.
                        let hw = parse_hardware(&sec);
                        let _ = app.emit(
                            "ssh://hardware",
                            serde_json::json!({ "sid": sid, "hardware": hw }),
                        );
                        // 时延/丢包跟慢采集同频：ping 一次约 1 秒 wall time，
                        // 塞进 2 秒的快循环会互相打架，而链路质量本来也不会秒级跳变。
                        let pg = parse_ping(&sec);
                        // 顺手留一份给面板 hydrate（面板切回来时不用从零攒）。
                        if let Some(p) = &pg {
                            push_recent_ping(&sid, p);
                        }
                        let _ = app.emit(
                            "ssh://ping",
                            serde_json::json!({ "sid": sid, "ping": pg, "ts": wall_secs() }),
                        );
                        // 落库（独立 ping 表）：只有慢采集才有值，15s 一档。
                        if settings.history_enabled {
                            if let Some(pi) = &pg {
                                crate::history::record_ping(crate::history::PingRow::from_ping(
                                    &crate::history::host_key(&sid),
                                    crate::history::now_unix(),
                                    pi,
                                ));
                            }
                        }
                    }
                    Ok(Err(e)) => log::warn!("服务信息采集失败: {:#}", e),
                    Err(_) => log::warn!("慢采集超时（20s 上限），跳过本轮"),
                }
            }
            if let Some(m) = parse_metrics(&raw, &mut prev) {
                // Evaluate alerts before moving `m` into the event payload.
                let fired = crate::alerts::evaluate(&sid, &m, &settings);
                // 面板 hydrate 用的环形缓冲：先留一份再发事件（顺序无所谓，
                // 但先留保证"事件到了、缓冲里也一定有同一条"，前端去重才不打架）。
                push_recent(&sid, &m);
                // History is fire-and-forget: `record` hands the row to a
                // dedicated thread and returns immediately, so a slow disk can
                // never delay the next sample.
                if settings.history_enabled
                    && crate::history::due(m.ts, last_history, settings.history_interval_secs)
                {
                    last_history = m.ts;
                    crate::history::record(crate::history::Row::from_metrics(
                        &crate::history::host_key(&sid),
                        &m,
                        m.ts,
                    ));
                }
                let _ = app.emit(
                    "ssh://metrics",
                    serde_json::json!({ "sid": sid, "metrics": m }),
                );
                for ev in fired {
                    // 触发和解除都走同一个事件，前端靠 resolved 区分：
                    // 解除事件让徽标熄灭 / 「告警 N」递减，否则一次冲高会亮到会话结束。
                    log::info!(
                        "告警[{}]: {} {}",
                        sid,
                        if ev.resolved { "解除" } else { "触发" },
                        ev.alert.body
                    );
                    let _ = app.emit(
                        "ssh://alert",
                        serde_json::json!({ "sid": sid, "alert": ev.alert, "resolved": ev.resolved }),
                    );
                }
            }
        }
        // 只有「当前注册的那个任务」才配清注册表。
        // 重启监控（monitor_restart / 设置里的「重启监控任务」）是 stop() + 立刻 spawn()，
        // 而 stop() 只是置个标志位：旧任务要等到下一轮才收尾。如果旧任务跑在新任务
        // 注册之后，无条件 remove 会把新任务刚注册的条目和它刚写好的静态缓存一起抹掉 ——
        // 表现就是重启后主机信息卡永远停在「采集系统信息中…」，暂停/可见性也失灵。
        // 静态缓存同理：同一个会话重启监控不该丢主机信息，它由新任务覆盖写。
        retire(&sid, &stop_flag);
    });
}

/// 任务收尾：只有当注册表里登记的仍是「我自己」时才注销自己。
///
/// 抽出来是为了能单测这个竞态 —— 被顶替的旧任务收尾时绝不能把新任务抹掉。
pub(crate) fn retire(sid: &SessionId, stop_flag: &Arc<AtomicBool>) {
    let mut reg = registry();
    let is_current = reg
        .get(sid)
        .map(|t| Arc::ptr_eq(&t.stop, stop_flag))
        .unwrap_or(false);
    if is_current {
        reg.remove(sid);
    }
}

/// Drop everything belonging to a session that is really gone (disconnect).
/// 和任务收尾不同：这里要连静态缓存一起清，因为 sid 不会再被复用。
pub fn forget(sid: &SessionId) {
    stop(sid);
    unregister_handle(sid);
    registry().remove(sid);
    STATIC_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(sid);
    // 环形缓冲也一起清：sid 不会再被复用，留着就是纯泄漏
    // （面板的 `liveTrend` 就是只写不删的前车之鉴）。
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).remove(sid);
}

pub fn stop(sid: &SessionId) {
    unregister_handle(sid);
    if let Some(t) = registry().remove(sid) {
        t.stop.store(true, Ordering::SeqCst);
    }
}

pub fn set_visible(sid: &SessionId, visible: bool) {
    if let Some(t) = registry().get(sid) {
        t.visible.store(visible, Ordering::SeqCst);
        // 切回标签时叫醒采集循环。后台会话是 ×5 降频的（默认 10s 一档 → 50s），
        // 只改标志位的话用户要盯着空面板等最多一整个后台周期（实测 5.8–48.2s）
        // 才等到第一个点 —— 和 `set_paused(false)` 对称，那里早就有这一句了。
        // 面板 hydrate（`monitor_recent`）负责把历史补上，这句负责"立刻来一条新的"。
        if visible {
            t.wake.notify_one();
        }
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
            "Filesystem Type 1024-blocks      Used Available Capacity Mounted on",
            "none ext4 4029616 4 4029612 1% /mnt/wsl",
            "drivers ext4 209715196 191965376 17749820 92% /usr/lib/wsl/drivers",
            "/dev/sdc ext4 1055762868 2311604 999747792 1% /",
            "tmpfs ext4 4096 0 4096 0% /sys/fs/cgroup",
            "C:\\ ext4 209715196 191965376 17749820 92% /mnt/c",
            "/dev/sdb1 ext4 1048576000 10485760 1038090240 1% /data",
            "overlay ext4 104857600 52428800 52428800 50% /var/lib/docker/overlay2",
            "overlay ext4 52428800 26214400 26214400 50% /var/lib/containers",
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

    #[test]
    fn net_hides_loopback_by_type_not_name() {
        // WSL 的环回不叫 lo 而叫 loopback0，而且状态是 up —— 只比名字会漏掉它，
        // 而它恰好承载着经 localhost 转发的 SSH 流量（也就是本应用自己的连接），
        // 于是它会排在最前、还把顶部合计拉高，看起来像主机的真实网卡在跑。
        let raw1 = "@@TS@@ 100.0\n@@STAT@@\ncpu  100 0 100 800 0 0 0 0 0 0\n@@NET@@\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n  lo: 10 0 0 0 0 0 0 0 20 0 0 0 0 0 0 0\n  loopback0: 700 0 0 0 0 0 0 0 800 0 0 0 0 0 0 0\n@@NETSTATE@@\neth0 up 1\nlo up 772\nloopback0 up 772\n@@END@@\n";
        let raw2 = "@@TS@@ 102.0\n@@STAT@@\ncpu  600 0 600 1800 0 0 0 0 0 0\n@@NET@@\n  eth0: 15000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n  lo: 10 0 0 0 0 0 0 0 20 0 0 0 0 0 0 0\n  loopback0: 90000 0 0 0 0 0 0 0 800 0 0 0 0 0 0 0\n@@NETSTATE@@\neth0 up 1\nlo up 772\nloopback0 up 772\n@@END@@\n";
        let mut prev = PrevSample::default();
        let _ = parse_metrics(raw1, &mut prev).unwrap();
        prev.mono_ts = mono_secs() - 2.0;
        let m2 = parse_metrics(raw2, &mut prev).unwrap();
        let names: Vec<&str> = m2.net.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["eth0"], "环回（含 WSL 的 loopback0）不该出现在网络卡");
        assert!(
            (m2.net[0].rx_bps - 7000.0).abs() < 1.0,
            "rx={}",
            m2.net[0].rx_bps
        );
    }

    #[test]
    fn superseded_task_does_not_evict_the_new_one() {
        // 重启监控 = stop() + 立刻 spawn()，旧任务可能晚于新任务才收尾。
        // 它只能注销自己：无条件 remove 会把新任务的注册项和静态缓存一起抹掉。
        let sid = "sid-restart".to_string();
        let old_stop = Arc::new(AtomicBool::new(true));
        let new_stop = Arc::new(AtomicBool::new(false));
        let new_handle = TaskHandle {
            visible: Arc::new(AtomicBool::new(true)),
            stop: new_stop.clone(),
            paused: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(tokio::sync::Notify::new()),
        };
        registry().insert(sid.clone(), new_handle);

        retire(&sid, &old_stop); // 旧任务收尾
        assert!(
            registry().contains_key(&sid),
            "被顶替的旧任务不该注销新任务"
        );

        retire(&sid, &new_stop); // 当前任务收尾
        assert!(!registry().contains_key(&sid), "当前任务收尾应正常注销");
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

    /// 切标签唤醒：`set_visible(true)` 必须叫醒采集循环，`set_visible(false)` 不该叫。
    ///
    /// 这条单测盯的是本 bug 的修法本身：后台会话 ×5 降频（默认 10s 一档 = 50s），
    /// 只改标志位不唤醒 = 用户切回来盯着空面板等最多 50s（实测 5.8–48.2s）。
    #[tokio::test]
    async fn set_visible_wakes_the_loop_only_when_becoming_visible() {
        let sid = "unit-test-visible-wake".to_string();
        registry().insert(sid.clone(), dummy_handle());
        let wake = registry().get(&sid).unwrap().wake.clone();

        set_visible(&sid, false);
        let quiet = tokio::time::timeout(Duration::from_millis(200), wake.notified()).await;
        assert!(quiet.is_err(), "set_visible(false) 不该唤醒采集循环");

        let waiter = tokio::spawn(async move { wake.notified().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        set_visible(&sid, true);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("set_visible(true) 未能在 2s 内唤醒采集循环")
            .unwrap();
        assert!(
            registry().get(&sid).unwrap().visible.load(Ordering::SeqCst),
            "可见标志应置位"
        );
        registry().remove(&sid);
    }

    /// 面板 hydrate 用的环形缓冲：满了丢最老的、顺序不变。
    #[test]
    fn recent_buffer_keeps_the_newest_points_in_order() {
        let mut q: VecDeque<u32> = VecDeque::new();
        for i in 0..(RECENT_MAX as u32 + 25) {
            push_capped(&mut q, i, RECENT_MAX);
        }
        assert_eq!(q.len(), RECENT_MAX, "缓冲长度应封顶");
        assert_eq!(*q.front().unwrap(), 25, "丢掉的应该是最老的 25 条");
        assert_eq!(*q.back().unwrap(), RECENT_MAX as u32 + 24, "最后一条应是最新的");
    }

    /// 断开要连缓冲一起清：sid 不复用，留着就是只写不删的泄漏。
    #[test]
    fn recent_buffer_is_cleared_on_forget() {
        let sid = "unit-test-recent-forget".to_string();
        let raw = "@@TS@@ 100.0\n@@STAT@@\ncpu  100 0 100 800 0 0 0 0 0 0\n@@NET@@\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n@@END@@\n";
        let mut prev = PrevSample::default();
        let m = parse_metrics(raw, &mut prev).expect("样本应可解析");
        push_recent(&sid, &m);
        push_recent_ping(&sid, &PingInfo::default());
        assert_eq!(recent(&sid).metrics.len(), 1);
        assert_eq!(recent(&sid).ping.len(), 1);

        forget(&sid);
        let after = recent(&sid);
        assert!(
            after.metrics.is_empty() && after.ping.is_empty(),
            "forget 后不该还留着缓冲"
        );
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

    /// 时延解析要认两种格式：iputils 给 mdev，busybox 只给 min/avg/max。
    /// 只认一种的话，一类发行版上时延永远空白 —— 这种"功能静默失效"最难查。
    #[test]
    fn parse_ping_handles_iputils_and_busybox() {
        // 真机（WSL Ubuntu → 网关 192.168.1.1）捕获的原样输出。手写夹具漏掉了
        // `tail -3` 会把 ping 头部截掉这件事，结果发布验证才发现报告里没有链路质量。
        let mut sec = HashMap::new();
        sec.insert(
            "PING".to_string(),
            vec![
                "TARGET 192.168.1.1".to_string(),
                "--- 192.168.1.1 ping statistics ---".to_string(),
                "3 packets transmitted, 3 received, 0% packet loss, time 606ms".to_string(),
                "rtt min/avg/max/mdev = 0.792/17.273/50.234/23.306 ms".to_string(),
            ],
        );
        let p = parse_ping(&sec).expect("真机（tail 后）格式要能解析");
        assert_eq!(p.target, "192.168.1.1");
        assert_eq!((p.sent, p.recv), (3, 3));
        assert_eq!(p.loss_pct, 0.0);
        assert!((p.rtt_avg - 17.273).abs() < 1e-9);
        assert!((p.jitter - 23.306).abs() < 1e-9);

        let mut b = HashMap::new();
        b.insert(
            "PING".to_string(),
            vec![
                "PING 10.0.0.1 (10.0.0.1): 56 data bytes".to_string(),
                "3 packets transmitted, 2 packets received, 33% packet loss".to_string(),
                "round-trip min/avg/max = 1.1/2.2/3.3 ms".to_string(),
            ],
        );
        let p2 = parse_ping(&b).expect("busybox 格式要能解析");
        assert_eq!(p2.loss_pct, 33.0);
        assert_eq!((p2.sent, p2.recv), (3, 2));
        assert!((p2.rtt_max - 3.3).abs() < 1e-9);
        assert_eq!(p2.jitter, 0.0, "busybox 没有 mdev，留 0 而不是瞎猜");

        // 直接喂 ping 的原始输出（未经 tail，头部有 "PING host (ip)"）也要认
        let mut h = HashMap::new();
        h.insert(
            "PING".to_string(),
            vec![
                "PING 172.20.0.1 (172.20.0.1) 56(84) bytes of data.".to_string(),
                "--- 172.20.0.1 ping statistics ---".to_string(),
                "3 packets transmitted, 3 received, 0% packet loss, time 605ms".to_string(),
                "rtt min/avg/max/mdev = 0.045/0.052/0.061/0.007 ms".to_string(),
            ],
        );
        let p3 = parse_ping(&h).expect("iputils 原始输出要能解析");
        assert_eq!(p3.target, "172.20.0.1");
        assert!((p3.rtt_avg - 0.052).abs() < 1e-9);

        // 没网关 / 没装 ping / 目标不可达时整段跳过 → None，报告里那一节不显示
        assert!(parse_ping(&HashMap::new()).is_none());
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
    fn proc_top_n_zero_means_every_process() {
        // 「全部」用 0 表示：不截断，但仍按 CPU 降序，前端分页才有稳定顺序。
        let mut ls: Vec<String> = Vec::new();
        for pid in 1..=40 {
            ls.push(format!("{pid}|p{pid}|S|0|0|1"));
        }
        let mut prev = PrevSample::default();
        let (rows, total) = parse_procs(Some(&ls), &mut prev, 1.0, 100.0, 4096.0, 0);
        assert_eq!(total, 40);
        assert_eq!(rows.len(), 40, "top_n=0 要返回全部进程");
        let (rows3, _) = parse_procs(Some(&ls), &mut prev, 1.0, 100.0, 4096.0, 3);
        assert_eq!(rows3.len(), 3, "有限值仍要截断");
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
    // --- 硬件传感器解析 ---------------------------------------------------

    fn hw(pairs: &[(&str, &[&str])]) -> HardwareInfo {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), lines(v));
        }
        parse_hardware(&m)
    }

    #[test]
    fn parse_hardware_reads_millidegrees_and_puts_the_hottest_first() {
        let h = hw(&[(
            "HW_TEMP",
            &[
                "hwmon0|coretemp|Package id 0|45000",
                "hwmon0|coretemp|Core 0|39000",
                "hwmon1|nvme|Composite|32850",
            ],
        )]);
        assert_eq!(h.temps.len(), 3);
        assert_eq!(h.temps[0].label, "Package id 0");
        assert!((h.temps[0].celsius - 45.0).abs() < 0.001);
        assert!((h.temps[2].celsius - 32.85).abs() < 0.001);
        assert_eq!(h.temps[2].chip, "nvme");
        assert!(h.fans.is_empty() && h.gpus.is_empty());
    }

    #[test]
    fn parse_hardware_drops_implausible_readings() {
        // 0 = 传感器没接, 100000000 = 毫度/度搞混, 1000 = 1°C
        let h = hw(&[(
            "HW_TEMP",
            &[
                "hwmon0|coretemp|Package id 0|0",
                "hwmon0|coretemp|Core 0|100000000",
                "hwmon1|acpitz|temp1|1000",
                "hwmon0|coretemp|Core 1|51000",
            ],
        )]);
        assert_eq!(h.temps.len(), 1, "只剩一条可信读数: {:?}", h.temps);
        assert_eq!(h.temps[0].label, "Core 1");
    }

    #[test]
    fn parse_hardware_suffixes_only_repeated_chip_names() {
        let h = hw(&[(
            "HW_TEMP",
            &[
                "hwmon1|nvme|Composite|40000",
                "hwmon2|nvme|Composite|41000",
                "hwmon0|coretemp|Package id 0|50000",
            ],
        )]);
        let chips: Vec<&str> = h.temps.iter().map(|t| t.chip.as_str()).collect();
        assert!(
            chips.contains(&"nvme#1") && chips.contains(&"nvme#2"),
            "{:?}",
            chips
        );
        assert!(
            chips.contains(&"coretemp"),
            "唯一的名字不该加后缀: {:?}",
            chips
        );
    }

    /// 回归：一颗芯片多个读数不能被当成多颗芯片。coretemp 每个核一个温度，
    /// 早期按"读数条数"去重，面板上就出现了 coretemp#1..#9 这样的假芯片。
    #[test]
    fn parse_hardware_does_not_suffix_a_chip_that_has_many_readings() {
        let h = hw(&[(
            "HW_TEMP",
            &[
                "hwmon0|coretemp|Package id 0|51000",
                "hwmon0|coretemp|Core 0|47000",
                "hwmon0|coretemp|Core 1|45000",
                "hwmon0|coretemp|Core 2|82000",
            ],
        )]);
        assert_eq!(h.temps.len(), 4);
        assert!(
            h.temps.iter().all(|t| t.chip == "coretemp"),
            "同一颗芯片的读数不该各带一个后缀: {:?}",
            h.temps
        );
    }

    /// 反过来：两块 NVMe 同名，必须分开 —— 但同一目录的多个风扇也不能被拆。
    #[test]
    fn parse_hardware_suffixes_by_directory_not_by_reading() {
        let h = hw(&[
            (
                "HW_TEMP",
                &[
                    "hwmon0|coretemp|Core 0|40000",
                    "hwmon1|nvme|Composite|41000",
                    "hwmon2|nvme|Composite|42000",
                ],
            ),
            (
                "HW_FAN",
                &[
                    "hwmon3|nct6775|fan1|1200",
                    "hwmon3|nct6775|fan2|900",
                    "hwmon4|nct6775|fan1|1500",
                ],
            ),
        ]);
        let chips: Vec<&str> = h.temps.iter().map(|t| t.chip.as_str()).collect();
        assert!(chips.contains(&"coretemp"), "单颗芯片保持原名: {:?}", chips);
        assert!(
            chips.contains(&"nvme#1") && chips.contains(&"nvme#2"),
            "{:?}",
            chips
        );
        let fans: Vec<(&str, u32)> = h.fans.iter().map(|f| (f.chip.as_str(), f.rpm)).collect();
        assert_eq!(
            fans,
            vec![("nct6775#1", 1200), ("nct6775#1", 900), ("nct6775#2", 1500)],
            "同一目录的两个风扇要共享一个后缀"
        );
    }

    #[test]
    fn parse_hardware_reads_nvidia_csv_and_treats_n_a_as_unknown() {
        let h = hw(&[(
            "HW_GPU",
            &[
                "n|NVIDIA GeForce RTX 3060|37|1234|12288|52|115.4",
                "n|Tesla T4|[N/A]|0|15360|41|[Not Supported]",
            ],
        )]);
        assert_eq!(h.gpus.len(), 2);
        let g = &h.gpus[0];
        assert_eq!(g.vendor, "nvidia");
        assert_eq!(g.util_pct, Some(37.0));
        assert_eq!(g.mem_used_mb, Some(1234));
        assert_eq!(g.mem_total_mb, Some(12288));
        assert_eq!(g.temp_c, Some(52.0));
        assert_eq!(g.power_w, Some(115.4));
        // [N/A] 是"未知"，不是 0
        assert_eq!(h.gpus[1].util_pct, None);
        assert_eq!(h.gpus[1].power_w, None);
        assert_eq!(h.gpus[1].mem_used_mb, Some(0));
    }

    #[test]
    fn parse_hardware_converts_amd_vram_bytes_to_mb() {
        let h = hw(&[("HW_GPU", &["a|amdgpu|12|1610612736|8589934592"])]);
        let g = &h.gpus[0];
        assert_eq!(g.name, "AMD GPU (amdgpu)");
        assert_eq!(g.vendor, "amd");
        assert_eq!(g.mem_used_mb, Some(1536));
        assert_eq!(g.mem_total_mb, Some(8192));
        assert_eq!(g.temp_c, None, "这条没有温度，必须是 None 而不是 0");
    }

    #[test]
    fn parse_hardware_keeps_fans_and_skips_unwired_zeros() {
        let h = hw(&[(
            "HW_FAN",
            &[
                "hwmon3|nct6775|fan1|1200",
                "hwmon3|nct6775|fan2|0",
                "hwmon3|nct6775|fan3|junk",
            ],
        )]);
        assert_eq!(h.fans.len(), 1);
        assert_eq!(h.fans[0].rpm, 1200);
    }

    #[test]
    fn parse_hardware_is_silent_when_a_machine_has_no_sensors() {
        // WSL / 容器：三个 section 一个都没有 —— 不报错，也不造数据
        let h = hw(&[]);
        assert!(h.temps.is_empty() && h.fans.is_empty() && h.gpus.is_empty());
        // 有 section 但全是垃圾行，同样要安静
        let h2 = hw(&[
            ("HW_TEMP", &["", "garbage", "a|b", "d|c|e"]),
            ("HW_GPU", &["n|"]),
        ]);
        assert!(h2.temps.is_empty() && h2.gpus.is_empty());
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
            forwards: Default::default(),
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

        let raw = exec_capture(&h, &slow_script())
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
    /// 一棵假的 sysfs 树：coretemp（含一个 0°C 的假传感器）+ 两块同名 NVMe +
    /// thermal zone + 一个 AMD GPU。测试机（WSL）本身没有任何传感器，
    /// "有传感器"这条路只能靠它覆盖。
    ///
    /// 路径和内容都跟 `.dev/fake_sys.sh`（UI 测试用）保持一致，但**故意用不同
    /// 的目录**：跑一遍 live 测试不能把 UI 那棵树删掉。改这里记得同步改那边。
    const FAKE_SYS_SETUP: &str = r#"
root=/tmp/sshbox-fake-sys-test
rm -rf "$root"
mkdir -p "$root/class/hwmon/hwmon0" "$root/class/hwmon/hwmon1" "$root/class/hwmon/hwmon2" \
         "$root/class/thermal/thermal_zone0" "$root/class/drm/card0/device" \
         "$root/drivers/amdgpu"
printf 'coretemp\n' > "$root/class/hwmon/hwmon0/name"
printf '51000\n' > "$root/class/hwmon/hwmon0/temp1_input"; printf 'Package id 0\n' > "$root/class/hwmon/hwmon0/temp1_label"
printf '47000\n' > "$root/class/hwmon/hwmon0/temp2_input"; printf 'Core 0\n' > "$root/class/hwmon/hwmon0/temp2_label"
printf '45000\n' > "$root/class/hwmon/hwmon0/temp3_input"; printf 'Core 1\n' > "$root/class/hwmon/hwmon0/temp3_label"
printf '82000\n' > "$root/class/hwmon/hwmon0/temp4_input"; printf 'Core 2\n' > "$root/class/hwmon/hwmon0/temp4_label"
printf '0\n'     > "$root/class/hwmon/hwmon0/temp5_input"; printf 'Core 3\n' > "$root/class/hwmon/hwmon0/temp5_label"
printf '65000\n' > "$root/class/hwmon/hwmon0/temp6_input"; printf 'Core 4\n' > "$root/class/hwmon/hwmon0/temp6_label"
printf '1200\n'  > "$root/class/hwmon/hwmon0/fan1_input";  printf 'CPU Fan\n' > "$root/class/hwmon/hwmon0/fan1_label"
printf 'nvme\n' > "$root/class/hwmon/hwmon1/name"
printf '32850\n' > "$root/class/hwmon/hwmon1/temp1_input"; printf 'Composite\n' > "$root/class/hwmon/hwmon1/temp1_label"
printf 'nvme\n' > "$root/class/hwmon/hwmon2/name"
printf '41000\n' > "$root/class/hwmon/hwmon2/temp1_input"; printf 'Composite\n' > "$root/class/hwmon/hwmon2/temp1_label"
printf 'x86_pkg_temp\n' > "$root/class/thermal/thermal_zone0/type"
printf '48000\n' > "$root/class/thermal/thermal_zone0/temp"
ln -s "$root/drivers/amdgpu" "$root/class/drm/card0/device/driver"
printf '34\n' > "$root/class/drm/card0/device/gpu_busy_percent"
printf '1610612736\n' > "$root/class/drm/card0/device/mem_info_vram_used"
printf '8589934592\n' > "$root/class/drm/card0/device/mem_info_vram_total"
echo ok
"#;

    /// 真机跑一遍采集脚本：sysfs 指向假树，验证脚本→解析→数值全对；再用真
    /// /sys 验一遍"这机器没有传感器"的安静路径。
    #[tokio::test]
    async fn live_hardware_snapshot_reads_a_fake_sysfs_tree() {
        let Some((host, port, user, pw)) = test_target() else {
            eprintln!("跳过 live 硬件测试：未设置 SSHBOX_TEST_HOST / SSHBOX_TEST_PASSWORD");
            return;
        };
        let kh = TempKh::new("kh-hw", port);
        let h = connect_for_test(&host, port, &user, &pw, "accept_new", kh.path())
            .await
            .expect("连接失败（WSL 是否在运行？）");

        exec_capture(&h, FAKE_SYS_SETUP)
            .await
            .expect("造假 sysfs 树失败");

        // SSHBOX_SYSFS 是本进程的环境变量，所以这个测试别和别的动
        // slow_script() 的测试并行跑（live 测试一律 --test-threads=1）。
        std::env::set_var("SSHBOX_SYSFS", "/tmp/sshbox-fake-sys-test");
        let raw = exec_capture(&h, &slow_script()).await.expect("慢脚本失败");
        std::env::remove_var("SSHBOX_SYSFS");
        let hw = parse_hardware(&sections(&raw));
        eprintln!("假树读数: {:?}", hw);

        // 假树：8 条可信读数 + 1 条 0°C 的假传感器（必须被丢掉）
        assert_eq!(
            hw.temps.len(),
            8,
            "0°C 的假传感器必须被丢掉: {:?}",
            hw.temps
        );
        assert!(
            hw.temps.iter().all(|t| (5.0..=125.0).contains(&t.celsius)),
            "不该有越界读数: {:?}",
            hw.temps
        );
        assert!(
            !hw.temps.iter().any(|t| t.label == "Core 3"),
            "0°C 那条（Core 3）必须被丢掉: {:?}",
            hw.temps
        );
        assert!(
            (hw.temps[0].celsius - 82.0).abs() < 0.01,
            "最热的排第一: {:?}",
            hw.temps
        );
        assert!(hw
            .temps
            .iter()
            .any(|t| t.label == "Package id 0" && (t.celsius - 51.0).abs() < 0.01));
        assert!(hw
            .temps
            .iter()
            .any(|t| t.label == "Core 4" && (t.celsius - 65.0).abs() < 0.01));
        // 两块 NVMe 同名 -> 加后缀；thermal zone 的 chip 固定是 "thermal"
        assert_eq!(
            hw.temps.iter().filter(|t| t.label == "Composite").count(),
            2
        );
        assert!(
            hw.temps.iter().any(|t| t.chip.starts_with("nvme#")),
            "重复的芯片名要加后缀: {:?}",
            hw.temps
        );
        // 反过来：coretemp 有 5 条读数却只有一颗芯片，绝不能出现 coretemp#N
        assert!(
            hw.temps.iter().filter(|t| t.chip == "coretemp").count() == 5,
            "同一颗芯片的读数必须共用一个名字: {:?}",
            hw.temps
        );
        assert!(
            !hw.temps.iter().any(|t| t.chip.starts_with("coretemp#")),
            "一颗芯片被按读数条数拆开了: {:?}",
            hw.temps
        );
        assert!(hw
            .temps
            .iter()
            .any(|t| t.chip == "thermal" && t.label == "x86_pkg_temp"));
        assert_eq!(hw.fans.len(), 1);
        assert_eq!(hw.fans[0].rpm, 1200);
        assert_eq!(hw.gpus.len(), 1);
        assert_eq!(hw.gpus[0].name, "AMD GPU (amdgpu)");
        assert_eq!(hw.gpus[0].util_pct, Some(34.0));
        assert_eq!(hw.gpus[0].mem_total_mb, Some(8192));

        // 同一台机器、真 /sys：没有任何传感器 —— 安静，而不是造数据
        let real = exec_capture(&h, &slow_script()).await.expect("慢脚本失败");
        let hwr = parse_hardware(&sections(&real));
        eprintln!("真 /sys 读数: {:?}", hwr);
        assert!(
            hwr.temps.is_empty() && hwr.gpus.is_empty() && hwr.fans.is_empty(),
            "WSL 不该有传感器，却读到: {:?}",
            hwr
        );

        let _ = exec_capture(&h, "rm -rf /tmp/sshbox-fake-sys-test").await;
    }

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
        let burn = h.channel_open_session().await.expect("打开燃烧通道失败");
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
