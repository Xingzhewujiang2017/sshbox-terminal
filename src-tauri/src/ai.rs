//! 多协议 AI 客户端：OpenAI 兼容 / Anthropic / Gemini。
//!
//! 三条协议只在三处不同：**请求体形状**、**鉴权头**、**流式事件的字段名**。
//! 所以差异全部收进几个纯函数（`build_request` / `parse_delta` / `extract_text` /
//! `extract_error`），网络与读流只写一份 —— 这样「某家协议又变了」只改一个纯函数，
//! 而且它能在没有网络、没有密钥的情况下单测。
//!
//! 密钥**不进 settings.json**：走系统凭据管理器，entry = `ai:<profile-id>`。

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

fn default_protocol() -> String {
    "openai".into()
}
fn default_temperature() -> f32 {
    0.2
}
fn default_max_tokens() -> u32 {
    1024
}
fn default_true() -> bool {
    true
}

/// 一个模型接入点。密钥不在里面 —— 见 `key_entry`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiProfile {
    pub id: String,
    pub name: String,
    /// `openai` | `anthropic` | `gemini`
    #[serde(default = "default_protocol")]
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 由后端填：凭据存储里有没有这个 profile 的密钥（不持久化，也不外传）
    #[serde(default)]
    pub has_key: bool,
}

/// AI 相关设置。整块都有默认值，旧 settings.json 里没有 `ai` 也能读。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSettings {
    /// 留空 = 还没选（AI 入口禁用并提示去设置里挑一个）
    #[serde(default)]
    pub active_profile_id: String,
    /// 字段缺失时给预设；显式写成 `[]`（用户删光了）就尊重空列表。
    #[serde(default = "presets")]
    pub profiles: Vec<AiProfile>,
    /// 巡检报告里附一段 AI 结论（默认开，可关）
    #[serde(default = "default_true")]
    pub report_ai_summary: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        // 三个预设都放上，但**默认不选**：没配好之前 AI 入口保持禁用，
        // 不会出现「点一下报一堆错」的首次体验。
        AiSettings {
            active_profile_id: String::new(),
            profiles: presets(),
            report_ai_summary: true,
        }
    }
}

/// 开箱预设。base_url / model 都填好，只有需要密钥的那两家要用户补 key。
pub fn presets() -> Vec<AiProfile> {
    vec![
        AiProfile {
            id: "ollama-local".into(),
            name: "本地 Ollama".into(),
            protocol: "openai".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "qwen2.5:7b".into(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            has_key: false,
        },
        AiProfile {
            id: "siliconflow".into(),
            name: "SiliconFlow".into(),
            protocol: "openai".into(),
            base_url: "https://api.siliconflow.cn/v1".into(),
            model: "deepseek-ai/DeepSeek-V3".into(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            has_key: false,
        },
        AiProfile {
            id: "aiaaa".into(),
            name: "aiaaa.cc".into(),
            protocol: "openai".into(),
            base_url: "https://aiaaa.cc/v1".into(),
            model: "deepseek-v4-flash-0731".into(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            has_key: false,
        },
    ]
}

/// 密钥在凭据管理器里的条目名。
pub fn key_entry(profile_id: &str) -> String {
    format!("ai:{profile_id}")
}

// ---------------------------------------------------------------------------
// 协议
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
    Gemini,
}

impl Protocol {
    /// 未知值一律当 OpenAI 兼容 —— 这是事实标准，也是自定义端点的默认。
    pub fn parse(s: &str) -> Protocol {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Protocol::Anthropic,
            "gemini" | "google" => Protocol::Gemini,
            _ => Protocol::OpenAi,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "system".into(),
            content: content.into(),
        }
    }
}

/// 一次已经拼好的 HTTP 请求。返回结构体而不是直接发，是为了能单测。
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

/// 拼 base 与 path：base 的尾斜杠和 path 的首斜杠都不重复。
pub fn join_url(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    let p = path.trim_start_matches('/');
    if b.is_empty() {
        p.to_string()
    } else {
        format!("{b}/{p}")
    }
}

/// 把消息按协议拼成请求。
///
/// - OpenAI 兼容：system 留在 messages 里
/// - Anthropic：system 必须提到顶层 `system` 字段，否则 400
/// - Gemini：system 走 `systemInstruction`，assistant 要改名 `model`
pub fn build_request(
    profile: &AiProfile,
    key: Option<&str>,
    messages: &[ChatMessage],
    stream: bool,
) -> BuiltRequest {
    let proto = Protocol::parse(&profile.protocol);
    let key = key.unwrap_or("").trim().to_string();
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];

    match proto {
        Protocol::OpenAi => {
            if !key.is_empty() {
                headers.push(("authorization".into(), format!("Bearer {key}")));
            }
            BuiltRequest {
                url: join_url(&profile.base_url, "/chat/completions"),
                headers,
                body: serde_json::json!({
                    "model": profile.model,
                    "messages": messages.iter().map(|m| serde_json::json!({
                        "role": m.role, "content": m.content,
                    })).collect::<Vec<_>>(),
                    "temperature": profile.temperature,
                    "max_tokens": profile.max_tokens,
                    "stream": stream,
                }),
            }
        }
        Protocol::Anthropic => {
            if !key.is_empty() {
                headers.push(("x-api-key".into(), key));
            }
            // 版本头是 Anthropic 的硬要求，缺了直接 400。
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            let system: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect();
            let turns: Vec<serde_json::Value> = messages
                .iter()
                .filter(|m| m.role != "system")
                .map(|m| {
                    serde_json::json!({
                        "role": if m.role == "assistant" { "assistant" } else { "user" },
                        "content": m.content,
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "model": profile.model,
                "messages": turns,
                // max_tokens 在 Anthropic 是必填项，不能省。
                "max_tokens": profile.max_tokens,
                "temperature": profile.temperature,
                "stream": stream,
            });
            if !system.is_empty() {
                body["system"] = serde_json::json!(system.join("\n\n"));
            }
            BuiltRequest {
                url: join_url(&profile.base_url, "/messages"),
                headers,
                body,
            }
        }
        Protocol::Gemini => {
            if !key.is_empty() {
                // 放头不放 URL：密钥不该出现在日志/报错里的 query string 上。
                headers.push(("x-goog-api-key".into(), key));
            }
            let method = if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            let url = join_url(
                &profile.base_url,
                &format!("/models/{}:{method}", profile.model),
            );
            let url = if stream {
                format!("{url}?alt=sse")
            } else {
                url
            };
            let system: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect();
            let contents: Vec<serde_json::Value> = messages
                .iter()
                .filter(|m| m.role != "system")
                .map(|m| {
                    serde_json::json!({
                        "role": if m.role == "assistant" { "model" } else { "user" },
                        "parts": [{ "text": m.content }],
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "contents": contents,
                "generationConfig": {
                    "temperature": profile.temperature,
                    "maxOutputTokens": profile.max_tokens,
                },
            });
            if !system.is_empty() {
                body["systemInstruction"] =
                    serde_json::json!({ "parts": [{ "text": system.join("\n\n") }] });
            }
            BuiltRequest { url, headers, body }
        }
    }
}

/// 一行 SSE 负载（已剥掉 `data: ` 前缀）里的增量文本。
///
/// 返回 `None` 表示这行不含可见文本：心跳、角色声明、`[DONE]`、结束帧都走这里。
pub fn parse_delta(proto: Protocol, data: &str) -> Option<String> {
    let d = data.trim();
    if d.is_empty() || d == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(d).ok()?;
    match proto {
        Protocol::OpenAi => v
            .get("choices")?
            .get(0)?
            .get("delta")?
            .get("content")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Protocol::Anthropic => {
            // 只有 content_block_delta 带文本；message_start / ping / content_block_start 都没有。
            if v.get("type")?.as_str()? != "content_block_delta" {
                return None;
            }
            v.get("delta")?
                .get("text")?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        Protocol::Gemini => {
            let parts = v.get("candidates")?.get(0)?.get("content")?.get("parts")?;
            let text: String = parts
                .as_array()?
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
    }
}

/// 流是否到此结束。
pub fn is_done(proto: Protocol, data: &str) -> bool {
    let d = data.trim();
    if d == "[DONE]" {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(d) else {
        return false;
    };
    match proto {
        Protocol::OpenAi => false, // OpenAI 兼容只认 [DONE]
        Protocol::Anthropic => v.get("type").and_then(|t| t.as_str()) == Some("message_stop"),
        Protocol::Gemini => false, // Gemini 直接断流
    }
}

/// 非流式响应里的完整文本。
pub fn extract_text(proto: Protocol, body: &serde_json::Value) -> Option<String> {
    match proto {
        Protocol::OpenAi => body
            .get("choices")?
            .get(0)?
            .get("message")?
            .get("content")?
            .as_str()
            .map(|s| s.to_string()),
        Protocol::Anthropic => {
            let blocks = body.get("content")?.as_array()?;
            let text: String = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            (!text.is_empty()).then_some(text)
        }
        Protocol::Gemini => {
            let parts = body.get("candidates")?.get(0)?.get("content")?.get("parts")?;
            let text: String = parts
                .as_array()?
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            (!text.is_empty()).then_some(text)
        }
    }
}

/// 把 HTTP 状态 + 响应体翻成一句能看懂的中文。
///
/// 优先用服务端自己给的 message（三家都放在 `error.message`），
/// 拿不到再按状态码给常见原因 —— 光甩一个 401 用户不知道是 key 没填还是填错了。
pub fn extract_error(proto: Protocol, status: u16, body: &str) -> String {
    let _ = proto; // 三家的错误体形状一致，留着参数是为了以后分叉
    let server_msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
        })
        .unwrap_or_default();
    let hint = match status {
        401 | 403 => "密钥无效、没填或没有该模型的权限",
        404 => "base_url 或模型名不对",
        429 => "被限流了，等一会儿再试",
        500..=599 => "服务端错误",
        _ => "请求被拒绝",
    };
    let tail = if server_msg.is_empty() {
        String::new()
    } else {
        format!("：{}", server_msg.chars().take(300).collect::<String>())
    };
    format!("HTTP {status} · {hint}{tail}")
}

// ---------------------------------------------------------------------------
// 上下文裁剪
// ---------------------------------------------------------------------------

/// 按字符数粗略裁掉超长上下文（终端输出可能几万行）。
///
/// 保留**头部**（主机/命令上下文）与**尾部**（最近的报错），中间省略 ——
/// 报错和上下文两头最有信息量，中间那一大坨通常是重复日志。
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars < 64 {
        return text.to_string();
    }
    let head = max_chars / 3;
    let tail = max_chars - head - 32;
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}\n\n…（此处省略 {} 字符）…\n\n{t}", chars.len() - head - tail)
}

// ---------------------------------------------------------------------------
// 取消
// ---------------------------------------------------------------------------

/// 已取消的请求 id。前端点「停止」时塞进来，读流的循环每块检查一次。
static CANCELLED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn cancel(req_id: &str) {
    if let Ok(mut m) = CANCELLED.lock() {
        m.insert(req_id.to_string());
    }
}

fn is_cancelled(req_id: &str) -> bool {
    CANCELLED
        .lock()
        .map(|m| m.contains(req_id))
        .unwrap_or(false)
}

fn clear_cancel(req_id: &str) {
    if let Ok(mut m) = CANCELLED.lock() {
        m.remove(req_id);
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()
        .context("创建 HTTP 客户端失败")
}

fn apply_headers(
    mut req: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

/// 非流式请求：用于「测试连接」和报告结论段（要的就是完整一句话）。
///
/// **空回复会重试一次**。实测 aiaaa.cc 大约每 3 次就有 1 次返回 `content: ""`
/// （HTTP 200、没有 error 字段、耗时正常）—— 当成功处理的话，用户只会看到
/// 报告里少一段、对话框里空一格，而且完全不知道为什么。
pub async fn send_once(
    profile: &AiProfile,
    key: Option<&str>,
    messages: &[ChatMessage],
) -> Result<String> {
    let proto = Protocol::parse(&profile.protocol);
    for attempt in 0..2 {
        let built = build_request(profile, key, messages, false);
        let resp = apply_headers(client()?.post(&built.url), &built.headers)
            .json(&built.body)
            .send()
            .await
            .map_err(|e| anyhow!("请求失败：{e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(anyhow!(extract_error(proto, status, &text)));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| anyhow!("响应不是合法 JSON：{e}"))?;
        let out = extract_text(proto, &v).unwrap_or_default();
        if !out.trim().is_empty() {
            return Ok(out);
        }
        if attempt == 0 {
            log::warn!("[ai] 模型返回空内容，重试一次：{}", built.url);
        }
    }
    Err(anyhow!(
        "模型返回了空内容（HTTP 200 但没有正文）—— 可能是限流或模型异常，可再试一次或换一个模型"
    ))
}

/// 流式请求。每个增量通过 `on_delta` 交出去，返回完整文本。
///
/// 两条约定：
/// - 流中途断开**不丢已收到的内容** —— 部分回答也比一句「失败」有用；
/// - 「干净地结束但一个字都没有」**重试一次**（提供方偶发空回复，见 `send_once`）；
///   只在还没有任何增量时重试，所以不会把文本吐两遍。
pub async fn send_stream<F>(
    profile: &AiProfile,
    key: Option<&str>,
    messages: &[ChatMessage],
    req_id: &str,
    mut on_delta: F,
) -> Result<String>
where
    F: FnMut(&str),
{
    let proto = Protocol::parse(&profile.protocol);
    let mut acc = String::new();
    for attempt in 0..2 {
        let built = build_request(profile, key, messages, true);
        let mut resp = apply_headers(client()?.post(&built.url), &built.headers)
            .json(&built.body)
            .send()
            .await
            .map_err(|e| anyhow!("请求失败：{e}"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(extract_error(proto, status, &text)));
        }

        acc.clear();
        let mut buf = String::new();
        let mut finished = false;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow!("读取流失败：{e}"))?
        {
            if is_cancelled(req_id) {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE 以空行分隔事件；一行一行处理，半个事件留在 buf 里等下一块。
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue; // event: / id: / 注释行都不需要
                };
                let data = data.trim_start();
                if is_done(proto, data) {
                    finished = true;
                    break;
                }
                if let Some(text) = parse_delta(proto, data) {
                    acc.push_str(&text);
                    on_delta(&text);
                }
            }
            if finished {
                break;
            }
        }

        if is_cancelled(req_id) {
            // 用户点了停止：拿到多少算多少，不重试、不报错
            clear_cancel(req_id);
            return Ok(acc);
        }
        if !acc.trim().is_empty() {
            break;
        }
        if !finished {
            clear_cancel(req_id);
            return Err(anyhow!("模型没有返回任何内容（连接可能被中断）"));
        }
        if attempt == 0 {
            log::warn!("[ai] 模型返回空内容，重试一次：{}", built.url);
        }
    }
    clear_cancel(req_id);
    if acc.trim().is_empty() {
        return Err(anyhow!(
            "模型返回了空内容（HTTP 200 但没有正文）—— 可能是限流或模型异常，可再试一次或换一个模型"
        ));
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 这个 profile 最终会请求到哪个 URL —— 日志和"测试连接"都要显示它。
/// 三种协议的路径拼法不同（Gemini 把模型放进路径），所以只能在这里统一算。
pub fn endpoint(p: &AiProfile) -> String {
    match Protocol::parse(&p.protocol) {
        Protocol::OpenAi => join_url(&p.base_url, "/chat/completions"),
        Protocol::Anthropic => join_url(&p.base_url, "/messages"),
        Protocol::Gemini => format!(
            "{}?alt=sse",
            join_url(&p.base_url, &format!("/models/{}:streamGenerateContent", p.model))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(protocol: &str) -> AiProfile {
        AiProfile {
            id: "t".into(),
            name: "t".into(),
            protocol: protocol.into(),
            base_url: "https://example.com/v1/".into(),
            model: "m1".into(),
            temperature: 0.3,
            max_tokens: 512,
            has_key: false,
        }
    }

    #[test]
    fn join_url_does_not_double_or_drop_slash() {
        assert_eq!(join_url("https://a/v1/", "/chat"), "https://a/v1/chat");
        assert_eq!(join_url("https://a/v1", "chat"), "https://a/v1/chat");
        assert_eq!(join_url("https://a/v1///", "///chat"), "https://a/v1/chat");
        assert_eq!(join_url("", "/chat"), "chat");
    }

    #[test]
    fn openai_request_shape_and_auth_header() {
        let r = build_request(&p("openai"), Some("sk-1"), &[ChatMessage::user("hi")], true);
        assert_eq!(r.url, "https://example.com/v1/chat/completions");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer sk-1"));
        assert_eq!(r.body["model"], "m1");
        assert_eq!(r.body["stream"], true);
        assert_eq!(r.body["messages"][0]["role"], "user");
        assert_eq!(r.body["messages"][0]["content"], "hi");
    }

    #[test]
    fn anthropic_lifts_system_out_and_needs_version_header() {
        let msgs = vec![ChatMessage::system("be terse"), ChatMessage::user("hi")];
        let r = build_request(&p("anthropic"), Some("k"), &msgs, false);
        assert_eq!(r.url, "https://example.com/v1/messages");
        assert!(r.headers.iter().any(|(k, v)| k == "x-api-key" && v == "k"));
        assert!(r
            .headers
            .iter()
            .any(|(k, _)| k == "anthropic-version"));
        // system 不能留在 messages 里，否则 Anthropic 直接 400
        assert_eq!(r.body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(r.body["messages"][0]["role"], "user");
        assert_eq!(r.body["system"], "be terse");
        assert!(r.body["max_tokens"].is_number(), "max_tokens 必填");
    }

    #[test]
    fn endpoint_matches_the_url_each_protocol_actually_calls() {
        let e = endpoint(&p("openai"));
        assert_eq!(e, "https://example.com/v1/chat/completions");
        let e = endpoint(&p("anthropic"));
        assert_eq!(e, "https://example.com/v1/messages");
        let e = endpoint(&p("gemini"));
        assert_eq!(
            e,
            "https://example.com/v1/models/m1:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn endpoint_shows_where_a_local_profile_will_go() {
        let mut q = p("openai");
        q.base_url = "http://127.0.0.1:11434/v1".into();
        assert_eq!(endpoint(&q), "http://127.0.0.1:11434/v1/chat/completions");
    }

    #[test]
    fn gemini_uses_model_in_path_and_renames_assistant() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage {
                role: "assistant".into(),
                content: "yo".into(),
            },
        ];
        let r = build_request(&p("gemini"), Some("gk"), &msgs, true);
        assert_eq!(
            r.url,
            "https://example.com/v1/models/m1:streamGenerateContent?alt=sse"
        );
        assert!(r.headers.iter().any(|(k, v)| k == "x-goog-api-key" && v == "gk"));
        assert_eq!(r.body["contents"][0]["role"], "user");
        assert_eq!(r.body["contents"][1]["role"], "model", "assistant→model");
        assert_eq!(r.body["systemInstruction"]["parts"][0]["text"], "sys");
        assert!(r.body["generationConfig"]["maxOutputTokens"].is_number());
    }

    #[test]
    fn non_stream_gemini_has_no_alt_sse() {
        let r = build_request(&p("gemini"), None, &[ChatMessage::user("x")], false);
        assert_eq!(r.url, "https://example.com/v1/models/m1:generateContent");
        assert!(!r.url.contains("alt=sse"));
        assert!(!r.headers.iter().any(|(k, _)| k == "x-goog-api-key"), "没 key 就不带这个头");
    }

    #[test]
    fn sse_deltas_per_protocol() {
        // OpenAI：角色声明帧没有 content，必须跳过
        assert_eq!(
            parse_delta(Protocol::OpenAi, r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
            None
        );
        assert_eq!(
            parse_delta(Protocol::OpenAi, r#"{"choices":[{"delta":{"content":"你"}}]}"#),
            Some("你".into())
        );
        // Anthropic：只有 content_block_delta 有文本
        assert_eq!(
            parse_delta(Protocol::Anthropic, r#"{"type":"content_block_start"}"#),
            None
        );
        assert_eq!(
            parse_delta(
                Protocol::Anthropic,
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"好"}}"#
            ),
            Some("好".into())
        );
        // Gemini：parts 可能多段，拼起来
        assert_eq!(
            parse_delta(
                Protocol::Gemini,
                r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"text":"b"}]}}]}"#
            ),
            Some("ab".into())
        );
        assert_eq!(parse_delta(Protocol::Gemini, r#"{"candidates":[]}"#), None);
        // 心跳/结束/垃圾行都不该产出文本
        for bad in ["", "[DONE]", "not json", "{\"ping\":1}"] {
            assert_eq!(parse_delta(Protocol::OpenAi, bad), None, "bad={bad}");
        }
    }

    #[test]
    fn done_markers_per_protocol() {
        assert!(is_done(Protocol::OpenAi, "[DONE]"));
        assert!(!is_done(Protocol::OpenAi, r#"{"choices":[]}"#));
        assert!(is_done(Protocol::Anthropic, r#"{"type":"message_stop"}"#));
        assert!(!is_done(Protocol::Anthropic, r#"{"type":"message_delta"}"#));
        assert!(!is_done(Protocol::Gemini, r#"{"candidates":[]}"#));
    }

    #[test]
    fn non_stream_text_extraction() {
        let o: serde_json::Value =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert_eq!(extract_text(Protocol::OpenAi, &o), Some("hi".into()));

        let a: serde_json::Value =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"yo"}]}"#).unwrap();
        assert_eq!(extract_text(Protocol::Anthropic, &a), Some("yo".into()));

        let g: serde_json::Value =
            serde_json::from_str(r#"{"candidates":[{"content":{"parts":[{"text":"zz"}]}}]}"#).unwrap();
        assert_eq!(extract_text(Protocol::Gemini, &g), Some("zz".into()));

        let empty: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert_eq!(extract_text(Protocol::OpenAi, &empty), None);
    }

    #[test]
    fn errors_prefer_server_message_and_add_a_hint() {
        let e = extract_error(
            Protocol::OpenAi,
            401,
            r#"{"error":{"message":"Invalid API key provided"}}"#,
        );
        assert!(e.contains("401"), "{e}");
        assert!(e.contains("密钥"), "要有中文原因: {e}");
        assert!(e.contains("Invalid API key provided"), "要带服务端原话: {e}");

        let e2 = extract_error(Protocol::Gemini, 404, "not found");
        assert!(e2.contains("base_url"), "{e2}");

        // 服务端没给 JSON 也不能崩
        let e3 = extract_error(Protocol::Anthropic, 500, "<html>bad gateway</html>");
        assert!(e3.contains("服务端错误"), "{e3}");
    }

    #[test]
    fn truncate_keeps_both_ends() {
        let long: String = (0..500).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let cut = truncate_middle(&long, 100);
        assert!(cut.contains("省略"), "{cut}");
        assert!(cut.starts_with(&long[..10]), "头要留着");
        assert!(cut.ends_with(&long[long.len() - 10..]), "尾要留着");
        assert!(cut.chars().count() < long.chars().count());

        // 短文本原样返回
        assert_eq!(truncate_middle("short", 100), "short");
    }

    #[test]
    fn settings_default_ships_presets_but_selects_none() {
        let s = AiSettings::default();
        assert!(s.active_profile_id.is_empty(), "默认不选，AI 入口保持禁用");
        assert_eq!(s.profiles.len(), 3);
        assert!(s.report_ai_summary);
        // 旧 settings.json 没有 ai 字段也要能读
        let parsed: AiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.profiles.len(), 3);
    }

    #[test]
    fn profile_survives_missing_optional_fields() {
        let j = r#"{"id":"x","name":"X","base_url":"https://a/v1","model":"m"}"#;
        let p: AiProfile = serde_json::from_str(j).unwrap();
        assert_eq!(p.protocol, "openai");
        assert_eq!(p.max_tokens, 1024);
        assert!(!p.has_key);
    }

    #[test]
    fn protocol_parse_falls_back_to_openai() {
        assert_eq!(Protocol::parse("anthropic"), Protocol::Anthropic);
        assert_eq!(Protocol::parse("Claude"), Protocol::Anthropic);
        assert_eq!(Protocol::parse("gemini"), Protocol::Gemini);
        assert_eq!(Protocol::parse(""), Protocol::OpenAi);
        assert_eq!(Protocol::parse("whatever"), Protocol::OpenAi);
    }
}
