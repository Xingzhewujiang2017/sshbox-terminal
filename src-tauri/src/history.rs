//! Persistent metric history.
//!
//! Samples land in a SQLite file next to settings.json. Writes go through a
//! dedicated thread fed by a bounded channel: the sampler runs on a 2s
//! heartbeat and must never wait on disk, so a full queue drops the sample
//! (logged, rate-limited) instead of stalling the monitor loop.
//!
//! Timestamps are *local machine* wall-clock seconds — never the remote clock,
//! because a VM with a skewed clock would draw a scrambled time axis. Rates are
//! summed over interfaces/devices: the history charts show host totals, and
//! per-interface rows would blow the table up for no viewing benefit.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::monitor::Metrics;

/// Rows per transaction. Big enough to amortise the commit, small enough that a
/// crash loses at most a second of samples.
const BATCH_MAX: usize = 64;
const FLUSH_EVERY: Duration = Duration::from_secs(1);
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Samples that may sit unwritten before we start dropping. At the default 2s
/// cadence this is over two hours of buffer — it only ever fills if the disk
/// has stopped responding, in which case dropping beats blocking.
const QUEUE: usize = 4096;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS metrics (
    host_id     TEXT    NOT NULL,
    ts          REAL    NOT NULL,
    cpu_pct     REAL    NOT NULL,
    mem_pct     REAL    NOT NULL,
    mem_used_kb INTEGER NOT NULL,
    net_rx      REAL    NOT NULL,
    net_tx      REAL    NOT NULL,
    disk_r      REAL    NOT NULL,
    disk_w      REAL    NOT NULL,
    load1       REAL    NOT NULL,
    proc_total  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_metrics_host_ts ON metrics(host_id, ts);
-- 时延/丢包（到默认网关）单独一张表：慢采集（15s）才有值，且列可空，
-- 塞进 metrics 的 NOT NULL 骨架只会制造 NULL 窟窿。旧库自动建表，无需迁移。
CREATE TABLE IF NOT EXISTS ping (
    host_id    TEXT NOT NULL,
    ts         REAL NOT NULL,
    latency_ms REAL,
    jitter_ms  REAL,
    loss_pct   REAL
);
CREATE INDEX IF NOT EXISTS idx_ping_host_ts ON ping(host_id, ts);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
"#;

pub(crate) fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// SQLite has no NaN/Inf: binding one would silently become NULL and poison
/// every later AVG. A non-finite rate means "no data for this window" → 0.
fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Sum, skipping non-finite terms. A single bogus interface (a zero-width delta
/// window can yield inf) must not zero out the whole host total — the other
/// interfaces still measured something real.
fn finite_sum(values: impl Iterator<Item = f64>) -> f64 {
    finite(values.filter(|v| v.is_finite()).sum())
}

pub fn db_path() -> PathBuf {
    crate::store::data_dir().join("history.sqlite")
}

// --- host binding ----------------------------------------------------------
//
// A SessionId is a fresh uuid per connection, so it can never key history —
// the same VM would look like a new machine every reconnect. The frontend
// binds sid → host_id right after connecting (quick connections that never had
// a host entry fall back to a clearly-marked temporary key).

static BINDS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn binds() -> &'static Mutex<HashMap<String, String>> {
    BINDS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn bind(sid: &str, host_id: &str) {
    if let Ok(mut m) = binds().lock() {
        m.insert(sid.to_string(), host_id.to_string());
    }
}

pub fn unbind(sid: &str) {
    if let Ok(mut m) = binds().lock() {
        m.remove(sid);
    }
}

/// History key for a session: the host id when known, else a temporary key
/// derived from the sid so the UI can label it as such.
pub fn host_key(sid: &str) -> String {
    if let Ok(m) = binds().lock() {
        if let Some(h) = m.get(sid) {
            return h.clone();
        }
    }
    let short: String = sid.chars().take(8).collect();
    format!("临时-{}", short)
}

// --- one row ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub host_id: String,
    pub ts: f64,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub mem_used_kb: i64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_r: f64,
    pub disk_w: f64,
    pub load1: f64,
    pub proc_total: i64,
}

/// 时延/丢包的历史行。三个值都可空：慢采集 15s 一次，且拿不到网关时整行不落。
#[derive(Debug, Clone)]
pub struct PingRow {
    pub host_id: String,
    pub ts: f64,
    pub latency_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub loss_pct: Option<f64>,
}

impl PingRow {
    pub fn from_ping(host_id: &str, ts: f64, pi: &crate::monitor::PingInfo) -> Self {
        Self {
            host_id: host_id.to_string(),
            ts,
            latency_ms: (pi.rtt_avg.is_finite() && pi.rtt_avg > 0.0).then_some(finite(pi.rtt_avg)),
            jitter_ms: (pi.jitter.is_finite()).then_some(finite(pi.jitter)),
            loss_pct: (pi.loss_pct.is_finite()).then_some(finite(pi.loss_pct)),
        }
    }
}

/// 写线程的消息：指标行和时延行共用一条队列（都是落盘前的小数据结构）。
enum DbMsg {
    Metric(Row),
    Ping(PingRow),
}

impl Row {
    pub fn from_metrics(host_id: &str, m: &Metrics, ts: f64) -> Self {
        Self {
            host_id: host_id.to_string(),
            ts,
            cpu_pct: finite(m.cpu_pct),
            mem_pct: finite(m.mem_pct),
            mem_used_kb: m.mem_used_kb as i64,
            net_rx: finite_sum(m.net.iter().map(|n| n.rx_bps)),
            net_tx: finite_sum(m.net.iter().map(|n| n.tx_bps)),
            disk_r: finite_sum(m.disk_io.iter().map(|d| d.read_bps)),
            disk_w: finite_sum(m.disk_io.iter().map(|d| d.write_bps)),
            load1: finite(m.load.first().copied().unwrap_or(0.0)),
            proc_total: m.proc_total as i64,
        }
    }
}

/// Should this sample be persisted? `last` is the ts of the previous stored
/// sample (0.0 = none yet). The 0.25s tolerance keeps a 2s cadence from
/// skipping every other sample when the timer lands at 1.999s.
pub fn due(now: f64, last: f64, interval_secs: u64) -> bool {
    if last <= 0.0 {
        return true;
    }
    let interval = interval_secs.clamp(1, 3600) as f64;
    now - last >= interval - 0.25
}

// --- writer thread ---------------------------------------------------------

static SENDER: OnceLock<SyncSender<DbMsg>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    if SENDER.get().is_some() {
        return;
    }
    let (tx, rx) = sync_channel::<DbMsg>(QUEUE);
    match std::thread::Builder::new()
        .name("history-writer".into())
        .spawn(move || writer_loop(rx))
    {
        Ok(_) => {
            let _ = SENDER.set(tx);
            log::info!("历史落盘已启动: {}", db_path().display());
        }
        Err(e) => log::error!("历史落盘线程启动失败，历史功能不可用: {:#}", e),
    }
}

/// Queue one sample. Never blocks, never panics — history is a convenience,
/// the live monitor is the product.
pub fn record(row: Row) {
    let Some(tx) = SENDER.get() else { return };
    match tx.try_send(DbMsg::Metric(row)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            let n = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 100 == 0 {
                log::warn!("历史落盘队列已满（磁盘跟不上采样），已丢弃 {} 个样本", n);
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// 时延/丢包落库（慢采集 15s 一次，独立表）。同样 fire-and-forget。
pub fn record_ping(row: PingRow) {
    let Some(tx) = SENDER.get() else { return };
    match tx.try_send(DbMsg::Ping(row)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            let n = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 100 == 0 {
                log::warn!("历史落盘队列已满（磁盘跟不上采样），已丢弃 {} 个样本", n);
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn writer_loop(rx: Receiver<DbMsg>) {
    let path = db_path();
    let mut conn = match open_at(&path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("历史库不可用，落盘线程退出: {:#}", e);
            return;
        }
    };
    let mut batch: Vec<DbMsg> = Vec::with_capacity(BATCH_MAX);
    let mut last_flush = Instant::now();
    // `None` → prune on the first pass, so a long-idle app cleans up at startup
    // instead of waiting six hours.
    let mut last_prune: Option<Instant> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(row) => {
                batch.push(row);
                if batch.len() >= BATCH_MAX {
                    flush(&mut conn, &mut batch, &mut last_flush);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut conn, &mut batch, &mut last_flush);
                break;
            }
        }
        if !batch.is_empty() && last_flush.elapsed() >= FLUSH_EVERY {
            flush(&mut conn, &mut batch, &mut last_flush);
        }
        if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            last_prune = Some(Instant::now());
            prune(&mut conn);
        }
    }
}

fn flush(conn: &mut Connection, batch: &mut Vec<DbMsg>, last_flush: &mut Instant) {
    if batch.is_empty() {
        return;
    }
    let n = batch.len();
    let written = (|| -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO metrics (host_id, ts, cpu_pct, mem_pct, mem_used_kb,
                                      net_rx, net_tx, disk_r, disk_w, load1, proc_total)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for r in batch.iter().filter_map(|m| match m {
                DbMsg::Metric(r) => Some(r),
                DbMsg::Ping(_) => None,
            }) {
                st.execute(params![
                    r.host_id,
                    r.ts,
                    r.cpu_pct,
                    r.mem_pct,
                    r.mem_used_kb,
                    r.net_rx,
                    r.net_tx,
                    r.disk_r,
                    r.disk_w,
                    r.load1,
                    r.proc_total
                ])?;
            }
        }
        {
            let mut st = tx.prepare(
                "INSERT INTO ping (host_id, ts, latency_ms, jitter_ms, loss_pct)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for r in batch.iter().filter_map(|m| match m {
                DbMsg::Ping(r) => Some(r),
                DbMsg::Metric(_) => None,
            }) {
                st.execute(params![r.host_id, r.ts, r.latency_ms, r.jitter_ms, r.loss_pct])?;
            }
        }
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = written {
        log::warn!("历史写入失败，丢弃 {} 条消息: {:#}", n, e);
    }
    batch.clear();
    *last_flush = Instant::now();
}

fn retention_days() -> u64 {
    crate::store::load_settings()
        .history_retention_days
        .clamp(1, 3650)
}

fn prune(conn: &mut Connection) {
    let days = retention_days();
    let cutoff = now_unix() - (days as f64) * 86_400.0;
    match conn.execute("DELETE FROM metrics WHERE ts < ?1", params![cutoff]) {
        Ok(0) => {}
        Ok(n) => log::info!("历史清理: 删除 {} 行（保留 {} 天）", n, days),
        Err(e) => log::warn!("历史清理失败: {:#}", e),
    }
    match conn.execute("DELETE FROM ping WHERE ts < ?1", params![cutoff]) {
        Ok(0) => {}
        Ok(n) => log::info!("时延历史清理: 删除 {} 行（保留 {} 天）", n, days),
        Err(e) => log::warn!("时延历史清理失败: {:#}", e),
    }
    // DELETE 之后 WAL 不回收，-wal 文件会越涨越大（长跑机器的 %APPDATA% 陷阱）。
    // checkpoint(TRUNCATE) 把 WAL 合并回主库并截断；低频 VACUUM 回收主库空洞。
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let meta: Result<String, _> = conn
        .query_row("SELECT v FROM meta WHERE k = 'last_vacuum'", [], |r| r.get(0));
    let due = match meta {
        Ok(s) => s.parse::<i64>().unwrap_or(0) + 7 * 86_400 <= now_unix() as i64,
        Err(_) => true,
    };
    if due {
        match conn.execute_batch("VACUUM;") {
            Ok(()) => {
                let _ = conn.execute(
                    "INSERT INTO meta(k, v) VALUES('last_vacuum', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                    params![now_unix().to_string()],
                );
                log::info!("历史库 VACUUM 完成（每 7 天一次，回收删除空洞）");
            }
            Err(e) => log::warn!("VACUUM 失败: {:#}", e),
        }
    }
}

fn open_at(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建历史库目录失败: {}", dir.display()))?;
    }
    let conn =
        Connection::open(path).with_context(|| format!("打开历史库失败: {}", path.display()))?;
    // WAL so a history query never blocks the writer; `journal_mode` returns a
    // row, so it has to go through query_row rather than pragma_update.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// --- reads -----------------------------------------------------------------

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Bucket {
    pub ts: f64,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_r: f64,
    pub disk_w: f64,
    pub load1: f64,
}

#[derive(Debug, Serialize)]
pub struct HistoryRange {
    pub buckets: Vec<Bucket>,
    /// Raw rows the buckets were averaged from.
    pub raw_points: i64,
    pub bucket_secs: f64,
    /// Coverage of the whole stored history for this host, so the UI can offer
    /// "jump to where the data actually is".
    pub oldest: f64,
    pub newest: f64,
}

/// Average rows into fixed time buckets so the chart gets ~`max_points` points
/// no matter how long the window is: 30 days of 2s samples is 1.3M rows, which
/// no chart and no IPC payload wants to see.
fn range_at(
    conn: &Connection,
    host_id: &str,
    from: f64,
    to: f64,
    max_points: usize,
) -> Result<HistoryRange> {
    let max_points = max_points.clamp(50, 5000);
    let span = (to - from).max(1.0);
    let bucket = (span / max_points as f64).max(1.0);

    let mut st = conn.prepare(
        "SELECT CAST(ts / ?4 AS INTEGER) * ?4 AS b,
                AVG(cpu_pct), AVG(mem_pct), AVG(net_rx), AVG(net_tx),
                AVG(disk_r), AVG(disk_w), AVG(load1)
         FROM metrics
         WHERE host_id = ?1 AND ts >= ?2 AND ts <= ?3
         GROUP BY b
         ORDER BY b",
    )?;
    let buckets: Vec<Bucket> = st
        .query_map(params![host_id, from, to, bucket], |r| {
            Ok(Bucket {
                ts: r.get(0)?,
                cpu_pct: r.get(1)?,
                mem_pct: r.get(2)?,
                net_rx: r.get(3)?,
                net_tx: r.get(4)?,
                disk_r: r.get(5)?,
                disk_w: r.get(6)?,
                load1: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let raw_points: i64 = conn.query_row(
        "SELECT COUNT(*) FROM metrics WHERE host_id = ?1 AND ts >= ?2 AND ts <= ?3",
        params![host_id, from, to],
        |r| r.get(0),
    )?;
    let (oldest, newest): (f64, f64) = conn.query_row(
        "SELECT COALESCE(MIN(ts), 0), COALESCE(MAX(ts), 0) FROM metrics WHERE host_id = ?1",
        params![host_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    Ok(HistoryRange {
        buckets,
        raw_points,
        bucket_secs: bucket,
        oldest,
        newest,
    })
}

#[derive(Debug, Serialize)]
pub struct HostStat {
    pub host_id: String,
    pub rows: i64,
    pub oldest: f64,
    pub newest: f64,
    /// Latest sample, so the list can show where each machine left off.
    pub last_cpu: f64,
    pub last_mem: f64,
}

fn hosts_at(conn: &Connection) -> Result<Vec<HostStat>> {
    let mut st = conn.prepare(
        "SELECT host_id, COUNT(*), MIN(ts), MAX(ts)
         FROM metrics GROUP BY host_id ORDER BY MAX(ts) DESC",
    )?;
    let base: Vec<(String, i64, f64, f64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(base.len());
    for (host_id, rows, oldest, newest) in base {
        let (last_cpu, last_mem): (f64, f64) = conn.query_row(
            "SELECT cpu_pct, mem_pct FROM metrics WHERE host_id = ?1
             ORDER BY ts DESC LIMIT 1",
            params![host_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        out.push(HostStat {
            host_id,
            rows,
            oldest,
            newest,
            last_cpu,
            last_mem,
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct DbStat {
    pub path: String,
    pub bytes: u64,
    pub rows: i64,
    pub hosts: i64,
    pub oldest: f64,
    pub newest: f64,
    pub retention_days: u64,
}

/// Size of the database *including* the WAL. Right after a burst of writes
/// nearly everything still lives in the -wal file, so counting only the main
/// file would under-report the real footprint by orders of magnitude.
fn db_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        total += std::fs::metadata(PathBuf::from(p))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    total
}

fn stats_at(conn: &Connection, path: &Path) -> Result<DbStat> {
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM metrics", [], |r| r.get(0))?;
    let hosts: i64 = conn.query_row("SELECT COUNT(DISTINCT host_id) FROM metrics", [], |r| {
        r.get(0)
    })?;
    let (oldest, newest): (f64, f64) = conn.query_row(
        "SELECT COALESCE(MIN(ts), 0), COALESCE(MAX(ts), 0) FROM metrics",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(DbStat {
        path: path.display().to_string(),
        bytes: db_bytes(path),
        rows,
        hosts,
        oldest,
        newest,
        retention_days: retention_days(),
    })
}

#[derive(Debug, Serialize)]
pub struct ExportResult {
    pub path: String,
    pub rows: i64,
    pub bytes: u64,
}

/// CSV escaping: quote only when needed, and double the quotes inside.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn local_stamp(ts: f64) -> String {
    use chrono::{Local, TimeZone};
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9).clamp(0.0, 999_999_999.0) as u32;
    match Local.timestamp_opt(secs, nanos) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        chrono::LocalResult::None => format!("{:.3}", ts),
    }
}

/// Raw rows (not buckets) go to CSV: the point of an export is the data you
/// cannot see in the chart.
fn export_csv_at(
    conn: &Connection,
    host_id: &str,
    from: f64,
    to: f64,
    dest_dir: Option<&Path>,
) -> Result<ExportResult> {
    let dir = match dest_dir {
        Some(d) => d.to_path_buf(),
        None => db_path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("exports"),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建导出目录失败: {}", dir.display()))?;

    let safe: String = host_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let stem = format!(
        "sshbox-history-{}-{}",
        safe,
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    // Two exports inside the same second must not silently overwrite one
    // another — the user asked for data they can keep.
    let mut path = dir.join(format!("{}.csv", stem));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{}-{}.csv", stem, n));
        n += 1;
    }
    let file = std::fs::File::create(&path)
        .with_context(|| format!("创建 CSV 失败: {}", path.display()))?;
    let mut w = std::io::BufWriter::new(file);

    writeln!(
        w,
        "host_id,time,cpu_pct,mem_pct,mem_used_kb,net_rx_bps,net_tx_bps,disk_read_bps,disk_write_bps,load1,proc_total"
    )?;
    let mut st = conn.prepare(
        "SELECT ts, cpu_pct, mem_pct, mem_used_kb, net_rx, net_tx, disk_r, disk_w, load1, proc_total
         FROM metrics WHERE host_id = ?1 AND ts >= ?2 AND ts <= ?3 ORDER BY ts",
    )?;
    let mut rows = st.query(params![host_id, from, to])?;
    let mut count: i64 = 0;
    while let Some(r) = rows.next()? {
        let ts: f64 = r.get(0)?;
        writeln!(
            w,
            "{},{},{:.2},{:.2},{},{:.1},{:.1},{:.1},{:.1},{:.2},{}",
            csv_field(host_id),
            local_stamp(ts),
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, f64>(5)?,
            r.get::<_, f64>(6)?,
            r.get::<_, f64>(7)?,
            r.get::<_, f64>(8)?,
            r.get::<_, i64>(9)?,
        )?;
        count += 1;
    }
    w.flush()?;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(ExportResult {
        path: path.display().to_string(),
        rows: count,
        bytes,
    })
}

// --- commands --------------------------------------------------------------

fn err(e: impl std::fmt::Display) -> String {
    crate::ssh::err_kind("history_error", format!("{:#}", e))
}

async fn blocking<T, F>(f: F) -> std::result::Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| err(format!("历史任务失败: {:#}", e)))?
        .map_err(err)
}

#[tauri::command]
pub async fn history_range(
    host_id: String,
    from: f64,
    to: f64,
    max_points: Option<usize>,
) -> std::result::Result<HistoryRange, String> {
    blocking(move || {
        let path = db_path();
        if !path.exists() {
            anyhow::bail!("还没有历史数据（库文件尚未创建）");
        }
        let conn = open_at(&path)?;
        range_at(&conn, &host_id, from, to, max_points.unwrap_or(600))
    })
    .await
}

/// 一次查多台主机的历史（组内对比用）。同一时间窗、同一桶数，曲线才可比。
/// 循环复用 `range_at`：库是本地 SQLite，一个组几台到几十台都在毫秒级。
/// 单台失败只跳过那台，不拖垮整组 —— 集群视图里"一台没数据"不该让整屏白掉。
#[tauri::command]
pub async fn history_range_multi(
    host_ids: Vec<String>,
    from: f64,
    to: f64,
    max_points: Option<usize>,
) -> std::result::Result<std::collections::HashMap<String, HistoryRange>, String> {
    let mut out = std::collections::HashMap::new();
    if host_ids.is_empty() {
        return Ok(out);
    }
    let points = max_points.unwrap_or(600);
    blocking(move || {
        let path = db_path();
        if !path.exists() {
            anyhow::bail!("还没有历史数据（库文件尚未创建）");
        }
        let conn = open_at(&path)?;
        for hid in host_ids {
            if let Ok(r) = range_at(&conn, &hid, from, to, points) {
                out.insert(hid, r);
            }
        }
        Ok(out)
    })
    .await
}

// --- 时延/丢包历史（ping 表，慢采集 15s 一档） -------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PingBucket {
    pub ts: f64,
    /// AVG 会跳过 NULL（慢采样之间的快采样行没有 ping 值）。
    pub latency_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub loss_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PingRange {
    pub bucket_secs: f64,
    pub buckets: Vec<PingBucket>,
}

/// 时延分桶。窗口和桶数与 `range_at` 完全一致（同 ts 边界），这样报告的
/// 时延曲线和 CPU/网络曲线能共用同一套时间轴。
fn ping_buckets_at(
    conn: &Connection,
    host_id: &str,
    from: f64,
    to: f64,
    max_points: usize,
) -> Result<PingRange> {
    let max_points = max_points.clamp(50, 5000);
    let span = (to - from).max(1.0);
    let bucket = (span / max_points as f64).max(1.0);
    let mut st = conn.prepare(
        "SELECT CAST(ts / ?4 AS INTEGER) * ?4 AS b,
                AVG(latency_ms), AVG(jitter_ms), AVG(loss_pct)
         FROM ping
         WHERE host_id = ?1 AND ts >= ?2 AND ts <= ?3
         GROUP BY b
         ORDER BY b",
    )?;
    let buckets: Vec<PingBucket> = st
        .query_map(params![host_id, from, to, bucket], |r| {
            Ok(PingBucket {
                ts: r.get(0)?,
                latency_ms: r.get::<_, Option<f64>>(1)?,
                jitter_ms: r.get::<_, Option<f64>>(2)?,
                loss_pct: r.get::<_, Option<f64>>(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    Ok(PingRange { bucket_secs: bucket, buckets })
}

#[tauri::command]
pub async fn ping_range(
    host_id: String,
    from: f64,
    to: f64,
    max_points: Option<usize>,
) -> std::result::Result<PingRange, String> {
    blocking(move || {
        let path = db_path();
        if !path.exists() {
            anyhow::bail!("还没有历史数据（库文件尚未创建）");
        }
        let conn = open_at(&path)?;
        ping_buckets_at(&conn, &host_id, from, to, max_points.unwrap_or(600))
    })
    .await
}

/// 多台主机的时延对比（组内矩阵用）。同一窗口同桶数，单台失败只跳过那台。
#[tauri::command]
pub async fn ping_range_multi(
    host_ids: Vec<String>,
    from: f64,
    to: f64,
    max_points: Option<usize>,
) -> std::result::Result<std::collections::HashMap<String, PingRange>, String> {
    let mut out = std::collections::HashMap::new();
    if host_ids.is_empty() {
        return Ok(out);
    }
    let points = max_points.unwrap_or(600);
    blocking(move || {
        let path = db_path();
        if !path.exists() {
            anyhow::bail!("还没有历史数据（库文件尚未创建）");
        }
        let conn = open_at(&path)?;
        for hid in host_ids {
            if let Ok(r) = ping_buckets_at(&conn, &hid, from, to, points) {
                out.insert(hid, r);
            }
        }
        Ok(out)
    })
    .await
}

#[tauri::command]
pub async fn history_hosts() -> std::result::Result<Vec<HostStat>, String> {
    blocking(move || {
        let path = db_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let conn = open_at(&path)?;
        hosts_at(&conn)
    })
    .await
}

#[tauri::command]
pub async fn history_stats() -> std::result::Result<DbStat, String> {
    blocking(move || {
        let path = db_path();
        let conn = open_at(&path)?;
        stats_at(&conn, &path)
    })
    .await
}

#[tauri::command]
pub async fn history_export(
    host_id: String,
    from: f64,
    to: f64,
    dest_dir: Option<String>,
) -> std::result::Result<ExportResult, String> {
    blocking(move || {
        let conn = open_at(&db_path())?;
        export_csv_at(
            &conn,
            &host_id,
            from,
            to,
            dest_dir.as_deref().map(Path::new),
        )
    })
    .await
}

/// 把一段 CSV 文本（前端已拼好 BOM+CRLF）落盘到导出目录。
/// 总览面板的「导出 CSV / 组历史 CSV」走这里 —— WebView2 里 <a download>
/// 会被静默拦截，按钮点了没反应，所以导出一律走后端写盘（与历史回看同款）。
#[tauri::command]
pub async fn export_csv_text(
    filename: String,
    content: String,
    dest_dir: Option<String>,
) -> std::result::Result<ExportResult, String> {
    blocking(move || {
        let base = db_path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("exports");
        let dir = match dest_dir {
            // 相对子目录（如 "lab"）归位到 exports/ 下，不落到进程 cwd。
            Some(d) => {
                let p = PathBuf::from(&d);
                if p.is_absolute() {
                    p
                } else {
                    base.join(p)
                }
            }
            None => base,
        };
        export_csv_text_at(&filename, &content, &dir)
    })
    .await
}

fn export_csv_text_at(filename: &str, content: &str, dir: &Path) -> Result<ExportResult> {
    // 只拦 Windows 文件名非法字符，中文分组名要原样保留
    let safe: String = filename
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' | '\r' | '\n' => '_',
            _ => c,
        })
        .collect();
    let path = dir.join(safe);
    // 嵌套子目录（exports/lab 之类）也要建出来。
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建导出目录失败: {}", parent.display()))?;
    }
    let bytes = content.as_bytes().len() as u64;
    std::fs::write(&path, content.as_bytes())
        .with_context(|| format!("写入失败: {}", path.display()))?;
    // content.lines() 认识 \r\n，不会把 \r 算进行内容
    let rows = content.lines().count().saturating_sub(1) as i64;
    Ok(ExportResult {
        path: path.display().to_string(),
        rows,
        bytes,
    })
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{DiskIo, NetIf};
    use chrono::TimeZone;

    fn temp_db(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sshbox-hist-{}-{}-{}.sqlite",
            tag,
            std::process::id(),
            now_unix() as u64
        ));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(p.with_extension("sqlite-shm"));
        p
    }

    #[test]
    fn export_csv_text_writes_bom_crlf_chinese_name() {
        let dir = std::env::temp_dir().join(format!("sshbox-csv-{}", now_unix() as u64));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let content = "\u{feff}主机,分组,CPU%\r\n测试机,测试组,0.5\r\n";
        let res = export_csv_text_at(
            "SSHBox-总览-测试组-20260920.csv",
            content,
            &dir,
        )
        .unwrap();
        let bytes = std::fs::read(&res.path).unwrap();
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]), "要带 BOM，Excel 才不串码");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\r\n"), "要 CRLF 换行");
        assert!(text.contains("测试组"), "中文分组名原样保留");
        assert_eq!(res.rows, 1, "1 条数据行（表头不计）");
        assert_eq!(res.bytes as usize, content.as_bytes().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ping_buckets_avg_skips_nulls_and_all_null_is_none() {
        let dir = std::env::temp_dir().join(format!("sshbox-ping-{}", now_unix() as u64));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&SCHEMA).unwrap();
        // 同桶的 3 行：一行 NULL（快采样无 ping），两行有值 → AVG 应跳过 NULL
        conn.execute(
            "INSERT INTO ping (host_id, ts, latency_ms, jitter_ms, loss_pct) VALUES (?1,?2,?3,?4,?5)",
            params!["h1", 100.0, 10.0, 1.0, 0.0],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ping (host_id, ts, latency_ms, jitter_ms, loss_pct) VALUES (?1,?2,?3,?4,?5)",
            params!["h1", 100.9, None::<f64>, None::<f64>, None::<f64>],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ping (host_id, ts, latency_ms, jitter_ms, loss_pct) VALUES (?1,?2,?3,?4,?5)",
            params!["h1", 101.5, 20.0, 3.0, 5.0],
        )
        .unwrap();
        // 另一台主机整个桶都是 NULL（从未测到网关）→ 桶值全是 None，不是 0
        conn.execute(
            "INSERT INTO ping (host_id, ts, latency_ms, jitter_ms, loss_pct) VALUES (?1,?2,?3,?4,?5)",
            params!["h2", 100.0, None::<f64>, None::<f64>, None::<f64>],
        )
        .unwrap();

        let r = ping_buckets_at(&conn, "h1", 0.0, 200.0, 100).unwrap();
        assert_eq!(r.buckets.len(), 1);
        let b = &r.buckets[0];
        assert!(b.latency_ms.is_some());
        assert_eq!(b.latency_ms.unwrap(), 15.0, "AVG(10, NULL, 20) = 15");
        assert_eq!(b.jitter_ms.unwrap(), 2.0);
        assert_eq!(b.loss_pct.unwrap(), 2.5);

        let r2 = ping_buckets_at(&conn, "h2", 0.0, 200.0, 100).unwrap();
        assert_eq!(r2.buckets[0].latency_ms, None, "全 NULL 桶应为 None，不能假装 0ms");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn metrics(cpu: f64, mem: f64, net: Vec<NetIf>, io: Vec<DiskIo>) -> Metrics {
        Metrics {
            platform: "Linux".into(),
            ts: 1_700_000_000.0,
            cpu_pct: cpu,
            cpu_per_core: vec![cpu],
            mem_total_kb: 8_000_000,
            mem_used_kb: 4_000_000,
            mem_pct: mem,
            swap_total_kb: 0,
            swap_used_kb: 0,
            net,
            disk_io: io,
            disks: Vec::new(),
            load: vec![1.5, 1.0, 0.5],
            processes: Vec::new(),
            proc_total: 200,
        }
    }

    fn row(host: &str, ts: f64, cpu: f64, mem: f64) -> Row {
        Row {
            host_id: host.to_string(),
            ts,
            cpu_pct: cpu,
            mem_pct: mem,
            mem_used_kb: 4_000_000,
            net_rx: 1000.0,
            net_tx: 200.0,
            disk_r: 10.0,
            disk_w: 5.0,
            load1: 1.0,
            proc_total: 200,
        }
    }

    fn insert(conn: &mut Connection, rows: &[Row]) {
        let mut b: Vec<DbMsg> = rows.iter().cloned().map(DbMsg::Metric).collect();
        let mut lf = Instant::now();
        flush(conn, &mut b, &mut lf);
    }

    #[test]
    fn from_metrics_sums_interfaces_and_guards_non_finite() {
        let m = metrics(
            12.5,
            60.0,
            vec![
                NetIf {
                    name: "eth0".into(),
                    rx_bps: 1000.0,
                    tx_bps: 200.0,
                },
                NetIf {
                    name: "eth1".into(),
                    rx_bps: 500.0,
                    tx_bps: 50.0,
                },
                // A zero-width delta window can produce inf; it must not reach
                // SQLite (which would store NULL and poison every later AVG).
                NetIf {
                    name: "veth0".into(),
                    rx_bps: f64::INFINITY,
                    tx_bps: f64::NAN,
                },
            ],
            vec![
                DiskIo {
                    name: "sda".into(),
                    read_bps: 100.0,
                    write_bps: 10.0,
                },
                DiskIo {
                    name: "sdb".into(),
                    read_bps: 5.0,
                    write_bps: 1.0,
                },
            ],
        );
        let r = Row::from_metrics("h1", &m, 1_700_000_000.0);
        assert_eq!(r.cpu_pct, 12.5);
        assert_eq!(r.mem_pct, 60.0);
        assert_eq!(r.net_rx, 1500.0, "网卡应按合计落库");
        assert_eq!(r.net_tx, 250.0);
        assert_eq!(r.disk_r, 105.0);
        assert_eq!(r.disk_w, 11.0);
        assert_eq!(r.load1, 1.5, "取 load1 而不是 load5");
        assert_eq!(r.proc_total, 200);
        assert!(r.net_rx.is_finite() && r.net_tx.is_finite());
    }

    #[test]
    fn due_respects_interval_and_tolerates_jitter() {
        assert!(due(1000.0, 0.0, 2), "第一个样本必须落库");
        assert!(due(1002.0, 1000.0, 2), "正好 2 秒应落库");
        assert!(
            due(1001.8, 1000.0, 2),
            "1.8 秒也要落（否则 2 秒节奏会隔次丢）"
        );
        assert!(!due(1001.0, 1000.0, 2), "1 秒不该落");
        assert!(due(1010.0, 1000.0, 10));
        assert!(!due(1009.0, 1000.0, 10), "10 秒间隔下 9 秒不落");
        // Coarser history on top of a fast sampler.
        assert!(!due(1004.0, 1000.0, 5));
        assert!(due(1005.0, 1000.0, 5));
    }

    #[test]
    fn range_averages_into_buckets_and_counts_raw_rows() {
        let path = temp_db("range");
        let mut conn = open_at(&path).unwrap();
        let base = 1_700_000_000.0;
        // 20 samples, 1s apart, cpu ramping 0..19.
        let rows: Vec<Row> = (0..20)
            .map(|i| row("h1", base + i as f64, i as f64, 50.0))
            .collect();
        insert(&mut conn, &rows);

        let r = range_at(&conn, "h1", base - 1.0, base + 30.0, 50).unwrap();
        assert_eq!(r.raw_points, 20);
        // 31s window / 50 points → bucket clamped to 1s, so every sample is its
        // own bucket.
        assert_eq!(r.bucket_secs, 1.0);
        assert_eq!(r.buckets.len(), 20);
        assert_eq!(r.buckets[0].cpu_pct, 0.0);
        assert_eq!(r.buckets[19].cpu_pct, 19.0);
        assert_eq!(r.buckets[0].ts, base, "桶时间戳应落在整数秒边界");

        // Coarser query over the same rows: 100s window / 50 points → 2s
        // buckets, so neighbouring samples get averaged together.
        let r2 = range_at(&conn, "h1", base, base + 100.0, 50).unwrap();
        assert_eq!(r2.bucket_secs, 2.0);
        assert_eq!(r2.raw_points, 20);
        assert_eq!(r2.buckets.len(), 10);
        assert_eq!(r2.buckets[0].cpu_pct, 0.5, "0 和 1 应平均成 0.5");
        assert_eq!(r2.buckets[0].ts, base);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn range_bucket_secs_scales_with_span() {
        let path = temp_db("span");
        let mut conn = open_at(&path).unwrap();
        let base = 1_700_000_000.0;
        insert(&mut conn, &[row("h1", base, 1.0, 1.0)]);
        // 30 days / 600 points → 72 minute buckets.
        let r = range_at(&conn, "h1", base, base + 30.0 * 86400.0, 600).unwrap();
        assert!(
            (r.bucket_secs - 4320.0).abs() < 1.0,
            "30 天 / 600 点应约 72 分钟一桶，实际 {}",
            r.bucket_secs
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn range_is_scoped_to_one_host() {
        let path = temp_db("scope");
        let mut conn = open_at(&path).unwrap();
        let base = 1_700_000_000.0;
        insert(
            &mut conn,
            &[
                row("h1", base, 10.0, 10.0),
                row("h2", base, 99.0, 99.0),
                row("h1", base + 2.0, 20.0, 20.0),
            ],
        );
        let r = range_at(&conn, "h1", base - 1.0, base + 10.0, 600).unwrap();
        assert_eq!(r.raw_points, 2, "h2 的数据不能混进 h1");
        assert!(r.buckets.iter().all(|b| b.cpu_pct < 50.0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hosts_at_lists_coverage_and_latest_sample() {
        let path = temp_db("hosts");
        let mut conn = open_at(&path).unwrap();
        let base = 1_700_000_000.0;
        insert(
            &mut conn,
            &[
                row("h1", base, 10.0, 20.0),
                row("h1", base + 60.0, 30.0, 40.0),
                row("h2", base + 120.0, 5.0, 6.0),
            ],
        );
        let hs = hosts_at(&conn).unwrap();
        assert_eq!(hs.len(), 2);
        // Newest activity first.
        assert_eq!(hs[0].host_id, "h2");
        assert_eq!(hs[1].host_id, "h1");
        assert_eq!(hs[1].rows, 2);
        assert_eq!(hs[1].oldest, base);
        assert_eq!(hs[1].newest, base + 60.0);
        assert_eq!(hs[1].last_cpu, 30.0, "应取最新一条的 cpu");
        assert_eq!(hs[1].last_mem, 40.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_writes_raw_rows_with_local_time_and_escapes_host() {
        let path = temp_db("csv");
        let mut conn = open_at(&path).unwrap();
        let base = 1_700_000_000.0;
        let mut r1 = row("h,1\"x", base, 10.0, 20.0);
        r1.ts = base + 0.25;
        insert(&mut conn, &[r1, row("h,1\"x", base + 2.0, 11.0, 21.0)]);

        let dir = std::env::temp_dir().join(format!("sshbox-csv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = export_csv_at(&conn, "h,1\"x", base - 1.0, base + 10.0, Some(&dir)).unwrap();
        assert_eq!(out.rows, 2);
        assert!(out.bytes > 0);
        let text = std::fs::read_to_string(&out.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "表头 + 2 行");
        assert!(lines[0].starts_with("host_id,time,cpu_pct,mem_pct"));
        assert!(
            lines[1].starts_with("\"h,1\"\"x\","),
            "含逗号和引号的 host 必须被正确转义: {}",
            lines[1]
        );
        // Local time, so it looks like a clock rather than an epoch. Compare
        // against the same timestamp rendered in the local zone, so the test
        // does not depend on where it runs.
        let expected = chrono::Local
            .timestamp_opt(base as i64, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(
            lines[1].contains(&expected),
            "时间应是本地可读格式 {}: {}",
            expected,
            lines[1]
        );
        assert!(lines[2].contains(",11.00,21.00,"));

        // A second export in the same second must land in its own file rather
        // than overwrite the first one.
        let out2 = export_csv_at(&conn, "h,1\"x", base - 1.0, base + 10.0, Some(&dir)).unwrap();
        assert_ne!(out2.path, out.path, "同一秒内的两次导出不能互相覆盖");
        assert!(Path::new(&out2.path).exists());
        assert!(Path::new(&out.path).exists(), "第一个导出文件必须还在");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_drops_only_rows_older_than_the_cutoff() {
        let path = temp_db("prune");
        let mut conn = open_at(&path).unwrap();
        let now = now_unix();
        insert(
            &mut conn,
            &[
                row("h1", now - 40.0 * 86400.0, 1.0, 1.0), // 40 天前 → 删
                row("h1", now - 5.0 * 86400.0, 2.0, 2.0),  // 5 天前 → 留
                row("h1", now - 10.0, 3.0, 3.0),           // 刚刚 → 留
            ],
        );
        let days = retention_days();
        let cutoff = now - (days as f64) * 86_400.0;
        let deleted = conn
            .execute("DELETE FROM metrics WHERE ts < ?1", params![cutoff])
            .unwrap();
        assert_eq!(deleted, 1, "默认保留 {} 天，只该删 40 天前那条", days);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn host_key_falls_back_to_a_marked_temporary_key() {
        // No binding at all → temporary key, clearly marked so the UI can say
        // "this is not a saved host".
        let k = host_key("12345678-90ab-cdef-1234-567890abcdef");
        assert!(k.starts_with("临时-"), "未绑定主机应有临时标记: {}", k);
        assert_eq!(k, "临时-12345678", "只取前 8 位");

        bind("sid-1", "host-abc");
        assert_eq!(host_key("sid-1"), "host-abc");
        unbind("sid-1");
        assert!(host_key("sid-1").starts_with("临时-"));
    }

    #[test]
    fn record_without_init_is_a_no_op() {
        // Must not panic when history is disabled/unavailable.
        record(row("h1", 1.0, 1.0, 1.0));
    }
}
