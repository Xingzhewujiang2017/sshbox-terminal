//! Threshold alerts.
//!
//! Two rules keep this from becoming noise, which is the whole point of an
//! alert: a *sustained* condition fires once, and it must come back down before
//! it can fire again.
//!
//!  * Hysteresis — the value has to fall `HYSTERESIS` points below the
//!    threshold to re-arm, so a metric hovering exactly on the line does not
//!    flap.
//!  * Cooldown — even after re-arming, the same (session, metric) pair stays
//!    quiet for `COOLDOWN_SECS`, so a sawtooth workload cannot spam.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex, MutexGuard};

use serde::Serialize;

use crate::monitor::Metrics;
use crate::ssh::{DiskUsage, SessionId};
use crate::store::Settings;

const HYSTERESIS: f64 = 3.0;
const COOLDOWN_SECS: f64 = 120.0;

#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub value: f64,
    pub threshold: f64,
}

/// One alert event: either a new alert, or the **resolution** of a previous one.
///
/// Resolution has to be an event, not a frontend timeout: the UI keeps alerts
/// per session, so without it a single CPU spike leaves a ⚠ badge lit (and
/// "告警 N" permanently positive) until the session is closed — the alarm stops
/// being a signal. Emitted exactly once per excursion, on the firing → re-armed
/// transition (the same branch that already had to detect it for the cooldown).
#[derive(Debug, Clone, Serialize)]
pub struct AlertEvent {
    pub alert: Alert,
    pub resolved: bool,
}

#[derive(Clone)]
struct Entry {
    /// Currently above the line (and already reported).
    firing: bool,
    /// Last time we actually emitted, on the remote display clock.
    last_ts: f64,
}

impl Default for Entry {
    fn default() -> Self {
        // Not 0.0: with a remote clock that reads near zero (busybox fallback)
        // the very first alert would look like it was still inside its cooldown.
        Entry {
            firing: false,
            last_ts: f64::NEG_INFINITY,
        }
    }
}

static STATE: LazyLock<StdMutex<HashMap<(SessionId, String), Entry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn state() -> MutexGuard<'static, HashMap<(SessionId, String), Entry>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Evaluate one sample. Returns the alert events that should be surfaced *now*
/// (new alerts, and resolutions of earlier ones).
pub fn evaluate(sid: &SessionId, m: &Metrics, s: &Settings) -> Vec<AlertEvent> {
    if !s.alerts_enabled {
        forget(sid);
        return Vec::new();
    }

    let mut out = Vec::new();
    check(
        &mut out,
        sid,
        "cpu",
        m.cpu_pct,
        s.cpu_alert_pct,
        format!(
            "CPU 使用率 {:.0}% 超过阈值 {:.0}%",
            m.cpu_pct, s.cpu_alert_pct
        ),
        m.ts,
    );
    check(
        &mut out,
        sid,
        "mem",
        m.mem_pct,
        s.mem_alert_pct,
        format!(
            "内存使用率 {:.0}% 超过阈值 {:.0}%（已用 {} / {}）",
            m.mem_pct,
            s.mem_alert_pct,
            human_kb(m.mem_used_kb),
            human_kb(m.mem_total_kb)
        ),
        m.ts,
    );

    // The fullest mount is the one worth shouting about — but only among real
    // filesystems: a container overlay sits on the same physical disk as `/`,
    // so letting it win means one full disk can raise two alerts (and the
    // message would name a path the user cannot free up).
    let real: Vec<&DiskUsage> = m.disks.iter().filter(|d| !d.virtual_fs).collect();
    let pool: Vec<&DiskUsage> = if real.is_empty() {
        m.disks.iter().collect()
    } else {
        real
    };
    if let Some(d) = pool.into_iter().max_by(|a, b| {
        a.use_pct
            .partial_cmp(&b.use_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        check(
            &mut out,
            sid,
            "disk",
            d.use_pct,
            s.disk_alert_pct,
            format!(
                "磁盘 {} 已用 {:.0}%（阈值 {:.0}%），剩余 {}",
                d.mount,
                d.use_pct,
                s.disk_alert_pct,
                human_kb(d.total_kb.saturating_sub(d.used_kb))
            ),
            m.ts,
        );
    }

    out
}

fn check(
    out: &mut Vec<AlertEvent>,
    sid: &SessionId,
    kind: &str,
    value: f64,
    threshold: f64,
    body: String,
    ts: f64,
) {
    let mut st = state();
    let e = st.entry((sid.clone(), kind.to_string())).or_default();

    if value < threshold - HYSTERESIS {
        // Comfortably back to normal: re-arm for the next excursion, and say so
        // once — the badge the UI is showing is no longer true.
        if e.firing {
            e.firing = false;
            out.push(AlertEvent {
                alert: Alert {
                    kind: kind.to_string(),
                    title: format!("SSHBox 告警解除 · {}", kind_label(kind)),
                    body: format!(
                        "{} 已回落到阈值以下（当前 {:.0}，阈值 {:.0}）",
                        kind_label(kind),
                        value,
                        threshold
                    ),
                    value,
                    threshold,
                },
                resolved: true,
            });
        }
        return;
    }
    if value < threshold {
        // Inside the hysteresis band — hold the current state, say nothing.
        return;
    }
    if e.firing {
        // Already reported for this excursion; a sustained condition is one alert.
        return;
    }
    // Cooldown applies across re-arms too: a sawtooth workload that dips out of
    // the band and climbs straight back must not alert every few seconds.
    if ts - e.last_ts < COOLDOWN_SECS {
        return;
    }
    e.firing = true;
    e.last_ts = ts;
    out.push(AlertEvent {
        alert: Alert {
            kind: kind.to_string(),
            title: format!("SSHBox 告警 · {}", kind_label(kind)),
            body,
            value,
            threshold,
        },
        resolved: false,
    });
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "cpu" => "CPU",
        "mem" => "内存",
        "disk" => "磁盘",
        _ => kind,
    }
}

fn human_kb(kb: u64) -> String {
    let gb = kb as f64 / 1024.0 / 1024.0;
    if gb >= 1.0 {
        format!("{:.1} GB", gb)
    } else {
        format!("{:.0} MB", kb as f64 / 1024.0)
    }
}

/// Drop all state for a session (closed session, or alerts switched off).
pub fn forget(sid: &SessionId) {
    state().retain(|(s, _), _| s != sid);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            alerts_enabled: true,
            cpu_alert_pct: 90.0,
            mem_alert_pct: 90.0,
            disk_alert_pct: 90.0,
            ..Settings::default()
        }
    }

    fn metrics(ts: f64, cpu: f64, mem: f64, disk: f64) -> Metrics {
        Metrics {
            platform: "Linux".into(),
            ts,
            cpu_pct: cpu,
            cpu_per_core: vec![],
            mem_total_kb: 8 * 1024 * 1024,
            mem_used_kb: 4 * 1024 * 1024,
            mem_pct: mem,
            swap_total_kb: 0,
            swap_used_kb: 0,
            net: vec![],
            disk_io: vec![],
            disks: vec![crate::ssh::DiskUsage {
                mount: "/".into(),
                total_kb: 100_000,
                used_kb: 10_000,
                use_pct: disk,
                virtual_fs: false,
            }],
            load: vec![],
            processes: vec![],
            proc_total: 0,
        }
    }

    #[test]
    fn sustained_condition_fires_once_then_stays_quiet() {
        let sid = "t-fire-once".to_string();
        let s = settings();
        let first = evaluate(&sid, &metrics(100.0, 95.0, 10.0, 10.0), &s);
        assert_eq!(first.len(), 1, "首次越过阈值应告警");
        assert_eq!(first[0].alert.kind, "cpu");
        // Still hot 2s later, and every 2s after that: silence.
        for i in 1..30 {
            let a = evaluate(&sid, &metrics(100.0 + i as f64 * 2.0, 99.0, 10.0, 10.0), &s);
            assert!(a.is_empty(), "持续高温不应重复告警（第 {} 次）", i);
        }
        forget(&sid);
    }

    #[test]
    fn dropping_below_rearms_after_cooldown() {
        let sid = "t-rearm".to_string();
        let s = settings();
        assert_eq!(
            evaluate(&sid, &metrics(100.0, 95.0, 10.0, 10.0), &s).len(),
            1
        );
        // Back to normal — must re-arm, and say the earlier alert is over.
        let back = evaluate(&sid, &metrics(110.0, 10.0, 10.0, 10.0), &s);
        assert_eq!(back.len(), 1, "回落到阈值以下应发一条解除事件");
        assert!(back[0].resolved);
        // Hot again but inside the cooldown window: still quiet.
        assert!(evaluate(&sid, &metrics(150.0, 95.0, 10.0, 10.0), &s).is_empty());
        // Past the cooldown: fires again.
        let later = evaluate(
            &sid,
            &metrics(100.0 + COOLDOWN_SECS + 1.0, 95.0, 10.0, 10.0),
            &s,
        );
        assert_eq!(later.len(), 1, "冷却结束后应重新告警");
        forget(&sid);
    }

    #[test]
    fn recovery_emits_one_resolution_then_a_fresh_alert() {
        let sid = "t-resolve".to_string();
        let s = settings();
        assert_eq!(
            evaluate(&sid, &metrics(100.0, 95.0, 10.0, 10.0), &s).len(),
            1
        );
        // 回落：正好一条解除事件，带得动 UI 需要的字段
        let back = evaluate(&sid, &metrics(110.0, 5.0, 10.0, 10.0), &s);
        assert_eq!(back.len(), 1, "回落只发一条解除");
        assert!(back[0].resolved);
        assert_eq!(back[0].alert.kind, "cpu");
        assert!(
            back[0].alert.body.contains("已回落"),
            "解除文案要说明回落到哪里：{}",
            back[0].alert.body
        );
        // 一直正常：不再重复发解除（UI 不该被无效事件刷屏）
        for i in 1..20 {
            assert!(
                evaluate(&sid, &metrics(110.0 + i as f64 * 2.0, 5.0, 10.0, 10.0), &s).is_empty(),
                "第 {} 次正常采样不该再有事件",
                i
            );
        }
        // 冷却过后再冲高：是一条**新告警**，不是解除
        let again = evaluate(
            &sid,
            &metrics(110.0 + COOLDOWN_SECS + 2.0, 95.0, 10.0, 10.0),
            &s,
        );
        assert_eq!(again.len(), 1);
        assert!(!again[0].resolved);
        forget(&sid);
    }

    #[test]
    fn inside_the_band_nothing_is_resolved() {
        let sid = "t-band-resolve".to_string();
        let s = settings();
        assert_eq!(
            evaluate(&sid, &metrics(100.0, 95.0, 10.0, 10.0), &s).len(),
            1
        );
        // 88% 落在迟滞带内：状态保持 —— 既不解除，也不重复告警
        assert!(evaluate(&sid, &metrics(102.0, 88.0, 10.0, 10.0), &s).is_empty());
        forget(&sid);
    }

    #[test]
    fn hysteresis_band_does_not_flap() {
        let sid = "t-hyst".to_string();
        let s = settings();
        assert_eq!(
            evaluate(&sid, &metrics(100.0, 95.0, 10.0, 10.0), &s).len(),
            1
        );
        // 88% is under the 90% line but inside the 3-point band: state holds,
        // nothing is emitted, and crucially it has not re-armed.
        assert!(evaluate(&sid, &metrics(102.0, 88.0, 10.0, 10.0), &s).is_empty());
        assert!(evaluate(&sid, &metrics(400.0, 95.0, 10.0, 10.0), &s).is_empty());
        forget(&sid);
    }

    #[test]
    fn disabled_alerts_are_silent_and_forget_state() {
        let sid = "t-disabled".to_string();
        let mut s = settings();
        assert_eq!(
            evaluate(&sid, &metrics(100.0, 95.0, 95.0, 95.0), &s).len(),
            3
        );
        s.alerts_enabled = false;
        assert!(evaluate(&sid, &metrics(102.0, 95.0, 95.0, 95.0), &s).is_empty());
        // Re-enabling must alert again immediately, not think it already did.
        s.alerts_enabled = true;
        assert_eq!(
            evaluate(&sid, &metrics(104.0, 95.0, 95.0, 95.0), &s).len(),
            3
        );
        forget(&sid);
    }

    #[test]
    fn disk_alert_names_the_fullest_mount() {
        let sid = "t-disk".to_string();
        let s = settings();
        let mut m = metrics(100.0, 1.0, 1.0, 50.0);
        m.disks.push(crate::ssh::DiskUsage {
            mount: "/data".into(),
            total_kb: 100_000,
            used_kb: 96_000,
            use_pct: 96.0,
            virtual_fs: false,
        });
        let a = evaluate(&sid, &m, &s);
        assert_eq!(a.len(), 1);
        assert!(
            a[0].alert.body.contains("/data"),
            "应报最满的挂载点: {}",
            a[0].alert.body
        );
        forget(&sid);
    }

    #[test]
    fn container_overlay_does_not_win_the_disk_alert() {
        // overlay 挂在根盘上、squashfs 是只读镜像：它报 99% 不代表「盘要满了」，
        // 而且它赢的话，同一块物理盘会同时报根盘和叠加层两条。
        let sid = "t-disk-overlay".to_string();
        let s = settings();
        let mut m = metrics(100.0, 1.0, 1.0, 50.0);
        m.disks.push(DiskUsage {
            mount: "/var/lib/docker/overlay2/abc".into(),
            total_kb: 100_000,
            used_kb: 99_000,
            use_pct: 99.0,
            virtual_fs: true,
        });
        assert!(
            evaluate(&sid, &m, &s).is_empty(),
            "叠加层不该触发磁盘告警"
        );
        forget(&sid);

        // 反过来：真实盘 96% 必须报，而且报的是它（而不是没标记的叠加层）
        let mut m2 = metrics(100.0, 1.0, 1.0, 50.0);
        m2.disks.push(DiskUsage {
            mount: "/var/lib/docker/overlay2/abc".into(),
            total_kb: 100_000,
            used_kb: 99_000,
            use_pct: 99.0,
            virtual_fs: true,
        });
        m2.disks.push(DiskUsage {
            mount: "/data".into(),
            total_kb: 100_000,
            used_kb: 96_000,
            use_pct: 96.0,
            virtual_fs: false,
        });
        let a = evaluate(&sid, &m2, &s);
        assert_eq!(a.len(), 1);
        assert!(
            a[0].alert.body.contains("/data"),
            "该报真实盘: {}",
            a[0].alert.body
        );
        forget(&sid);
    }
}
