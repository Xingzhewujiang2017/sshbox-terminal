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
use crate::monitor::{self, HardwareInfo, Metrics, ServiceInfo};
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
        })
    }
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
pub fn render_markdown(d: &ReportData, ai_summary: Option<&str>) -> String {
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

    if let Some(ai) = ai_summary.filter(|a| !a.trim().is_empty()) {
        o.push_str("## 二、AI 结论\n\n");
        o.push_str(ai.trim());
        o.push_str("\n\n");
    }

    o.push_str(&format!(
        "## {}、主机与系统\n\n",
        if ai_summary.is_some() { "三" } else { "二" }
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
pub fn render_html(d: &ReportData, ai_summary: Option<&str>) -> String {
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

    if let Some(ai) = ai_summary.filter(|a| !a.trim().is_empty()) {
        o.push_str("<h2>AI 结论</h2>");
        o.push_str(&format!("<div>{}</div>", esc(ai.trim()).replace('\n', "<br>")));
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

    o.push_str("<hr><div class=\"meta\">由 SSHBox 生成 · 数值来自对目标机的实时采集</div></body></html>");
    o
}

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

/// 让 AI 写一段结论。失败**不影响报告**：返回 Err 由调用方记进 ai_error。
async fn ai_summary(d: &ReportData) -> Result<String> {
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
    out
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
    let hist = history::history_range(host_id, from, to, Some(600))
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
    };

    let mut ai_used = false;
    let mut ai_error: Option<String> = None;
    let mut summary: Option<String> = None;
    if crate::store::load_settings().ai.report_ai_summary {
        match ai_summary(&data).await {
            Ok(text) => {
                summary = Some(text);
                ai_used = true;
            }
            Err(e) => ai_error = Some(format!("{e:#}")),
        }
    }

    let md = render_markdown(&data, summary.as_deref());
    let html = render_html(&data, summary.as_deref());
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
        let md = render_markdown(&d, None);
        for want in ["# 巡检报告", "## 一、结论", "主机与系统", "当前负载", "## 磁盘", "服务与端口"] {
            assert!(md.contains(want), "缺小节 {want}");
        }
        assert!(!md.contains("AI 结论"), "没传 AI 结论就不该出现这一节");

        let with_ai = render_markdown(&d, Some("整体正常。"));
        assert!(with_ai.contains("## 二、AI 结论"));
        assert!(with_ai.contains("整体正常。"));
        // 加了 AI 小节后，后续章节编号要跟着挪
        assert!(with_ai.contains("## 三、主机与系统"));
    }

    #[test]
    fn markdown_numbers_come_from_the_data() {
        let d = mk(77.0, 42.0, 0, 3);
        let md = render_markdown(&d, None);
        assert!(md.contains("77%"), "磁盘百分比要出现: {md}");
        assert!(md.contains("42.0%"), "内存百分比要出现");
        assert!(md.contains("41"), "进程数要出现");
        assert!(md.contains("2 小时"), "运行时长要可读: {md}");
    }

    #[test]
    fn html_is_self_contained_and_escapes_input() {
        let mut d = mk(40.0, 30.0, 0, 1);
        d.static_info.hostname = "<script>alert(1)</script>".into();
        let html = render_html(&d, None);
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
