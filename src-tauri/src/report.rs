//! 一键巡检报告：把一次现场快照 + 最近一段历史渲染成 Markdown / HTML。
//!
//! 判异常（`findings`）与渲染（`render_markdown` / `render_html`）都是**纯函数**，
//! 输入是结构体 —— 所以"报告会不会漏掉某个异常、措辞对不对"可以在没有 SSH、
//! 没有网络的情况下单测。命令层只做四件事：取快照 → 取历史 → 渲染 → 落盘。
//!
//! 报告里的每个结论都带**依据**（阈值、具体数值、挂载点/单元名），
//! 不写"系统状态良好"这种没有信息量的句子。

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::history;
use crate::monitor::{self, HardwareInfo, Metrics, PingInfo, ProcInfo, ServiceInfo};
use crate::ssh::{SessionManager, StaticInfo};

// ---------------------------------------------------------------------------
// 输入
// ---------------------------------------------------------------------------

/// 历史区间的汇总（报告里的"最近 N 小时趋势"）。
#[derive(Debug, Clone, Default)]
pub struct HistorySummary {
    pub hours: u64,
    pub points: usize,
    pub raw_rows: i64,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub mem_avg: f64,
    pub mem_max: f64,
    pub load_max: f64,
    pub net_rx_avg: f64,
    pub net_tx_avg: f64,
    pub net_rx_max: f64,
    pub net_tx_max: f64,
    pub disk_r_max: f64,
    pub disk_w_max: f64,
    pub proc_max: i64,
}

impl HistorySummary {
    pub fn from_buckets(hours: u64, range: &history::HistoryRange) -> Option<HistorySummary> {
        if range.buckets.is_empty() {
            return None;
        }
        let n = range.buckets.len() as f64;
        let sum = |f: fn(&history::Bucket) -> f64| range.buckets.iter().map(f).sum::<f64>();
        let max = |f: fn(&history::Bucket) -> f64| {
            range
                .buckets
                .iter()
                .map(f)
                .fold(f64::MIN, |a, b| a.max(b))
        };
        Some(HistorySummary {
            hours,
            points: range.buckets.len(),
            raw_rows: range.raw_points,
            cpu_avg: sum(|b| b.cpu_pct) / n,
            cpu_max: max(|b| b.cpu_pct),
            mem_avg: sum(|b| b.mem_pct) / n,
            mem_max: max(|b| b.mem_pct),
            load_max: max(|b| b.load1),
            net_rx_avg: sum(|b| b.net_rx) / n,
            net_tx_avg: sum(|b| b.net_tx) / n,
            net_rx_max: max(|b| b.net_rx),
            net_tx_max: max(|b| b.net_tx),
            disk_r_max: max(|b| b.disk_r),
            disk_w_max: max(|b| b.disk_w),
            proc_max: 0,
        })
    }
}

/// HTML 报告里的趋势图数据。
///
/// 三档分辨率各带自己的窗口和桶数，前端按用户选的时间范围切换 —— 这样"改时间范围"
/// 是纯前端重绘，不用重新生成报告，也不用在 HTML 里塞几万个点。
/// `points` 用数组而不是对象：同样 500 个点，JSON 体积能小一半。
#[derive(Debug, Clone, Serialize)]
pub struct ChartSeries {
    pub label: String,
    pub hours: u64,
    pub bucket_secs: f64,
    /// 每个点：[ts, cpu%, mem%, net_rx, net_tx, disk_r, disk_w, load1]
    pub points: Vec<[f64; 8]>,
}

/// 时延/丢包趋势序列，独立于指标序列：慢采集 15s 一档，只有连通时才有值。
/// 每个点：[ts, latency_ms, jitter_ms, loss_pct]；时延缺失（没测到网关）的
/// 桶整点丢弃 —— 画 0 ms 是骗人。
#[derive(Debug, Clone, Serialize)]
pub struct PingChartSeries {
    pub label: String,
    pub hours: u64,
    pub bucket_secs: f64,
    pub points: Vec<[f64; 4]>,
}

pub struct ReportData {
    /// 「名称 · user@host:port」
    pub host_label: String,
    pub generated_at: String,
    pub static_info: StaticInfo,
    pub metrics: Metrics,
    pub services: ServiceInfo,
    pub hardware: HardwareInfo,
    pub history: Option<HistorySummary>,
    /// 趋势图数据（1 小时 / 24 小时 / 7 天三档），HTML 报告内嵌后由前端重绘
    pub series: Vec<ChartSeries>,
    pub ping_series: Vec<PingChartSeries>,
    /// 进程表（采集时已按 CPU 降序）。报告里只列 Top N。
    pub processes: Vec<ProcInfo>,
    /// 到默认网关的时延/丢包。拿不到就是 None —— 那一节整块不显示，不编数字。
    pub ping: Option<PingInfo>,
}

// ---------------------------------------------------------------------------
// 异常判定
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Critical,
    Warn,
    Info,
    Ok,
}

impl Level {
    pub fn markdown_badge(self) -> &'static str {
        match self {
            Level::Critical => "🔴 严重",
            Level::Warn => "🟡 注意",
            Level::Info => "🔵 提示",
            Level::Ok => "✅ 正常",
        }
    }
    fn html_class(self) -> &'static str {
        match self {
            Level::Critical => "crit",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Ok => "ok",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

/// 阈值集中在这里：报告说"高"必须能指出高在哪条线上。
pub const DISK_WARN: f64 = 85.0;
pub const DISK_CRIT: f64 = 90.0;
pub const MEM_WARN: f64 = 80.0;
pub const MEM_CRIT: f64 = 90.0;
pub const CPU_WARN: f64 = 90.0;
pub const TEMP_WARN: f64 = 80.0;

fn gb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GB", kb as f64 / 1024.0 / 1024.0)
    } else if kb >= 1024 {
        format!("{:.0} MB", kb as f64 / 1024.0)
    } else {
        format!("{kb} KB")
    }
}

/// 逐条判异常。**顺序有意义**：严重的排前面，用户只看前三条也能抓到重点。
pub fn findings(d: &ReportData) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let m = &d.metrics;

    // systemd 失败单元 —— 这是最明确的故障信号
    if !d.services.failed.is_empty() {
        let names: Vec<String> = d.services
            .failed
            .iter()
            .map(|u| {
                if u.desc.is_empty() {
                    u.name.clone()
                } else {
                    format!("{}（{}）", u.name, u.desc)
                }
            })
            .collect();
        out.push(Finding {
            level: Level::Critical,
            title: format!("{} 个 systemd 单元处于失败状态", names.len()),
            detail: names.join("；"),
        });
    }

    // 磁盘：逐个挂载点判，指出是哪个分区
    let mut crit_disks: Vec<String> = Vec::new();
    let mut warn_disks: Vec<String> = Vec::new();
    for disk in &m.disks {
        let free = disk.total_kb.saturating_sub(disk.used_kb);
        let line = format!(
            "{} 已用 {:.0}%（{} / {}，剩 {}）",
            disk.mount,
            disk.use_pct,
            gb(disk.used_kb),
            gb(disk.total_kb),
            gb(free)
        );
        if disk.use_pct >= DISK_CRIT {
            crit_disks.push(line);
        } else if disk.use_pct >= DISK_WARN {
            warn_disks.push(line);
        }
    }
    if !crit_disks.is_empty() {
        out.push(Finding {
            level: Level::Critical,
            title: format!("磁盘使用率超过 {DISK_CRIT:.0}%"),
            detail: crit_disks.join("；"),
        });
    }
    if !warn_disks.is_empty() {
        out.push(Finding {
            level: Level::Warn,
            title: format!("磁盘使用率超过 {DISK_WARN:.0}%"),
            detail: warn_disks.join("；"),
        });
    }

    // 内存
    if m.mem_pct >= MEM_CRIT {
        out.push(Finding {
            level: Level::Critical,
            title: format!("内存使用率 {:.0}%（阈值 {MEM_CRIT:.0}%）", m.mem_pct),
            detail: format!(
                "{} / {}，Swap {} / {}",
                gb(m.mem_used_kb),
                gb(m.mem_total_kb),
                gb(m.swap_used_kb),
                gb(m.swap_total_kb)
            ),
        });
    } else if m.mem_pct >= MEM_WARN {
        out.push(Finding {
            level: Level::Warn,
            title: format!("内存使用率 {:.0}%（阈值 {MEM_WARN:.0}%）", m.mem_pct),
            detail: format!("{} / {}", gb(m.mem_used_kb), gb(m.mem_total_kb)),
        });
    }

    // Swap 吃紧：真正的内存压力信号（内存还没满但已经在换页）
    if m.swap_total_kb > 0 {
        let swap_pct = m.swap_used_kb as f64 / m.swap_total_kb as f64 * 100.0;
        if swap_pct >= 50.0 {
            out.push(Finding {
                level: Level::Warn,
                title: format!("Swap 已用 {swap_pct:.0}%"),
                detail: format!(
                    "{} / {} —— 内存可能长期吃紧",
                    gb(m.swap_used_kb),
                    gb(m.swap_total_kb)
                ),
            });
        }
    }

    // CPU：同时看瞬时和窗口峰值，瞬时 0% 但峰值 100% 也是信息
    let peak = d.history.as_ref().map(|h| h.cpu_max).unwrap_or(0.0);
    if m.cpu_pct >= CPU_WARN || peak >= CPU_WARN {
        out.push(Finding {
            level: Level::Warn,
            title: format!("CPU 使用率偏高（当前 {:.1}%，窗口峰值 {:.1}%）", m.cpu_pct, peak),
            detail: format!(
                "{} 核 · 负载 {}",
                d.static_info.cpu_cores,
                m.load
                    .iter()
                    .map(|l| format!("{l:.2}"))
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        });
    }

    // 负载超过核数 = 有排队
    if let Some(load1) = m.load.first() {
        let cores = d.static_info.cpu_cores.max(1) as f64;
        if *load1 > cores {
            out.push(Finding {
                level: Level::Warn,
                title: format!("1 分钟负载 {load1:.2} 超过核数 {cores:.0}"),
                detail: "有任务在排队等 CPU".into(),
            });
        }
    }

    // 温度
    let hot: Vec<String> = d
        .hardware
        .temps
        .iter()
        .filter(|t| t.celsius >= TEMP_WARN)
        .map(|t| format!("{} {} {:.0}°C", t.chip, t.label, t.celsius))
        .collect();
    if !hot.is_empty() {
        out.push(Finding {
            level: Level::Warn,
            title: format!("温度超过 {TEMP_WARN:.0}°C"),
            detail: hot.join("；"),
        });
    }

    // 端口：一个都没有通常是"服务没起来"，值得提一句
    if d.services.port_total == 0 {
        out.push(Finding {
            level: Level::Info,
            title: "没有读到监听端口".into(),
            detail: "可能是这台机器确实没跑服务，也可能是采集权限不足（非 root 下 ss -p 拿不到进程名）".into(),
        });
    }

    // 窗口里出现过明显波动
    if let Some(h) = &d.history {
        if h.cpu_max >= CPU_WARN && h.cpu_avg < CPU_WARN / 2.0 {
            out.push(Finding {
                level: Level::Info,
                title: format!("最近 {} 小时有过 CPU 尖峰", h.hours),
                detail: format!(
                    "峰值 {:.1}%、均值 {:.1}% —— 短时尖峰，不一定是问题",
                    h.cpu_max, h.cpu_avg
                ),
            });
        }
    }

    if out.is_empty() {
        out.push(Finding {
            level: Level::Ok,
            title: "未发现明显异常".into(),
            detail: format!(
                "CPU {:.1}%、内存 {:.1}%、磁盘最高 {:.0}%、{} 个监听端口、无失败单元",
                m.cpu_pct,
                m.mem_pct,
                m.disks.iter().map(|d| d.use_pct).fold(0.0, f64::max),
                d.services.port_total
            ),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

fn human_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分钟")
    } else {
        format!("{m} 分钟")
    }
}

fn fmt_rate(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bps / 1024.0 / 1024.0)
    } else if bps >= 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// Markdown 报告。`ai_summary` 为 None 时不出现"AI 结论"这一节。
/// 另外带上从正文剥离出的模型思考。报告侧过去只把它写进日志、内容直接丢，
/// 与 `think.ts` 声明的"搬移不是删除"矛盾 —— 现在放进折叠块，结论正文不受干扰。
pub fn render_markdown(
    d: &ReportData,
    ai_summary: Option<&str>,
    ai_thinking: Option<&str>,
) -> String {
    let s = &d.static_info;
    let m = &d.metrics;
    let mut o = String::new();

    o.push_str(&format!("# 巡检报告 · {}\n\n", d.host_label));
    o.push_str(&format!("生成时间：{}（本机时间）\n\n", d.generated_at));
    o.push_str("## 一、结论\n\n");
    for f in findings(d) {
        o.push_str(&format!(
            "- {} **{}** —— {}\n",
            f.level.markdown_badge(),
            f.title,
            f.detail
        ));
    }
    o.push('\n');

    let ai_text = ai_summary.filter(|a| !a.trim().is_empty());
    let think_text = ai_thinking.filter(|t| !t.trim().is_empty());
    let has_ai = ai_text.is_some() || think_text.is_some();
    if has_ai {
        o.push_str("## 二、AI 结论\n\n");
        if let Some(ai) = ai_text {
            o.push_str(ai.trim());
            o.push_str("\n\n");
        }
        if let Some(th) = think_text {
            o.push_str(&format!(
                "<details><summary>模型思考（已自动剥离，{} 字）</summary>\n\n{}\n\n</details>\n\n",
                th.chars().count(),
                th.trim()
            ));
        }
    }

    o.push_str(&format!(
        "## {}、主机与系统\n\n",
        if has_ai { "三" } else { "二" }
    ));
    o.push_str("| 项 | 值 |\n|---|---|\n");
    o.push_str(&format!("| 主机名 | {} |\n", s.hostname));
    o.push_str(&format!("| 系统 | {} |\n", s.os_pretty));
    o.push_str(&format!("| 内核 | {} ({}) |\n", s.kernel, s.arch));
    o.push_str(&format!("| CPU | {} × {} |\n", s.cpu_model, s.cpu_cores));
    o.push_str(&format!("| 内存 | {} |\n", gb(s.mem_total_kb)));
    o.push_str(&format!("| 运行时长 | {} |\n", human_uptime(s.uptime_secs)));
    o.push('\n');

    o.push_str("## 当前负载\n\n");
    o.push_str("| 指标 | 值 |\n|---|---|\n");
    o.push_str(&format!("| CPU | {:.1}% |\n", m.cpu_pct));
    o.push_str(&format!(
        "| 内存 | {:.1}%（{} / {}） |\n",
        m.mem_pct,
        gb(m.mem_used_kb),
        gb(m.mem_total_kb)
    ));
    if m.swap_total_kb > 0 {
        o.push_str(&format!(
            "| Swap | {:.1}%（{} / {}） |\n",
            m.swap_used_kb as f64 / m.swap_total_kb as f64 * 100.0,
            gb(m.swap_used_kb),
            gb(m.swap_total_kb)
        ));
    }
    if !m.load.is_empty() {
        o.push_str(&format!(
            "| 负载 (1/5/15 分钟) | {} |\n",
            m.load
                .iter()
                .map(|l| format!("{l:.2}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    o.push_str(&format!("| 进程数 | {} |\n", m.proc_total));
    o.push_str(&format!("| 监听端口 | {} |\n", d.services.port_total));
    if !m.net.is_empty() {
        let rx: f64 = m.net.iter().map(|n| n.rx_bps).sum();
        let tx: f64 = m.net.iter().map(|n| n.tx_bps).sum();
        o.push_str(&format!(
            "| 网络 | ↓ {} ↑ {} |\n",
            fmt_rate(rx),
            fmt_rate(tx)
        ));
    }
    o.push('\n');

    if !d.processes.is_empty() {
        o.push_str("## 进程\n\n");
        o.push_str(&format!(
            "共 {} 个进程。下面按 CPU 和内存各取前 8（同一份采集数据，排序规则一致）。\n\n",
            d.metrics.proc_total
        ));
        for (title, by_mem) in [("CPU 占用最高", false), ("内存占用最高", true)] {
            o.push_str(&format!(
                "**{}**\n\n| PID | 进程 | 状态 | CPU | 内存 |\n|---|---|---|---|---|\n",
                title
            ));
            for p in top_procs(&d.processes, by_mem, 8) {
                o.push_str(&format!(
                    "| {} | {} | {} | {:.1}% | {} |\n",
                    p.pid,
                    p.name,
                    p.state,
                    p.cpu_pct,
                    mb(p.rss_kb)
                ));
            }
            o.push('\n');
        }
    }

    o.push_str("## 网络\n\n");
    o.push_str("| 项 | ↓ 接收 | ↑ 发送 |\n|---|---|---|\n");
    {
        let rx: f64 = m.net.iter().map(|n| n.rx_bps).sum();
        let tx: f64 = m.net.iter().map(|n| n.tx_bps).sum();
        o.push_str(&format!(
            "| 当前（{} 个网卡合计） | {} | {} |\n",
            m.net.len(),
            fmt_rate(rx),
            fmt_rate(tx)
        ));
        if let Some(h) = &d.history {
            o.push_str(&format!(
                "| {} 小时平均 | {} | {} |\n",
                h.hours,
                fmt_rate(h.net_rx_avg * 1024.0),
                fmt_rate(h.net_tx_avg * 1024.0)
            ));
            o.push_str(&format!(
                "| {} 小时峰值 | {} | {} |\n",
                h.hours,
                fmt_rate(h.net_rx_max * 1024.0),
                fmt_rate(h.net_tx_max * 1024.0)
            ));
        }
    }
    o.push('\n');

    if let Some(p) = &d.ping {
        o.push_str("| 链路质量（到默认网关） | 值 |\n|---|---|\n");
        o.push_str(&format!("| 目标 | {} |\n", p.target));
        o.push_str(&format!(
            "| 平均时延 | {:.2} ms（min {:.2} / max {:.2}） |\n",
            p.rtt_avg, p.rtt_min, p.rtt_max
        ));
        if p.jitter > 0.0 {
            o.push_str(&format!("| 抖动 | {:.2} ms |\n", p.jitter));
        }
        o.push_str(&format!(
            "| 丢包 | {:.0}%（{}/{} 个包通） |\n",
            p.loss_pct, p.recv, p.sent
        ));
        // 时延趋势（#10）：窗口内均值/峰值取自 24h 档序列（慢采样 15s 一档）。
        if let Some(tier) = d.ping_series.iter().find(|s| s.hours == 24 && !s.points.is_empty()) {
            let n = tier.points.len() as f64;
            let avg = tier.points.iter().map(|p| p[1]).sum::<f64>() / n;
            let max = tier.points.iter().map(|p| p[1]).fold(0.0, f64::max);
            let loss_max = tier.points.iter().map(|p| p[3]).fold(0.0, f64::max);
            o.push_str(&format!(
                "| 时延趋势（最近 24h，{} 个采样） | 均值 {:.1} ms / 峰值 {:.1} ms |\n",
                tier.points.len(),
                avg,
                max
            ));
            o.push_str(&format!("| 丢包峰值（最近 24h） | {:.0}% |\n", loss_max));
        }
    }
    o.push('\n');

    o.push_str("## 磁盘\n\n");
    o.push_str("| 挂载点 | 已用 | 总量 | 使用率 |\n|---|---|---|---|\n");
    for disk in &m.disks {
        o.push_str(&format!(
            "| {} | {} | {} | {:.0}% |\n",
            disk.mount,
            gb(disk.used_kb),
            gb(disk.total_kb),
            disk.use_pct
        ));
    }
    o.push('\n');

    o.push_str("## 服务与端口\n\n");
    if d.services.failed.is_empty() {
        o.push_str("- systemd：没有失败单元\n");
    } else {
        o.push_str(&format!("- systemd：**{} 个失败单元**\n", d.services.failed.len()));
        for u in &d.services.failed {
            o.push_str(&format!("  - `{}` {}\n", u.name, u.desc));
        }
    }
    if d.services.ports.is_empty() {
        o.push_str("- 监听端口：未读到\n");
    } else {
        let list: Vec<String> = d
            .services
            .ports
            .iter()
            .map(|p| format!("{}/{}", p.port, p.proto))
            .collect();
        o.push_str(&format!("- 监听端口（{} 个）：{}\n", d.services.port_total, list.join("、")));
    }
    if d.services.docker_available {
        o.push_str(&format!("- 容器：{} 个\n", d.services.containers.len()));
        for c in &d.services.containers {
            o.push_str(&format!("  - {} —— {}\n", c.name, c.status));
        }
    } else {
        o.push_str("- 容器：未检测到 docker（无守护进程或无权限）\n");
    }
    o.push('\n');

    if !d.hardware.temps.is_empty() || !d.hardware.gpus.is_empty() {
        o.push_str("## 温度与 GPU\n\n");
        for t in &d.hardware.temps {
            o.push_str(&format!("- {} {}：{:.0}°C\n", t.chip, t.label, t.celsius));
        }
        for f in &d.hardware.fans {
            o.push_str(&format!("- 风扇 {} {}：{} RPM\n", f.chip, f.label, f.rpm));
        }
        for g in &d.hardware.gpus {
            let mut bits = vec![g.name.clone()];
            if let Some(u) = g.util_pct {
                bits.push(format!("利用率 {u:.0}%"));
            }
            if let Some(t) = g.temp_c {
                bits.push(format!("{t:.0}°C"));
            }
            if let Some(w) = g.power_w {
                bits.push(format!("{w:.0} W"));
            }
            o.push_str(&format!("- {}\n", bits.join(" · ")));
        }
        o.push('\n');
    }

    if let Some(h) = &d.history {
        o.push_str(&format!("## 最近 {} 小时趋势\n\n", h.hours));
        o.push_str(&format!(
            "（{} 个聚合点，来自 {} 行采样）\n\n",
            h.points, h.raw_rows
        ));
        o.push_str("| 指标 | 均值 | 峰值 |\n|---|---|---|\n");
        o.push_str(&format!("| CPU | {:.1}% | {:.1}% |\n", h.cpu_avg, h.cpu_max));
        o.push_str(&format!("| 内存 | {:.1}% | {:.1}% |\n", h.mem_avg, h.mem_max));
        o.push_str(&format!("| 负载 (1 分钟) | — | {:.2} |\n", h.load_max));
        o.push_str(&format!(
            "| 网络 | ↓ {} ↑ {} | — |\n",
            fmt_rate(h.net_rx_avg),
            fmt_rate(h.net_tx_avg)
        ));
        o.push('\n');
    } else {
        o.push_str("## 趋势\n\n- 这段时间没有落盘的历史数据（可能是刚连上，或历史落盘被关掉了）\n\n");
    }

    o.push_str("---\n\n");
    o.push_str("由 SSHBox 生成 · 所有数值来自对目标机的实时采集，未做任何人工修改\n");
    o
}

/// 自包含 HTML（可直接发给同事，双击就能看）。
/// 另外带一个"模型思考"折叠块（默认收起）。
pub fn render_html(
    d: &ReportData,
    ai_summary: Option<&str>,
    ai_thinking: Option<&str>,
) -> String {
    let s = &d.static_info;
    let m = &d.metrics;
    let mut o = String::new();
    o.push_str("<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">");
    o.push_str(&format!("<title>巡检报告 · {}</title>", d.host_label));
    o.push_str(
        "<style>\
         :root{color-scheme:light dark}\
         body{font:14px/1.6 system-ui,-apple-system,'Segoe UI',sans-serif;max-width:900px;margin:32px auto;padding:0 20px}\
         h1{font-size:22px;margin-bottom:4px}h2{font-size:16px;margin-top:28px;border-bottom:1px solid #8883;padding-bottom:4px}\
         table{border-collapse:collapse;width:100%;margin:8px 0}th,td{border:1px solid #8883;padding:5px 8px;text-align:left}\
         th{background:#8881}\
         .f{padding:8px 10px;border-left:3px solid #888;margin:6px 0;border-radius:4px;background:#8881}\
         .crit{border-color:#d20f39}.warn{border-color:#df8e1d}.info{border-color:#1e66f5}.ok{border-color:#40a02b}\
         code{background:#8882;padding:1px 4px;border-radius:3px}\
         .meta{color:#888;font-size:12px}\
         .think{margin:8px 0;border:1px solid #8883;border-radius:4px;padding:6px 10px;background:#8881}\
         .think summary{cursor:pointer;color:#888;font-size:12px}\
         .think pre{white-space:pre-wrap;font:12px/1.55 ui-monospace,SFMono-Regular,monospace;margin:6px 0 0}\
         </style></head><body>",
    );
    o.push_str(&format!("<h1>巡检报告 · {}</h1>", esc(&d.host_label)));
    o.push_str(&format!(
        "<div class=\"meta\">生成时间：{}（本机时间）</div>",
        esc(&d.generated_at)
    ));

    o.push_str("<h2>结论</h2>");
    for f in findings(d) {
        o.push_str(&format!(
            "<div class=\"f {}\"><b>{}</b> —— {}</div>",
            f.level.html_class(),
            esc(&f.title),
            esc(&f.detail)
        ));
    }

    let ai_text = ai_summary.filter(|a| !a.trim().is_empty());
    let think_text = ai_thinking.filter(|t| !t.trim().is_empty());
    if ai_text.is_some() || think_text.is_some() {
        o.push_str("<h2>AI 结论</h2>");
        if let Some(ai) = ai_text {
            o.push_str(&format!("<div>{}</div>", esc(ai.trim()).replace('\n', "<br>")));
        }
        if let Some(th) = think_text {
            o.push_str(&format!(
                "<details class=\"think\"><summary>模型思考（已自动剥离，{} 字）</summary><pre>{}</pre></details>",
                th.chars().count(),
                esc(th.trim())
            ));
        }
    }

    o.push_str("<h2>主机与系统</h2><table>");
    for (k, v) in [
        ("主机名", s.hostname.clone()),
        ("系统", s.os_pretty.clone()),
        ("内核", format!("{} ({})", s.kernel, s.arch)),
        ("CPU", format!("{} × {}", s.cpu_model, s.cpu_cores)),
        ("内存", gb(s.mem_total_kb)),
        ("运行时长", human_uptime(s.uptime_secs)),
    ] {
        o.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&v)));
    }
    o.push_str("</table>");

    o.push_str("<h2>当前负载</h2><table>");
    o.push_str(&format!(
        "<tr><th>CPU</th><td>{:.1}%</td></tr>",
        m.cpu_pct
    ));
    o.push_str(&format!(
        "<tr><th>内存</th><td>{:.1}%（{} / {}）</td></tr>",
        m.mem_pct,
        gb(m.mem_used_kb),
        gb(m.mem_total_kb)
    ));
    if !m.load.is_empty() {
        o.push_str(&format!(
            "<tr><th>负载 1/5/15</th><td>{}</td></tr>",
            m.load
                .iter()
                .map(|l| format!("{l:.2}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    o.push_str(&format!(
        "<tr><th>进程数 / 监听端口</th><td>{} / {}</td></tr>",
        m.proc_total, d.services.port_total
    ));
    o.push_str("</table>");

    o.push_str("<h2>磁盘</h2><table><tr><th>挂载点</th><th>已用</th><th>总量</th><th>使用率</th></tr>");
    for disk in &m.disks {
        o.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.0}%</td></tr>",
            esc(&disk.mount),
            gb(disk.used_kb),
            gb(disk.total_kb),
            disk.use_pct
        ));
    }
    o.push_str("</table>");

    o.push_str("<h2>服务与端口</h2>");
    if d.services.failed.is_empty() {
        o.push_str("<div class=\"f ok\">systemd 没有失败单元</div>");
    } else {
        o.push_str(&format!(
            "<div class=\"f crit\">{} 个 systemd 单元失败：{}</div>",
            d.services.failed.len(),
            esc(&d
                .services
                .failed
                .iter()
                .map(|u| u.name.clone())
                .collect::<Vec<_>>()
                .join("、"))
        ));
    }
    if !d.services.ports.is_empty() {
        o.push_str(&format!(
            "<p>监听端口（{} 个）：{}</p>",
            d.services.port_total,
            esc(&d
                .services
                .ports
                .iter()
                .map(|p| format!("{}/{}", p.port, p.proto))
                .collect::<Vec<_>>()
                .join("、"))
        ));
    }

    if !d.processes.is_empty() {
        o.push_str("<h2>进程</h2>");
        o.push_str(&format!(
            "<div class=\"meta\">共 {} 个进程，下面按 CPU 和内存各取前 8</div>",
            d.metrics.proc_total
        ));
        for (title, by_mem) in [("CPU 占用最高", false), ("内存占用最高", true)] {
            o.push_str(&format!(
                "<h3>{}</h3><table><tr><th>PID</th><th>进程</th><th>状态</th><th>CPU</th><th>内存</th></tr>",
                title
            ));
            for p in top_procs(&d.processes, by_mem, 8) {
                o.push_str(&format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1}%</td><td>{}</td></tr>",
                    p.pid,
                    esc(&p.name),
                    esc(&p.state),
                    p.cpu_pct,
                    mb(p.rss_kb)
                ));
            }
            o.push_str("</table>");
        }
    }

    // 网络：当前速率 + 窗口内均值/峰值（历史里存了 net_rx/net_tx）
    // 注意：速率要按网卡求和，不能取单个 m.net_rx —— 多网卡机器上会少算。
    let rx: f64 = m.net.iter().map(|n| n.rx_bps).sum();
    let tx: f64 = m.net.iter().map(|n| n.tx_bps).sum();
    o.push_str("<h2>网络</h2><table><tr><th>项</th><th>↓ 接收</th><th>↑ 发送</th></tr>");
    o.push_str(&format!(
        "<tr><td>当前速率（{} 个网卡合计）</td><td>{}</td><td>{}</td></tr>",
        m.net.len(),
        fmt_rate(rx),
        fmt_rate(tx)
    ));
    if let Some(h) = &d.history {
        o.push_str(&format!(
            "<tr><td>{} 小时平均</td><td>{}</td><td>{}</td></tr>",
            h.hours,
            fmt_rate(h.net_rx_avg * 1024.0),
            fmt_rate(h.net_tx_avg * 1024.0)
        ));
        o.push_str(&format!(
            "<tr><td>{} 小时峰值</td><td>{}</td><td>{}</td></tr>",
            h.hours,
            fmt_rate(h.net_rx_max * 1024.0),
            fmt_rate(h.net_tx_max * 1024.0)
        ));
    }
    o.push_str("</table>");

    if let Some(h) = &d.history {
        o.push_str(&format!("<h2>最近 {} 小时趋势</h2>", h.hours));
        o.push_str(&format!(
            "<div class=\"meta\">{} 个聚合点，来自 {} 行采样</div>",
            h.points, h.raw_rows
        ));
        o.push_str("<table><tr><th>指标</th><th>均值</th><th>峰值</th></tr>");
        o.push_str(&format!(
            "<tr><td>CPU</td><td>{:.1}%</td><td>{:.1}%</td></tr>",
            h.cpu_avg, h.cpu_max
        ));
        o.push_str(&format!(
            "<tr><td>内存</td><td>{:.1}%</td><td>{:.1}%</td></tr>",
            h.mem_avg, h.mem_max
        ));
        o.push_str(&format!(
            "<tr><td>负载 (1 分钟)</td><td>—</td><td>{:.2}</td></tr>",
            h.load_max
        ));
        o.push_str("</table>");
    }

    if let Some(p) = &d.ping {
        o.push_str("<table><tr><th>链路质量（到默认网关）</th><th>值</th></tr>");
        o.push_str(&format!("<tr><td>目标</td><td>{}</td></tr>", esc(&p.target)));
        o.push_str(&format!(
            "<tr><td>平均时延</td><td>{:.2} ms（min {:.2} / max {:.2}）</td></tr>",
            p.rtt_avg, p.rtt_min, p.rtt_max
        ));
        if p.jitter > 0.0 {
            o.push_str(&format!("<tr><td>抖动</td><td>{:.2} ms</td></tr>", p.jitter));
        }
        o.push_str(&format!(
            "<tr><td>丢包</td><td>{:.0}%（{}/{} 个包通）</td></tr>",
            p.loss_pct, p.recv, p.sent
        ));
        o.push_str("</table>");
    }

    // 趋势图：数据内嵌 + 时间范围在浏览器里切换（自包含 SVG，不依赖网络）
    if !d.series.is_empty() || !d.ping_series.is_empty() {
        o.push_str("<h2>趋势图</h2>");
        o.push_str(
            "<div class=\"meta\">默认最近 24 小时。切换范围或选自定义起止时间都不重新生成报告 —— 数据已内嵌在文件里。</div>",
        );
        o.push_str("<div class=\"ranges\">");
        for (i, s) in d.series.iter().enumerate() {
            let on = if s.hours == 24 { " on" } else { "" };
            o.push_str(&format!(
                "<button class=\"rg{}\" data-i=\"{}\">{}</button>",
                on, i, s.label
            ));
        }
        o.push_str("<button class=\"rg\" id=\"rg-custom\">自定义…</button>");
        o.push_str(
            "<span id=\"custom-box\" style=\"display:none\"> <input type=\"datetime-local\" id=\"t-from\"> 至 <input type=\"datetime-local\" id=\"t-to\"> <button class=\"rg\" id=\"t-apply\">应用</button></span>",
        );
        o.push_str("</div><div id=\"charts\"></div>");
        o.push_str(&format!(
            "<script>const SERIES={};</script>",
            serde_json::to_string(&d.series).unwrap_or_else(|_| "[]".into())
        ));
        o.push_str(&format!(
            "<script>const PINGERIES={};</script>",
            serde_json::to_string(&d.ping_series).unwrap_or_else(|_| "[]".into())
        ));
        o.push_str(CHART_JS);
        o.push_str(PAGINATE_JS);
    }

    o.push_str("<hr><div class=\"meta\">由 SSHBox 生成 · 数值来自对目标机的实时采集</div></body></html>");
    o
}

/// 报告里的趋势图脚本。
///
/// **自包含**：不引 CDN、不内联 ECharts（那会让文件从几十 KB 涨到近 1 MB），
/// 用 SVG 手绘折线。数据已经内嵌，所以切换时间范围是纯前端重绘，
/// 不用重新生成报告，也不依赖网络 —— 报告当附件发出去照样能看。
const CHART_JS: &str = r##"
<style>
.ranges { margin: 6px 0 10px; }
.rg { background: #313244; color: #cdd6f4; border: 1px solid #45475a; border-radius: 4px;
      font-size: 12px; padding: 3px 10px; margin-right: 6px; cursor: pointer; }
.rg.on { background: #89b4fa; color: #1e1e2e; border-color: #89b4fa; font-weight: 600; }
.chart { margin: 0 0 10px; }
.chart svg { display: block; }
#custom-box { font-size: 12px; color: #a6adc8; }
#custom-box input { background: #313244; color: #cdd6f4; border: 1px solid #45475a;
                    border-radius: 4px; font-size: 12px; padding: 2px 4px; }
</style>
<script>
const METRICS = [
  { name: 'CPU 使用率', idx: 1, color: '#89b4fa', pct: true },
  { name: '内存使用率', idx: 2, color: '#a6e3a1', pct: true },
  { name: '网络（实线 接收 / 虚线 发送）', idx: 3, idx2: 4, color: '#f9e2af', color2: '#fab387' },
  { name: '磁盘 IO（实线 读 / 虚线 写）', idx: 5, idx2: 6, color: '#cba6f7', color2: '#f38ba8' },
  { name: '负载（1 分钟）', idx: 7, color: '#94e2d5' },
];
function kb(v) { return v >= 1024 ? (v / 1024).toFixed(2) + ' MB/s' : (v || 0).toFixed(1) + ' KB/s'; }
function val(v, pct) { return pct ? (v || 0).toFixed(1) + '%' : kb(v); }
function msv(v, pct) { return pct ? (v || 0).toFixed(1) + '%' : (v || 0).toFixed(1) + ' ms'; }
function hhmm(ts) {
  const d = new Date(ts * 1000), p = n => String(n).padStart(2, '0');
  return p(d.getMonth() + 1) + '/' + p(d.getDate()) + ' ' + p(d.getHours()) + ':' + p(d.getMinutes());
}
function appendChart(arr, i1, i2, name, c1, c2, fmt, pct) {
  // 必须自己取容器：这个函数是顶层函数，看不到 draw() 里的局部 box。
  // 之前漏了这一行，draw() 里调用它时抛 ReferenceError，导致初始化 IIFE 中断、
  // 所有范围按钮监听器都没绑上（点『1 小时』没反应），时延/丢包两张图也不画。
  const box = document.getElementById('charts');
  const a = arr.map(p => p[i1]);
  const b = i2 != null ? arr.map(p => p[i2]) : null;
  let max = Math.max.apply(null, a.concat(b || [0]));
  if (!isFinite(max) || max <= 0) max = 1;
  max *= 1.15;
  const W = 760, H = 130, L = 34, R = 12;
  const x = j => L + (W - L - R) * (j / Math.max(1, arr.length - 1));
  const y = v => H - 20 - (H - 40) * (Math.max(0, v) / max);
  const path = as => as.map((v, j) => (j ? 'L' : 'M') + x(j).toFixed(1) + ',' + y(v).toFixed(1)).join(' ');
  let s = '<svg viewBox="0 0 ' + W + ' ' + H + '" width="100%" height="' + H + '">';
  s += '<line x1="' + L + '" y1="' + (H - 20) + '" x2="' + (W - R) + '" y2="' + (H - 20) + '" stroke="#45475a"/>';
  s += '<text x="4" y="12" fill="#a6adc8" font-size="10">' + name + '</text>';
  s += '<text x="' + (W - R) + '" y="12" fill="#6c7086" font-size="10" text-anchor="end">峰值 ' + fmt(max, pct) + '</text>';
  s += '<path d="' + path(a) + '" fill="none" stroke="' + c1 + '" stroke-width="1.4"/>';
  if (b) s += '<path d="' + path(b) + '" fill="none" stroke="' + c2 + '" stroke-width="1.4" stroke-dasharray="4 3"/>';
  s += '<text x="' + L + '" y="' + (H - 6) + '" fill="#6c7086" font-size="9">' + hhmm(arr[0][0]) + '</text>';
  s += '<text x="' + (W - R) + '" y="' + (H - 6) + '" fill="#6c7086" font-size="9" text-anchor="end">' + hhmm(arr[arr.length - 1][0]) + '</text>';
  s += '</svg>';
  box.insertAdjacentHTML('beforeend', '<div class="chart">' + s + '</div>');
}
function draw(points, pi) {
  const box = document.getElementById('charts');
  box.innerHTML = '';
  const pp = (pi && pi.points) || [];
  if (!points.length && !pp.length) { box.textContent = '这段时间没有数据'; return; }
  for (const m of METRICS) {
    const a = points.map(p => p[m.idx]);
    const b = m.idx2 ? points.map(p => p[m.idx2]) : null;
    let max = Math.max.apply(null, a.concat(b || [0]));
    if (!isFinite(max) || max <= 0) max = 1;
    max *= 1.15;
    const W = 760, H = 130, L = 34, R = 12;
    const x = i => L + (W - L - R) * (i / Math.max(1, points.length - 1));
    const y = v => H - 20 - (H - 40) * (Math.max(0, v) / max);
    const path = arr => arr.map((v, i) => (i ? 'L' : 'M') + x(i).toFixed(1) + ',' + y(v).toFixed(1)).join(' ');
    let s = '<svg viewBox="0 0 ' + W + ' ' + H + '" width="100%" height="' + H + '">';
    s += '<line x1="' + L + '" y1="' + (H - 20) + '" x2="' + (W - R) + '" y2="' + (H - 20) + '" stroke="#45475a"/>';
    s += '<text x="4" y="12" fill="#a6adc8" font-size="10">' + m.name + '</text>';
    s += '<text x="' + (W - R) + '" y="12" fill="#6c7086" font-size="10" text-anchor="end">峰值 ' + val(max, m.pct) + '</text>';
    s += '<path d="' + path(a) + '" fill="none" stroke="' + m.color + '" stroke-width="1.4"/>';
    if (b) s += '<path d="' + path(b) + '" fill="none" stroke="' + m.color2 + '" stroke-width="1.4" stroke-dasharray="4 3"/>';
    s += '<text x="' + L + '" y="' + (H - 6) + '" fill="#6c7086" font-size="9">' + hhmm(points[0][0]) + '</text>';
    s += '<text x="' + (W - R) + '" y="' + (H - 6) + '" fill="#6c7086" font-size="9" text-anchor="end">' + hhmm(points[points.length - 1][0]) + '</text>';
    s += '</svg>';
    box.insertAdjacentHTML('beforeend', '<div class="chart">' + s + '</div>');
  }
  // 时延/丢包趋势（#10）：同一时间轴、同一档分辨率，丢包/时延分两张图。
  if (pp.length) {
    appendChart(pp, 1, 2, '时延（实线） / 抖动（虚线）', '#89dceb', '#fab387', msv, false);
    appendChart(pp, 3, null, '丢包率', '#f38ba8', null, msv, true);
  } else if (PINGERIES.some(s => s.points.length)) {
    box.insertAdjacentHTML('beforeend',
      '<div class="chart" style="font-size:12px;color:#a6adc8">时延趋势：所选范围无数据（连通期间每 15 秒自动记录）</div>');
  }
}
function pick(i) {
  const s = SERIES[i];
  if (!s) return;
  document.querySelectorAll('.rg[data-i]').forEach(b => b.classList.toggle('on', Number(b.dataset.i) === i));
  draw(s.points, PINGERIES[i]);
}
function applyCustom() {
  const from = document.getElementById('t-from').value, to = document.getElementById('t-to').value;
  if (!from || !to) return;
  const f = new Date(from).getTime() / 1000, t = new Date(to).getTime() / 1000;
  // 用能覆盖这段范围、且分辨率最高的那一档（自定义范围不重新生成报告，只用已有数据）
  let best = null, bestI = -1;
  for (const [i, s] of SERIES.entries()) {
    let a = s.points[0][0], z = s.points[s.points.length - 1][0];
    if (a <= f && z >= t) { if (!best || s.bucket_secs < best.bucket_secs) { best = s; bestI = i; } }
  }
  if (!best) { document.getElementById('charts').textContent = '所选范围超出了报告内嵌的数据范围（最多 7 天）'; return; }
  document.querySelectorAll('.rg[data-i]').forEach(b => b.classList.remove('on'));
  draw(best.points.filter(p => p[0] >= f && p[0] <= t), PINGERIES[bestI]);
}
(function () {
  const def = SERIES.findIndex(s => s.hours === 24);
  if (def >= 0) pick(def); else if (SERIES.length) pick(0);
  else if (PINGERIES.length) draw([], PINGERIES[0]);
  document.querySelectorAll('.rg[data-i]').forEach(b => b.addEventListener('click', () => pick(Number(b.dataset.i))));
  document.getElementById('rg-custom').addEventListener('click', () => {
    const box = document.getElementById('custom-box');
    box.style.display = box.style.display === 'none' ? 'inline' : 'none';
    const s = SERIES[SERIES.length - 1];
    if (s) {
      const f = v => { const d = new Date(v * 1000); d.setMinutes(d.getMinutes() - d.getTimezoneOffset()); return d.toISOString().slice(0, 16); };
      document.getElementById('t-from').value = f(s.points[0][0]);
      document.getElementById('t-to').value = f(s.points[s.points.length - 1][0]);
    }
  });
  document.getElementById('t-apply').addEventListener('click', applyCustom);
})();
</script>
"##;

/// 按某个指标取前 N 个进程。
///
/// 纯函数：报告里的「进程」小节和单测都走它，排序规则只有一处。
/// 并列时按 pid 兜底，保证同一份数据渲染两次结果一致。
fn top_procs<'a>(procs: &'a [ProcInfo], by_mem: bool, n: usize) -> Vec<&'a ProcInfo> {
    let mut v: Vec<&ProcInfo> = procs.iter().collect();
    v.sort_by(|a, b| {
        let (x, y) = if by_mem {
            (a.rss_kb as f64, b.rss_kb as f64)
        } else {
            (a.cpu_pct, b.cpu_pct)
        };
        y.partial_cmp(&x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pid.cmp(&b.pid))
    });
    v.truncate(n);
    v
}

fn mb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GB", kb as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.1} MB", kb as f64 / 1024.0)
    }
}


// 长列表分页：磁盘挂载点、监听端口这类可能几十行的表，默认只显示前 10 条，
// 配「展开全部 N 条」按钮。自包含原则不变——纯 JS，不引库。
const PAGINATE_JS: &str = r##"
<script>
(function () {
  document.querySelectorAll('table').forEach(function (t) {
    var trs = Array.prototype.slice.call(t.querySelectorAll('tr'));
    var data = trs.filter(function (r) { return !r.querySelector('th'); });
    if (data.length <= 10) return;
    data.forEach(function (r, i) { if (i >= 10) r.style.display = 'none'; });
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '展开全部 ' + data.length + ' 条';
    btn.style.cssText = 'margin:6px 0;padding:3px 10px;font-size:11px;cursor:pointer;' +
      'background:#313244;color:#cdd6f4;border:1px solid #45475a;border-radius:4px;display:block';
    btn.onclick = function () {
      var opening = data[data.length - 1].style.display === 'none';
      data.forEach(function (r) { r.style.display = opening ? '' : 'none'; });
      btn.textContent = opening ? '收起' : '展开全部 ' + data.length + ' 条';
    };
    t.parentNode.insertBefore(btn, t.nextSibling);
  });
})();
</script>
"##;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ReportResult {
    pub markdown_path: String,
    pub html_path: String,
    pub dir: String,
    pub findings: usize,
    pub critical: usize,
    pub ai_used: bool,
    pub ai_error: Option<String>,
}

/// 报告默认落在桌面（用户要发给同事的东西，放配置目录里没人找得到）。
fn default_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let desktop = home.join("Desktop");
        if desktop.is_dir() {
            return desktop;
        }
        return home;
    }
    crate::store::data_dir()
}

/// 文件名里不能出现路径分隔符与 Windows 保留字符。
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 给 HTML 报告准备三档趋势数据。
///
/// 每档都是"自己的窗口 + 合适的桶数"：1 小时用 1 分钟桶、24 小时用 5 分钟桶、
/// 7 天用 1 小时桶。这样前端切换时间范围时看到的曲线疏密一致，而内嵌的数据量
/// 始终在几百点级别（2 秒一条原始数据的话 24 小时就是 4 万多点，直接内嵌会爆）。
async fn chart_series(host_id: &str, now: chrono::DateTime<chrono::Local>) -> Vec<ChartSeries> {
    let mut out = Vec::new();
    for (label, hours, buckets) in [
        ("1 小时", 1u64, 60usize),
        ("24 小时", 24, 288),
        ("7 天", 168, 168),
    ] {
        let from = (now.timestamp() - (hours as i64) * 3600) as f64;
        let to = now.timestamp() as f64;
        let Ok(r) = history::history_range(host_id.to_string(), from, to, Some(buckets)).await
        else {
            continue;
        };
        if r.buckets.is_empty() {
            continue;
        }
        out.push(ChartSeries {
            label: label.to_string(),
            hours,
            bucket_secs: r.bucket_secs,
            points: r
                .buckets
                .iter()
                .map(|b| {
                    [
                        b.ts, b.cpu_pct, b.mem_pct, b.net_rx, b.net_tx, b.disk_r, b.disk_w, b.load1,
                    ]
                })
                .collect(),
        });
    }
    out
}

/// 时延/丢包趋势（#10）：与指标同窗口同桶数（ts 边界一致）。
/// 桶里时延为 None（该窗口没测到网关）→ 整点丢弃，不画 0 ms。
async fn chart_ping_series(
    host_id: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<PingChartSeries> {
    let mut out = Vec::new();
    for (label, hours, buckets) in [
        ("1 小时", 1u64, 60usize),
        ("24 小时", 24, 288),
        ("7 天", 168, 168),
    ] {
        let from = (now.timestamp() - (hours as i64) * 3600) as f64;
        let to = now.timestamp() as f64;
        let Ok(r) = history::ping_range(host_id.to_string(), from, to, Some(buckets)).await else {
            continue;
        };
        let points: Vec<[f64; 4]> = r
            .buckets
            .iter()
            .filter_map(|b| {
                b.latency_ms
                    .map(|lat| [b.ts, lat, b.jitter_ms.unwrap_or(0.0), b.loss_pct.unwrap_or(0.0)])
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        out.push(PingChartSeries {
            label: label.to_string(),
            hours,
            bucket_secs: r.bucket_secs,
            points,
        });
    }
    out
}

/// 让 AI 写一段结论。失败**不影响报告**：返回 Err 由调用方记进 ai_error。
/// 返回（结论正文，从正文剥离出的思考）。思考不再被丢弃 —— 调用方把它放进报告折叠块。
async fn ai_summary(d: &ReportData) -> Result<(String, Option<String>)> {
    let settings = crate::store::load_settings();
    let ai = &settings.ai;
    let id = ai.active_profile_id.trim();
    if id.is_empty() {
        anyhow::bail!("没有选择模型");
    }
    let Some(profile) = ai.profiles.iter().find(|p| p.id == id).cloned() else {
        anyhow::bail!("当前模型配置不存在");
    };
    let key = crate::store::get_secret(&crate::ai::key_entry(&profile.id)).ok();

    let facts = findings(d)
        .iter()
        .map(|f| format!("- [{}] {} —— {}", f.level.markdown_badge(), f.title, f.detail))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "你是一名运维工程师。下面是一台服务器的巡检数据，请写一段结论，要求：\n\
         1) 先一句话说整体状况；2) 再给最多 3 条按优先级排序的建议，每条一句话；\n\
         3) 只依据给定数据，不要编造没出现的指标；4) 不要复述所有数字；5) 用中文，不要 Markdown 标题。\n\n\
         主机：{}（{}，{} 核，内存 {}）\n\
         当前：CPU {:.1}%、内存 {:.1}%、进程 {} 个、监听端口 {} 个\n\
         磁盘：{}\n\
         已判定的异常：\n{}\n",
        d.host_label,
        d.static_info.os_pretty,
        d.static_info.cpu_cores,
        gb(d.static_info.mem_total_kb),
        d.metrics.cpu_pct,
        d.metrics.mem_pct,
        d.metrics.proc_total,
        d.services.port_total,
        d.metrics
            .disks
            .iter()
            .map(|x| format!("{} {:.0}%", x.mount, x.use_pct))
            .collect::<Vec<_>>()
            .join("、"),
        facts
    );
    // 报告这条也留一行日志：之前它走 send_once 不打日志，出了"报告里少一段 AI 结论"
    // 只能靠猜（实测提供方偶发空回复，见 ai::send_once 的说明）。
    // 报告含主机名/IP/磁盘路径/挂载卷名 —— 发往外置模型前先脱敏，与对话上下文同一规则。
    let prompt = {
        let (censored, n) = crate::ai::censor_sensitive(&prompt);
        if n > 0 {
            format!("{censored}\n（注：以上内容已脱敏 {n} 处敏感信息。）")
        } else {
            censored
        }
    };
    let t0 = std::time::Instant::now();
    let out = crate::ai::send_once(
        &profile,
        key.as_deref(),
        &[crate::ai::ChatMessage::user(prompt)],
    )
    .await;
    match &out {
        Ok(t) => log::info!(
            "[ai] 报告结论 profile={} model={} 端点={} {}ms，{} 字",
            profile.name,
            profile.model,
            crate::ai::endpoint(&profile),
            t0.elapsed().as_millis(),
            t.chars().count()
        ),
        Err(e) => log::warn!(
            "[ai] 报告结论失败 profile={} model={} {}ms：{e:#}",
            profile.name,
            profile.model,
            t0.elapsed().as_millis()
        ),
    }
    // 自建模型（Qwen3 风格）/ 中转不拆字段时常把思考写进正文 → 剥离后进报告
    out.map(|t| {
        let (answer, think) = crate::ai::strip_thinking(&t);
        if let Some(th) = &think {
            log::info!(
                "[ai] 报告结论：从正文剥离思考 {} 字（模型把思考写进了 content，已放进报告折叠块）",
                th.chars().count()
            );
        }
        (answer, think)
    })
}

#[tauri::command]
pub async fn report_generate(
    sid: String,
    state: State<'_, SessionManager>,
    hours: Option<u64>,
    dest_dir: Option<String>,
) -> std::result::Result<ReportResult, String> {
    let handle = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(&sid)
            .map(|s| (s.handle.clone(), s.info.clone()))
            .ok_or_else(|| "会话不存在（可能已经断开）".to_string())?
    };
    let (handle, info) = handle;

    let snap = monitor::collect_snapshot(&handle)
        .await
        .map_err(|e| format!("采集失败：{e:#}"))?;

    let hours = hours.unwrap_or(24).clamp(1, 24 * 30);
    let now = chrono::Local::now();
    let host_id = history::host_key(&sid);
    let from = (now.timestamp() - (hours as i64) * 3600) as f64;
    let to = now.timestamp() as f64;
    // 三档分辨率：HTML 里的时间切换直接在这些数据上重绘
    let series = chart_series(&host_id, now).await;

    let hist = history::history_range(host_id.clone(), from, to, Some(600))
        .await
        .ok()
        .and_then(|r| HistorySummary::from_buckets(hours, &r));

    // 主机标签：优先已保存的主机名，其次 user@host:port。
    // 报告是给人看的，"WSL Ubuntu 测试机" 比 "R9000P" 有用得多。
    let saved_name = info
        .host_id
        .as_deref()
        .and_then(crate::store::get_host)
        .map(|h| h.name)
        .filter(|n| !n.trim().is_empty());
    let target = format!("{}@{}:{}", info.username, info.host, info.port);
    let host_label = match saved_name {
        Some(name) => format!("{name}（{target}）"),
        None => target,
    };

    let data = ReportData {
        host_label,
        generated_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        static_info: snap.static_info,
        metrics: snap.metrics,
        services: snap.services,
        hardware: snap.hardware,
        history: hist,
        series,
        ping_series: chart_ping_series(&host_id, now).await,
        processes: snap.processes,
        ping: snap.ping,
    };

    let mut ai_used = false;
    let mut ai_error: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut ai_thinking: Option<String> = None;
    if crate::store::load_settings().ai.report_ai_summary {
        match ai_summary(&data).await {
            Ok((text, think)) => {
                summary = Some(text);
                ai_thinking = think;
                ai_used = true;
            }
            Err(e) => ai_error = Some(format!("{e:#}")),
        }
    }

    let md = render_markdown(&data, summary.as_deref(), ai_thinking.as_deref());
    let html = render_html(&data, summary.as_deref(), ai_thinking.as_deref());
    let dir = dest_dir
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let base = format!(
        "SSHBox-巡检报告-{}-{}",
        safe_name(&data.static_info.hostname),
        now.format("%Y%m%d-%H%M")
    );
    let md_path = dir.join(format!("{base}.md"));
    let html_path = dir.join(format!("{base}.html"));
    std::fs::write(&md_path, md).map_err(|e| format!("写入 Markdown 失败：{e}"))?;
    std::fs::write(&html_path, html).map_err(|e| format!("写入 HTML 失败：{e}"))?;

    let fs = findings(&data);
    Ok(ReportResult {
        markdown_path: md_path.display().to_string(),
        html_path: html_path.display().to_string(),
        dir: dir.display().to_string(),
        findings: fs.len(),
        critical: fs.iter().filter(|f| f.level == Level::Critical).count(),
        ai_used,
        ai_error,
    })
}

/// 打开报告所在目录（复用 opener 插件，和导出 CSV 的行为一致）。
#[tauri::command]
pub async fn report_reveal(app: AppHandle, path: String) -> std::result::Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("打开目录失败：{e}"))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{NetIf, ProcInfo, TempReading};
    use crate::ssh::DiskUsage;

    fn mk(disk_pct: f64, mem_pct: f64, failed: usize, ports: usize) -> ReportData {
        ReportData {
            host_label: "测试机 · Ubuntu 22.04".into(),
            generated_at: "2026-09-20 12:00:00".into(),
            static_info: StaticInfo {
                hostname: "r9000p".into(),
                os_pretty: "Ubuntu 22.04.5 LTS".into(),
                kernel: "5.15".into(),
                arch: "x86_64".into(),
                cpu_model: "Ryzen 7".into(),
                cpu_cores: 16,
                mem_total_kb: 8 * 1024 * 1024,
                uptime_secs: 7200,
                disks: vec![],
            },
            metrics: Metrics {
                platform: "Linux".into(),
                ts: 0.0,
                cpu_pct: 12.0,
                cpu_per_core: vec![],
                mem_total_kb: 8 * 1024 * 1024,
                mem_used_kb: (8.0 * 1024.0 * 1024.0 * mem_pct / 100.0) as u64,
                mem_pct,
                swap_total_kb: 2048 * 1024,
                swap_used_kb: 0,
                net: vec![NetIf {
                    name: "eth0".into(),
                    rx_bps: 1024.0,
                    tx_bps: 512.0,
                }],
                disk_io: vec![],
                disks: vec![DiskUsage {
                    mount: "/".into(),
                    total_kb: 100 * 1024 * 1024,
                    used_kb: (100.0 * 1024.0 * 1024.0 * disk_pct / 100.0) as u64,
                    use_pct: disk_pct,
                }],
                load: vec![1.2, 1.0, 0.8],
                processes: vec![ProcInfo {
                    pid: 1,
                    name: "init".into(),
                    state: "S".into(),
                    cpu_pct: 0.0,
                    rss_kb: 1024,
                }],
                proc_total: 41,
            },
            services: ServiceInfo {
                failed: (0..failed)
                    .map(|i| crate::monitor::FailedUnit {
                        name: format!("unit-{i}.service"),
                        desc: "failed".into(),
                    })
                    .collect(),
                ports: (0..ports)
                    .map(|i| crate::monitor::PortInfo {
                        proto: "tcp".into(),
                        port: 22 + i as u16,
                        addrs: vec![],
                    })
                    .collect(),
                port_total: ports,
                containers: vec![],
                docker_available: false,
            },
            hardware: HardwareInfo {
                temps: vec![TempReading {
                    chip: "coretemp".into(),
                    label: "Package".into(),
                    celsius: 55.0,
                }],
                fans: vec![],
                gpus: vec![],
            },
            history: None,
            series: vec![],
            ping_series: vec![],
            processes: vec![],
            ping: None,
        }
    }

    #[test]
    fn healthy_host_reports_no_anomalies() {
        let f = findings(&mk(40.0, 30.0, 0, 4));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].level, Level::Ok);
        assert!(f[0].title.contains("未发现明显异常"));
    }

    #[test]
    fn disk_thresholds_split_critical_and_warn() {
        let f = findings(&mk(92.0, 30.0, 0, 1));
        assert!(f.iter().any(|x| x.level == Level::Critical && x.title.contains("磁盘")));
        // 明细里要指出是哪个挂载点、剩多少
        let d = &f[0].detail;
        assert!(d.contains("/"), "{d}");
        assert!(d.contains("剩"), "要给出剩余空间: {d}");

        let w = findings(&mk(86.0, 30.0, 0, 1));
        assert!(w.iter().any(|x| x.level == Level::Warn && x.title.contains("磁盘")));
        assert!(!w.iter().any(|x| x.level == Level::Critical));
    }

    #[test]
    fn failed_units_are_critical_and_named() {
        let f = findings(&mk(40.0, 30.0, 2, 3));
        let crit = f.iter().find(|x| x.level == Level::Critical).expect("要有严重项");
        assert!(crit.title.contains("2 个"));
        assert!(crit.detail.contains("unit-0.service"));
        assert!(crit.detail.contains("unit-1.service"));
    }

    #[test]
    fn memory_thresholds_and_swap_pressure() {
        let mut d = mk(40.0, 95.0, 0, 1);
        let f = findings(&d);
        assert!(f.iter().any(|x| x.level == Level::Critical && x.title.contains("内存")));

        d.metrics.mem_pct = 50.0;
        d.metrics.swap_used_kb = d.metrics.swap_total_kb; // swap 打满
        let f2 = findings(&d);
        assert!(f2.iter().any(|x| x.title.contains("Swap")), "swap 吃紧要单独提");
    }

    #[test]
    fn load_above_cores_is_flagged() {
        let mut d = mk(40.0, 30.0, 0, 1);
        d.metrics.load = vec![20.0, 10.0, 5.0];
        let f = findings(&d);
        assert!(f.iter().any(|x| x.title.contains("负载") && x.title.contains("超过核数")));
    }

    #[test]
    fn hot_sensor_is_flagged_with_its_name() {
        let mut d = mk(40.0, 30.0, 0, 1);
        d.hardware.temps[0].celsius = 91.0;
        let f = findings(&d);
        let hot = f.iter().find(|x| x.title.contains("温度")).expect("要报温度");
        assert!(hot.detail.contains("coretemp"));
        assert!(hot.detail.contains("91"));
    }

    #[test]
    fn no_ports_is_an_info_not_an_alarm() {
        let f = findings(&mk(40.0, 30.0, 0, 0));
        assert!(f.iter().any(|x| x.level == Level::Info && x.title.contains("监听端口")));
        assert!(!f.iter().any(|x| x.level == Level::Critical));
    }

    #[test]
    fn markdown_has_every_section_and_the_ai_part_is_optional() {
        let d = mk(40.0, 30.0, 0, 2);
        let md = render_markdown(&d, None, None);
        for want in ["# 巡检报告", "## 一、结论", "主机与系统", "当前负载", "## 磁盘", "服务与端口"] {
            assert!(md.contains(want), "缺小节 {want}");
        }
        assert!(!md.contains("AI 结论"), "没传 AI 结论就不该出现这一节");

        let with_ai = render_markdown(&d, Some("整体正常。"), None);
        assert!(with_ai.contains("## 二、AI 结论"));
        assert!(with_ai.contains("整体正常。"));
        // 加了 AI 小节后，后续章节编号要跟着挪
        assert!(with_ai.contains("## 三、主机与系统"));
    }

    #[test]
    fn markdown_numbers_come_from_the_data() {
        let d = mk(77.0, 42.0, 0, 3);
        let md = render_markdown(&d, None, None);
        assert!(md.contains("77%"), "磁盘百分比要出现: {md}");
        assert!(md.contains("42.0%"), "内存百分比要出现");
        assert!(md.contains("41"), "进程数要出现");
        assert!(md.contains("2 小时"), "运行时长要可读: {md}");
    }

    /// 报告的趋势图必须**自包含**：数据内嵌、脚本内嵌、不引任何 CDN。
    /// 报告是要能当附件发出去、断网也能看的 —— 引 CDN 就等于白屏。
    #[test]
    fn html_embeds_chart_series_and_stays_self_contained() {
        let mut d = mk(40.0, 30.0, 0, 1);
        d.series = vec![ChartSeries {
            label: "24 小时".into(),
            hours: 24,
            bucket_secs: 300.0,
            points: vec![
                [1000.0, 12.5, 30.0, 1.0, 2.0, 0.0, 0.0, 0.5],
                [1300.0, 20.0, 31.0, 2.0, 3.0, 0.0, 0.0, 0.6],
            ],
        }];
        let html = render_html(&d, None, None);
        assert!(html.contains("const SERIES="), "趋势数据要内嵌");
        assert!(html.contains("12.5"), "数据点要真的在文件里");
        assert!(html.contains("id=\"charts\""), "要有图表容器");
        assert!(html.contains("function draw"), "画图脚本要在");
        assert!(html.contains("data-i=\"0\""), "要有时间范围按钮");
        assert!(html.contains("自定义"), "要能自定义时间");
        assert!(!html.contains("cdn.") && !html.contains("https://"), "不能引外部资源");
        // 没有历史数据时不该出现空的图表区
        let mut d2 = mk(10.0, 10.0, 0, 0);
        d2.series = vec![];
        assert!(!render_html(&d2, None, None).contains("id=\"charts\""));
    }

    /// 网络速率要按网卡求和（多网卡机器取单个字段会少算）。
    #[test]
    fn network_section_sums_interfaces() {
        let d = mk(10.0, 10.0, 0, 1);
        let md = render_markdown(&d, None, None);
        assert!(md.contains("## 网络"), "{md}");
        assert!(md.contains("网卡合计"), "要说清是合计: {md}");
    }


    /// 进程小节：按 CPU 与内存各取前 8，排序规则只有一处（top_procs）。
    #[test]
    fn process_section_lists_top_by_cpu_and_by_memory() {
        let mut d = mk(10.0, 10.0, 0, 0);
        d.metrics.proc_total = 3;
        d.processes = vec![
            ProcInfo { pid: 1, name: "cpu-hog".into(), state: "R".into(), cpu_pct: 88.0, rss_kb: 1024 },
            ProcInfo { pid: 2, name: "mem-hog".into(), state: "S".into(), cpu_pct: 1.0, rss_kb: 3 * 1024 * 1024 },
            ProcInfo { pid: 3, name: "idle".into(), state: "S".into(), cpu_pct: 0.0, rss_kb: 512 },
        ];
        let md = render_markdown(&d, None, None);
        assert!(md.contains("## 进程"), "{md}");
        assert!(md.contains("共 3 个进程"));
        assert!(md.contains("cpu-hog") && md.contains("mem-hog"));
        assert!(md.contains("3.0 GB"), "内存要按 GB 显示: {md}");
        // 排序：CPU 榜第一位必须是 cpu-hog，内存榜第一位必须是 mem-hog
        let cpu_part = md.split("CPU 占用最高").nth(1).unwrap_or("");
        assert!(cpu_part.find("cpu-hog").unwrap_or(9999) < cpu_part.find("mem-hog").unwrap_or(9999));
        let mem_part = md.split("内存占用最高").nth(1).unwrap_or("");
        assert!(mem_part.find("mem-hog").unwrap_or(9999) < mem_part.find("cpu-hog").unwrap_or(9999));

        let html = render_html(&d, None, None);
        assert!(html.contains("<h2>进程</h2>"));
        assert!(html.contains("cpu-hog"));
        // 进程名为空也不能渲染出破表
        assert!(top_procs(&d.processes, false, 2).len() == 2);
        assert!(top_procs(&d.processes, true, 1)[0].pid == 2);
    }

    /// 链路质量：有数据才显示；拿不到就整块不出现（不编 0 ms 骗人）。
    #[test]
    fn network_section_shows_latency_and_loss_only_when_available() {
        let mut d = mk(10.0, 10.0, 0, 0);
        d.ping = Some(PingInfo {
            target: "172.20.0.1".into(),
            sent: 3,
            recv: 3,
            loss_pct: 0.0,
            rtt_min: 0.04,
            rtt_avg: 0.05,
            rtt_max: 0.06,
            jitter: 0.007,
        });
        let md = render_markdown(&d, None, None);
        assert!(md.contains("链路质量"), "{md}");
        assert!(md.contains("0.05"), "平均时延要显示出来");
        assert!(md.contains("丢包"), "丢包要显示");
        let html = render_html(&d, None, None);
        assert!(html.contains("链路质量") && html.contains("172.20.0.1"));

        // 拿不到 ping（没网关/没装 ping）→ 整块不出现
        let mut d2 = mk(10.0, 10.0, 0, 0);
        d2.ping = None;
        assert!(!render_markdown(&d2, None, None).contains("链路质量"));
        assert!(!render_html(&d2, None, None).contains("链路质量"));
    }

    #[test]
    fn report_keeps_ai_thinking_in_details_block() {
        let d = mk(10.0, 30.0, 0, 4);
        let think = "用户想要查询各个桶的状态。\n这里的桶指 Namespace。";
        let md = render_markdown(&d, Some("整体状况良好。"), Some(think));
        let html = render_html(&d, Some("整体状况良好。"), Some(think));
        for (name, doc) in [("markdown", &md), ("html", &html)] {
            assert!(doc.contains("模型思考（已自动剥离"), "{name} 缺少思考折叠块标题");
            assert!(doc.contains("这里的桶指 Namespace。"), "{name} 把思考内容丢了");
            assert!(doc.contains("整体状况良好。"), "{name} 丢了结论正文");
        }
        assert!(html.contains("class=\"think\""), "html 思考块要可折叠");
        assert!(!html.contains("cdn."), "报告不引 CDN");
        // 不传思考时不能凭空多出折叠块
        let plain = render_html(&d, Some("整体状况良好。"), None);
        assert!(!plain.contains("模型思考"), "无思考时不该出现折叠块");
    }

    #[test]
    fn report_embeds_ping_trend_in_md_and_html() {
        let mut d = mk(10.0, 30.0, 0, 4);
        d.ping = Some(PingInfo {
            target: "192.168.1.1".into(),
            sent: 3,
            recv: 3,
            loss_pct: 0.0,
            rtt_min: 1.0,
            rtt_avg: 17.3,
            rtt_max: 50.2,
            jitter: 23.3,
        });
        d.ping_series = vec![PingChartSeries {
            label: "24 小时".into(),
            hours: 24,
            bucket_secs: 300.0,
            points: vec![
                [0.0, 10.0, 2.0, 0.0],
                [300.0, 20.0, 5.0, 10.0],
                [600.0, 5.0, 1.0, 0.0],
            ],
        }];
        let md = render_markdown(&d, None, None);
        assert!(md.contains("时延趋势（最近 24h，3 个采样）"), "{md}");
        assert!(md.contains("均值 11.7 ms"), "（10+20+5）/3 = 11.7：{md}");
        assert!(md.contains("丢包峰值（最近 24h） | 10%"), "{md}");
        let html = render_html(&d, None, None);
        assert!(html.contains("PINGERIES"), "HTML 要内嵌时延序列");
        assert!(html.contains("时延（实线）"), "HTML 要有时延/抖动趋势图");
        assert!(html.contains("丢包率"), "HTML 要有丢包趋势图");
        // 回归断言（真实事故）：appendChart 是顶层函数，看不到 draw() 里的局部 box。
        // 漏掉自己的取容器那行就是 ReferenceError → 初始化 IIFE 中断 → 时延/丢包两张图
        // 不画、所有时间范围按钮都点不动。靠这条断言挡住。
        assert_eq!(
            html.matches("const box = document.getElementById('charts')")
                .count(),
            2,
            "draw() 与 appendChart() 必须各自取一次 charts 容器"
        );
        assert!(!html.contains("cdn."), "依旧自包含，不引 CDN");
    }

    #[test]
    fn html_is_self_contained_and_escapes_input() {
        let mut d = mk(40.0, 30.0, 0, 1);
        d.static_info.hostname = "<script>alert(1)</script>".into();
        let html = render_html(&d, None, None);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(!html.contains("<script>alert"), "必须转义，否则报告能当 XSS 载体");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn history_summary_aggregates_and_needs_points() {
        let buckets = vec![
            history::Bucket {
                ts: 1.0,
                cpu_pct: 10.0,
                mem_pct: 20.0,
                net_rx: 100.0,
                net_tx: 50.0,
                disk_r: 0.0,
                disk_w: 0.0,
                load1: 1.0,
            },
            history::Bucket {
                ts: 2.0,
                cpu_pct: 30.0,
                mem_pct: 40.0,
                net_rx: 300.0,
                net_tx: 150.0,
                disk_r: 0.0,
                disk_w: 0.0,
                load1: 3.0,
            },
        ];
        let r = history::HistoryRange {
            buckets,
            raw_points: 120,
            bucket_secs: 60.0,
            oldest: 1.0,
            newest: 2.0,
        };
        let s = HistorySummary::from_buckets(24, &r).unwrap();
        assert_eq!(s.cpu_avg, 20.0);
        assert_eq!(s.cpu_max, 30.0);
        assert_eq!(s.mem_avg, 30.0);
        assert_eq!(s.load_max, 3.0);
        assert_eq!(s.net_rx_avg, 200.0);
        assert_eq!(s.points, 2);

        let empty = history::HistoryRange {
            buckets: vec![],
            raw_points: 0,
            bucket_secs: 0.0,
            oldest: 0.0,
            newest: 0.0,
        };
        assert!(HistorySummary::from_buckets(24, &empty).is_none());
    }

    #[test]
    fn safe_name_strips_path_characters() {
        assert_eq!(safe_name("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(safe_name("  r9000p  "), "r9000p");
    }
}
