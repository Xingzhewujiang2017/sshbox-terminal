//! 故障演练台（Fault Lab）后端命令。
//!
//! - `exec_batch`：对单个会话跑一次性命令（复用采集通道的 exec_capture，
//!   non-interactive，带超时，返回 stdout/exit code/耗时）—— 前端对勾选的
//!   多台并行调用即为组播。
//! - `ping_now`：3s 高采用的快 ping —— 对目标机 `ping -c1` 到默认网关，
//!   解析时延/丢包。拿不到网关时三值皆 None（不编 0 骗人）。

use serde::Serialize;

use crate::monitor::{exec_capture_code, get_handle};

#[derive(Serialize)]
pub struct LabExec {
    pub sid: String,
    pub ok: bool,
    pub stdout: String,
    pub exit: i32,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

fn truncate_out(mut s: String, n: usize) -> String {
    if s.chars().count() <= n {
        return s;
    }
    s = s.chars().take(n).collect();
    s.push_str("\n…（输出过长，已截断）");
    s
}

#[tauri::command]
pub async fn exec_batch(
    sid: String,
    cmd: String,
    timeout_secs: Option<u64>,
) -> Result<LabExec, String> {
    let t0 = std::time::Instant::now();
    let timeout_s = timeout_secs.unwrap_or(60).clamp(1, 600);
    let handle = get_handle(&sid).ok_or_else(|| "演练台：会话已关闭或不存在".to_string())?;

    let fut = exec_capture_code(&handle, cmd.trim());
    let res =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_s), fut).await;
    let elapsed_ms = t0.elapsed().as_millis() as u64;

    Ok(match res {
        Ok(Ok((out, code))) => LabExec {
            sid,
            ok: true,
            stdout: truncate_out(out, 20_000),
            exit: code,
            elapsed_ms,
            error: None,
        },
        Ok(Err(e)) => LabExec {
            sid,
            ok: false,
            stdout: String::new(),
            exit: -1,
            elapsed_ms,
            error: Some(format!("{:#}", e)),
        },
        Err(_) => LabExec {
            sid,
            ok: false,
            stdout: String::new(),
            exit: -1,
            elapsed_ms,
            error: Some(format!("执行超时（{}s），已断开该通道", timeout_s)),
        },
    })
}

#[derive(Serialize)]
pub struct PingNow {
    /// 默认网关 IP；拿不到为 None。
    pub target: Option<String>,
    pub rtt_avg: Option<f64>,
    pub loss_pct: Option<f64>,
}

/// 快 ping：一次 `ping -c1` 到默认网关，返回解析结果。
/// 无默认路由 / 网关不通时 rtt_avg=None（丢包率仍如实给）。
#[tauri::command]
pub async fn ping_now(sid: String) -> Result<PingNow, String> {
    let handle = get_handle(&sid).ok_or_else(|| "演练台：会话已关闭".to_string())?;
    const SCRIPT: &str = r#"GW=$(ip route 2>/dev/null | awk '/^default/ {print $3; exit}'); [ -n "$GW" ] || { echo "NO_GW"; exit 0; }; ping -c4 -W1 "$GW" 2>/dev/null | tail -3; echo "GW=$GW""#;
    let (out, _) = exec_capture_code(&handle, SCRIPT)
        .await
        .map_err(|e| format!("{:#}", e))?;

    let mut target: Option<String> = None;
    let mut rtt_avg: Option<f64> = None;
    let mut loss_pct: Option<f64> = None;
    for line in out.lines() {
        if let Some(g) = line.strip_prefix("GW=") {
            target = Some(g.trim().to_string());
        }
        let l = parse_loss_pct(line);
        if l.is_some() {
            loss_pct = l;
        }
        let r = parse_rtt_avg(line);
        if r.is_some() {
            rtt_avg = r;
        }
    }
    Ok(PingNow {
        target,
        rtt_avg,
        loss_pct,
    })
}

/// "1 packets transmitted, 1 received, 0% packet loss, time 0ms" → 0.0
fn parse_loss_pct(line: &str) -> Option<f64> {
    let i = line.find("% packet loss")?;
    let head = &line[..i];
    let num: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    num.parse().ok()
}

/// "rtt min/avg/max/mdev = 0.79/0.79/0.79/0.00 ms" → 0.79
fn parse_rtt_avg(line: &str) -> Option<f64> {
    let i = line.find("= ")?;
    let rest = &line[i + 2..];
    rest.split('/').nth(1)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_parses() {
        assert_eq!(
            parse_loss_pct("1 packets transmitted, 1 received, 0% packet loss, time 0ms"),
            Some(0.0)
        );
        assert_eq!(
            parse_loss_pct("3 packets transmitted, 2 received, +1 errors, 33.3% packet loss, time 2000ms"),
            Some(33.3)
        );
        assert_eq!(parse_loss_pct("no packet loss here"), None);
    }

    #[test]
    fn rtt_parses() {
        assert_eq!(
            parse_rtt_avg("rtt min/avg/max/mdev = 0.795/0.795/0.795/0.023 ms"),
            Some(0.795)
        );
        assert_eq!(parse_rtt_avg("rtt min/avg/max/mdev = 17.273/23.306/50.234/0.921 ms"), Some(23.306));
        assert_eq!(parse_rtt_avg("100% packet loss, time 0ms"), None);
    }
}