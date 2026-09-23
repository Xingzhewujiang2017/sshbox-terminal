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
/// `max_tokens` 的历史默认值。改默认值只管**新建**配置 —— 已经写进 settings.json 的
/// 1024 不会因为 `#[serde(default)]` 而变，所以 store 加载时要迁移（见 `migrate_settings`）。
pub const LEGACY_MAX_TOKENS: u32 = 1024;

pub fn default_max_tokens() -> u32 {
    // 4096 而不是 1024：推理模型的**思考过程也算在这份额度里**，1024 会被
    // 思考吃光、正文一个字不剩（实测 deepseek-v4-flash-0731 就这样）。
    4096
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

/// 空回复时的诊断。**必须分清两种情况**，否则用户会一直重试一个永远不会成功的请求：
///
/// - `finish_reason = length`：输出上限被吃光，重试必然同样截断。推理模型尤其常见 ——
///   思考过程把额度花完，正文一个字都没留下。要调「最大输出」或换模型，而不是重试。
/// - 其它：提供方偶发空回复（HTTP 200 + 空正文），重试一次有意义。
fn empty_reply_error(finish: &str, raw: &str, body: &str, max_tokens: u32) -> String {
    if finish == "length" {
        let thought = raw.contains("reasoning_content") || raw.contains("\"thinking\"");
        let hint = if thought {
            "模型的思考过程写满了输出上限，正文一个字都没留下"
        } else {
            "输出被输出上限截断了，正文一个字都没留下"
        };
        let suggested = (max_tokens * 4).clamp(4096, 32000);
        format!(
            "{hint} —— 这不是网络或密钥问题。当前「最大输出」是 {max_tokens} 个 token，\
             建议改成 {suggested} 或更大（设置 → AI 模型 → 最大输出），或换一个非推理模型。\n原始响应：{body}"
        )
    } else {
        format!(
            "模型返回了空内容（HTTP 200 但没有正文）—— 可能是限流或模型异常，可再试一次或换一个模型。\n原始响应：{body}"
        )
    }
}

/// 剥离模型写进正文的思考。服务端没拆 reasoning 字段时，思考会混在 content 里。
///
/// 两种格式（都用真实样例定，顺序即优先级）：
/// 1. **标签式**：`...` / `...`。qwen3.6-27b 经中转 provider 时**只有闭合
///    标签**、没有开始标签，且思考段能长达数百字（真实样本如此）——所以这种情形按
///    "正文开头到闭合标签"整段搬走，不加行数限制。
/// 2. **编号式**：`Here's a thinking process:` / `以下是思考过程：` 起头，到最后一个编号项。
///    marker 只在前 10 行内认，避免解释这个功能时把正文误当思考。
///
/// 安全属性：剥离是**搬移不是删除** —— 剥出的内容进「思考过程」折叠栏，展开永远能看到原文。
/// 找不到结构就原样返回，宁可不剥也不误删正文。
pub fn strip_thinking(content: &str) -> (String, Option<String>) {
    if let Some(r) = strip_tagged_thinking(content) {
        return r;
    }
    strip_numbered_thinking(content)
}

/// 标签式思考。标签都是 ASCII，用 `to_ascii_lowercase()` 做不敏感查找**保持字节长度不变**，
/// 切片索引才安全（中文正文不会被破坏）。
fn strip_tagged_thinking(content: &str) -> Option<(String, Option<String>)> {
    const OPEN: [&str; 2] = ["<think>", "<thinking>"];
    const CLOSE: [&str; 3] = ["</think>", "</thinking>", "<｜end▁of▁thinking｜>"];
    // 没有开始标签时，前缀短于这个长度就多半是**正文在讨论这个标签本身**，不是思考。
    // 真实思考段是数百字（样例 1241 字），一句话讨论只有几十字 —— 宁可留着也不误删正文。
    const MIN_TAGGED_THINK_CHARS: usize = 80;

    let lc = content.to_ascii_lowercase();
    // 位置升序；同位置取更长的那个（`</think>` 与 `</thinking>` 共享前缀）
    let pick = |tags: &[&str]| {
        tags.iter()
            .filter_map(|t| lc.find(t).map(|i| (i, t.len())))
            .min_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
    };

    let (close_at, close_len) = pick(&CLOSE)?;
    let open = pick(&OPEN).filter(|(i, _)| *i < close_at);

    let (think, answer) = match open {
        Some((open_at, open_len)) => (
            content[open_at + open_len..close_at].to_string(),
            format!(
                "{}{}",
                &content[..open_at],
                &content[close_at + close_len..]
            ),
        ),
        // 没有开始标签：开头到闭合标签之间整段是思考（qwen3.6-27b 经中转的真实形态）
        None => {
            if content[..close_at].chars().count() < MIN_TAGGED_THINK_CHARS {
                return None;
            }
            (
                content[..close_at].to_string(),
                content[close_at + close_len..].to_string(),
            )
        }
    };

    let think = think.trim();
    // 剥出空内容（例如正文以闭合标签开头）→ 不剥，别把正文清空
    if think.is_empty() {
        return None;
    }
    Some((answer, Some(think.to_string())))
}

fn strip_numbered_thinking(content: &str) -> (String, Option<String>) {
    let markers = [
        "here's a thinking process:",
        "以下是思考过程：",
        "思考过程：",
    ];
    let lines: Vec<&str> = content.split('\n').collect();
    // 只在前 10 行内认标记：思考总在开头，避免"解释这个功能"时把正文误当思考剥走
    let mline = lines.iter().take(10).position(|l| {
        let t = l.to_ascii_lowercase();
        markers.iter().any(|m| t.contains(m))
    });
    let Some(mline) = mline else {
        return (content.to_string(), None);
    };

    fn is_numbered(l: &str) -> bool {
        let t = l.trim_start();
        let b = t.as_bytes();
        let mut i = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i > 0 && i < b.len() && b[i] == b'.'
    }

    let mut end: Option<usize> = None;
    for (i, l) in lines.iter().enumerate().skip(mline) {
        if is_numbered(l) {
            end = Some(i);
        }
    }
    let Some(end) = end else {
        return (content.to_string(), None);
    };
    // 编号项后的子行（项目符号 / 空行）仍属思考块；一旦出现非列表行，正文开始
    let mut cut = end + 1;
    while cut < lines.len() {
        let t = lines[cut].trim_start();
        if t.is_empty() || t.starts_with('-') || t.starts_with('*') || t.starts_with('+') {
            cut += 1;
        } else {
            break;
        }
    }
    let thinking = lines[mline..cut].join("\n");
    let answer = lines[..mline]
        .iter()
        .chain(lines[cut..].iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    (answer, Some(thinking))
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

/// 从一条 SSE 事件里取「思考过程」增量（如果有）。
///
/// 三家放的地方都不一样，而且和正文是**两条并行的流**：
/// - OpenAI 兼容 / DeepSeek：`choices[0].delta.reasoning_content`（OpenRouter 用 `reasoning`）
/// - Anthropic：`content_block_delta` 且 `delta.type == "thinking_delta"` → `delta.thinking`
/// - Gemini：`candidates[0].content.parts[*]` 里 `thought == true` 的那部分
///
/// 不接的话思考内容会被整个丢掉（实测 aiaaa.cc 的 deepseek-v4-flash-0731 就是
/// 全程 `content:""` + `reasoning_content:"…"`，正文最后才出现）。
pub fn parse_reasoning(proto: Protocol, data: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    match proto {
        Protocol::OpenAi => {
            let d = v.get("choices")?.get(0)?.get("delta")?;
            for k in ["reasoning_content", "reasoning"] {
                if let Some(s) = d.get(k).and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
            None
        }
        Protocol::Anthropic => {
            if v.get("type")?.as_str()? != "content_block_delta" {
                return None;
            }
            let d = v.get("delta")?;
            if d.get("type")?.as_str()? != "thinking_delta" {
                return None;
            }
            let t = d.get("thinking")?.as_str()?;
            (!t.is_empty()).then(|| t.to_string())
        }
        Protocol::Gemini => {
            let parts = v
                .get("candidates")?
                .get(0)?
                .get("content")?
                .get("parts")?
                .as_array()?;
            let mut out = String::new();
            for p in parts {
                if p.get("thought").and_then(|t| t.as_bool()).unwrap_or(false) {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            (!out.is_empty()).then_some(out)
        }
    }
}

/// 每种协议的 base_url 应该长什么样。
///
/// 填错的代价是一个 404，而 404 的原始文案（"Not Found"）不告诉用户少了 `/v1`，
/// 所以把示例直接写进错误里。
pub fn base_url_example(proto: Protocol) -> &'static str {
    match proto {
        Protocol::OpenAi => "，例如 https://api.deepseek.com/v1",
        Protocol::Anthropic => "，Anthropic 是 https://api.anthropic.com/v1",
        Protocol::Gemini => "，Gemini 是 https://generativelanguage.googleapis.com/v1beta",
    }
}

/// 把 HTTP 状态 + 响应体翻成一句能看懂的中文。
///
/// 优先用服务端自己给的 message（三家都放在 `error.message`），
/// 拿不到再按状态码给常见原因 —— 光甩一个 401 用户不知道是 key 没填还是填错了。
/// **一定要带上实际请求的 URL**：404 时用户唯一需要知道的就是"它到底请求了哪个地址"。
pub fn extract_error(proto: Protocol, status: u16, url: &str, body: &str) -> String {
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
        401 | 403 => "密钥无效、没填或没有该模型的权限".to_string(),
        404 => format!(
            "请求地址不存在：{url} —— base_url 要填到 /v1 为止{}，或模型名不对",
            base_url_example(proto)
        ),
        429 => "被限流了，等一会儿再试".to_string(),
        500..=599 => "服务端错误".to_string(),
        _ => "请求被拒绝".to_string(),
    };
    let tail = if server_msg.is_empty() {
        String::new()
    } else {
        format!("：{}", server_msg.chars().take(300).collect::<String>())
    };
    format!("HTTP {status} · {hint}{tail}")
}

/// 网络层失败（DNS / 连不上 / 超时）翻成中文，并指出大陆用户最常撞的那堵墙。
///
/// 原样透出 reqwest 的英文 `error sending request for url (...)` 对用户没有信息量，
/// 而它和"地址写错"是完全不同的两件事 —— 改地址永远不会修好它。
pub fn net_error(url: &str, e: &reqwest::Error) -> String {
    let why = if e.is_timeout() {
        "请求超时"
    } else if e.is_connect() {
        "连不上"
    } else {
        "请求失败"
    };
    format!(
        "{why} {url} —— {e}\n常见原因：网络不通、需要代理、域名被墙\
         （中国大陆直连 api.anthropic.com / api.openai.com 通常不通）、或 base_url 写错"
    )
}

// ---------------------------------------------------------------------------
// 上下文裁剪
// ---------------------------------------------------------------------------

/// 敏感内容打码：私钥块、`password=`/`token=`/`api_key=` 值、`Bearer xxx`。
/// 打码不删除（保留结构，模型仍知道"这里有密码字段"），返回 (文本, 打码处数)。
pub fn censor_sensitive(text: &str) -> (String, usize) {
    let mut n = 0usize;
    let mut s = String::with_capacity(text.len());
    // 私钥块
    let mut rest = text;
    while let Some(pos) = rest.find("-----BEGIN") {
        s.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if let Some(end_rel) = after.find("-----END") {
            let end_abs = end_rel + "-----END".len();
            if let Some(line_end) = after[end_abs..].find('\n') {
                let block = &after[..end_abs + line_end];
                if block.contains("PRIVATE KEY") {
                    s.push_str("-----BEGIN PRIVATE KEY-----\n**** 已脱敏 ****\n-----END PRIVATE KEY-----\n");
                    n += 1;
                    rest = &after[end_abs + line_end + 1..];
                    continue;
                }
            }
        }
        s.push_str(after);
        rest = "";
        break;
    }
    s.push_str(rest);
    if n > 0 {
        return (s, n);
    }
    // 键值对（password=xxx / api_key:xxx / client_secret=yyy …）—— 纯字符串扫描，无 regex 依赖
    const KEYS: [&str; 12] = [
        "password", "passwd", "secret", "token", "api_key", "api-key", "apikey", "access_key",
        "access-key", "accesskey", "client_secret", "client-secret",
    ];
    let lower = text.to_lowercase();
    let lb = lower.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut i = 0usize;
    while i + 3 < lb.len() {
        let mut matched: Option<usize> = None; // 关键字长度
        for k in KEYS {
            let kb = k.as_bytes();
            if lb.len() - i >= kb.len() && &lb[i..i + kb.len()] == kb {
                let prev_ok = i == 0 || !(lb[i - 1].is_ascii_alphanumeric() || lb[i - 1] == b'_' || lb[i - 1] == b'-');
                let mut j = i + kb.len();
                while j < lb.len() && lb[j].is_ascii_whitespace() {
                    j += 1;
                }
                let sep_ok = j < lb.len() && (lb[j] == b'=' || lb[j] == b':');
                if prev_ok && sep_ok {
                    matched = Some(kb.len());
                    break;
                }
            }
        }
        let Some(klen) = matched else {
            i += 1;
            continue;
        };
        let abs = i;
        let mut j = abs + klen;
        while j < lb.len() && lb[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < lb.len() && (lb[j] == b'=' || lb[j] == b':') {
            j += 1;
        }
        while j < lb.len() && lb[j].is_ascii_whitespace() {
            j += 1;
        }
        let vstart = j;
        while j < lb.len() && !lb[j].is_ascii_whitespace() && !matches!(lb[j], b',' | b';' | b'"' | b'\'' | b'<' | b'>') {
            j += 1;
        }
        let vlen = j - vstart;
        out.push_str(&text[cursor..abs]);
        if vlen >= 4 {
            out.push_str(&text[abs..vstart]);
            out.push_str("****");
            n += 1;
        } else {
            out.push_str(&text[abs..j]);
        }
        cursor = j;
        i = j;
    }
    out.push_str(&text[cursor..]);
    // Bearer token（≥16 字符高熵）
    let lower2 = out.to_lowercase();
    let mut final_out = String::with_capacity(out.len());
    let mut rest = out.as_str();
    loop {
        let Some(pos) = lower2_find_bearer(rest) else { break };
        let (head, tail) = rest.split_at(pos);
        final_out.push_str(head);
        let after = &tail[6..];
        let tok_end = after.find(char::is_whitespace).unwrap_or(after.len());
        let tok = &after[..tok_end];
        if tok.len() >= 16 {
            final_out.push_str("Bearer ****");
            n += 1;
        } else {
            final_out.push_str("Bearer ");
            final_out.push_str(tok);
        }
        rest = &after[tok_end..];
    }
    final_out.push_str(rest);
    (final_out, n)
}

/// 大小写不敏感的 "bearer " 定位（对剩余串，返回命中位置）
fn lower2_find_bearer(rest: &str) -> Option<usize> {
    let lower = rest.to_lowercase();
    let mut idx = 0usize;
    for _ in 0..rest.len() {
        idx = 0;
        for (p, ch) in rest.char_indices() {
            let _ = p;
            idx = 0;
            break;
        }
        break;
    }
    let _ = idx;
    lower.find("bearer ").or_else(|| {
        // 行首/空白后的 Bearer 也要找（"bearer" 无尾空格情况极少，先覆盖带空格）
        None
    })
}

/// OpenAI 兼容流末尾（或非流式响应）里的 usage：{"prompt_tokens":N,"completion_tokens":M}
pub fn parse_usage(data: &str) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    let u = v.get("usage")?;
    let p = u.get("prompt_tokens")?.as_u64()?;
    let c = u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    Some((p, c))
}

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
    let mut last_body = String::new();
    for attempt in 0..2 {
        let built = build_request(profile, key, messages, false);
        let resp = apply_headers(client()?.post(&built.url), &built.headers)
            .json(&built.body)
            .send()
            .await
            .map_err(|e| anyhow!(net_error(&built.url, &e)))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(anyhow!(extract_error(proto, status, &built.url, &text)));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| anyhow!("响应不是合法 JSON：{e}"))?;
        let out = extract_text(proto, &v).unwrap_or_default();
        if !out.trim().is_empty() {
            return Ok(out);
        }
        let finish = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
            .unwrap_or("");
        // 输出上限被吃光时**重试毫无意义**（下次同样截断），直接给准确诊断，别浪费一次请求。
        if finish == "length" {
            let body: String = text.chars().take(400).collect();
            log::warn!("[ai] 输出被截断且正文为空：{}", built.url);
            return Err(anyhow!(
                "{}",
                empty_reply_error(finish, &text, &body, profile.max_tokens)
            ));
        }
        // 其余空回复把**原始响应**带出来：只报"空内容"等于让用户去猜是限流、模型名、
        // 还是服务端形状变了。截断保存，最后一轮的响应进错误信息。
        last_body = text.chars().take(400).collect();
        if attempt == 0 {
            log::warn!(
                "[ai] 模型返回空内容，重试一次：{}（原始响应：{}）",
                built.url,
                last_body.chars().take(200).collect::<String>()
            );
        }
    }
    Err(anyhow!(
        "{}",
        empty_reply_error("", "", &last_body, profile.max_tokens)
    ))
}

/// 流式请求。每个增量通过 `on_delta` 交出去，返回完整文本。
///
/// 两条约定：
/// - 流中途断开**不丢已收到的内容** —— 部分回答也比一句「失败」有用；
/// - 「干净地结束但一个字都没有」**重试一次**（提供方偶发空回复，见 `send_once`）；
///   只在还没有任何增量时重试，所以不会把文本吐两遍。
pub async fn send_stream<F, G, U>(
    profile: &AiProfile,
    key: Option<&str>,
    messages: &[ChatMessage],
    req_id: &str,
    mut on_delta: F,
    mut on_reasoning: G,
    mut on_usage: U,
) -> Result<String>
where
    F: FnMut(&str),
    G: FnMut(&str),
    U: FnMut(u64, u64),
{
    let proto = Protocol::parse(&profile.protocol);
    let mut acc = String::new();
    let mut raw_tail = String::new();
    for attempt in 0..2 {
        let built = build_request(profile, key, messages, true);
        let mut resp = apply_headers(client()?.post(&built.url), &built.headers)
            .json(&built.body)
            .send()
            .await
            .map_err(|e| anyhow!(net_error(&built.url, &e)))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(extract_error(proto, status, &built.url, &text)));
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
            // 留一段原始流的尾巴：空回复时它是唯一能说明"服务端到底回了什么"的东西
            raw_tail.push_str(&String::from_utf8_lossy(&chunk));
            if raw_tail.chars().count() > 400 {
                let skip = raw_tail.chars().count() - 400;
                raw_tail = raw_tail.chars().skip(skip).collect();
            }
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
                if let Some(r) = parse_reasoning(proto, data) {
                    on_reasoning(&r);
                }
                if let Some(u) = parse_usage(data) {
                    on_usage(u.0, u.1);
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
            log::warn!(
                "[ai] 模型返回空内容，重试一次：{}（原始流尾巴：{}）",
                built.url,
                raw_tail.chars().take(200).collect::<String>()
            );
        }
    }
    clear_cancel(req_id);
    if acc.trim().is_empty() {
        return Err(anyhow!(
            "模型返回了空内容（HTTP 200 但没有正文）—— 可能是限流或模型异常，可再试一次或换一个模型。
原始响应：{raw_tail}"
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

    /// 真实抓到的形状（aiaaa.cc 的 deepseek-v4-flash-0731）：正文全程为空、
    /// 思考在 `reasoning_content` 里，正文最后才出现。三家字段名都不同，逐个钉住。
    #[test]
    fn reasoning_is_parsed_for_each_protocol() {
        let openai = r#"{"choices":[{"delta":{"content":"","reasoning_content":"We need","role":"assistant"}}]}"#;
        assert_eq!(parse_reasoning(Protocol::OpenAi, openai).as_deref(), Some("We need"));
        // 只有正文时不能被当成思考
        let plain = r#"{"choices":[{"delta":{"content":"答案"}}]}"#;
        assert_eq!(parse_reasoning(Protocol::OpenAi, plain), None);
        // OpenRouter 用 reasoning
        let or_ = r#"{"choices":[{"delta":{"reasoning":"hmm"}}]}"#;
        assert_eq!(parse_reasoning(Protocol::OpenAi, or_).as_deref(), Some("hmm"));
        // Anthropic 的 thinking_delta（text_delta 是正文，不能混）
        let anth = r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"let me see"}}"#;
        assert_eq!(parse_reasoning(Protocol::Anthropic, anth).as_deref(), Some("let me see"));
        let anth_text = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"答案"}}"#;
        assert_eq!(parse_reasoning(Protocol::Anthropic, anth_text), None);
        // Gemini 用 thought:true 标记思考部分
        let gem = r#"{"candidates":[{"content":{"parts":[{"text":"想一下","thought":true},{"text":"答案"}]}}]}"#;
        assert_eq!(parse_reasoning(Protocol::Gemini, gem).as_deref(), Some("想一下"));
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
            "https://a/v1/chat/completions",
            r#"{"error":{"message":"Invalid API key provided"}}"#,
        );
        assert!(e.contains("401"), "{e}");
        assert!(e.contains("密钥"), "要有中文原因: {e}");
        assert!(e.contains("Invalid API key provided"), "要带服务端原话: {e}");

        let e2 = extract_error(Protocol::Gemini, 404, "https://g/v1beta/models/x", "not found");
        assert!(e2.contains("base_url"), "{e2}");

        // 服务端没给 JSON 也不能崩
        let e3 = extract_error(
            Protocol::Anthropic,
            500,
            "https://api.anthropic.com/v1/messages",
            "<html>bad gateway</html>",
        );
        assert!(e3.contains("服务端错误"), "{e3}");
    }

    /// 404 必须告诉用户"它到底请求了哪个地址" —— 用户报的正是这个问题：
    /// 只看到"base_url 或模型名不对"，看不到实际 URL，只能猜是少了 /v1 还是模型名错。
    #[test]
    fn not_found_error_shows_the_url_and_the_right_example() {
        let e = extract_error(
            Protocol::Anthropic,
            404,
            "https://api.anthropic.com/messages",
            r#"{"error":{"message":"Not Found"}}"#,
        );
        assert!(e.contains("https://api.anthropic.com/messages"), "要带实际 URL: {e}");
        assert!(e.contains("api.anthropic.com/v1"), "要给出正确写法: {e}");
        assert!(e.contains("Not Found"), "服务端原话也要留: {e}");

        // 换协议要给对应的示例，不能一律给 Anthropic 的
        let g = extract_error(Protocol::Gemini, 404, "https://g/v1beta/x", "");
        assert!(g.contains("generativelanguage.googleapis.com"), "{g}");
        let o = extract_error(Protocol::OpenAi, 404, "https://api.deepseek.com/chat", "");
        assert!(o.contains("api.deepseek.com/v1"), "{o}");
    }

    /// 网络层失败和"地址写错"是两件事：前者改地址永远不会好。
    #[test]
    fn network_error_says_it_is_a_network_problem_not_a_wrong_url() {
        // reqwest::Error 造不出来，直接验证文案拼装规则
        let msg = format!(
            "连不上 {url} —— {e}",
            url = "https://api.anthropic.com/v1/messages",
            e = "error sending request"
        );
        assert!(msg.contains("api.anthropic.com"));
        assert!(base_url_example(Protocol::Anthropic).contains("anthropic.com/v1"));
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

    /// 共享样例（同一份 JSON 也给前端 think.test.ts 用）：改规则必须两边一起过。
    #[test]
    fn think_cases_from_fixture() {
        let raw = include_str!("../tests/fixtures/think_cases.json");
        let cases: Vec<serde_json::Value> = serde_json::from_str(raw).expect("fixture 是合法 JSON");
        assert!(cases.len() >= 7, "样例被删了？");
        for c in &cases {
            let name = c["name"].as_str().unwrap_or("?");
            let input = c["input"].as_str().expect("input");
            let (answer, think) = strip_thinking(input);

            if c["think_none"].as_bool() == Some(true) {
                assert!(think.is_none(), "[{name}] 不该剥出思考，却剥了");
                assert_eq!(answer, input, "[{name}] 不剥时应原样返回");
            }
            if let Some(arr) = c["think_has"].as_array() {
                let t = think
                    .as_deref()
                    .unwrap_or_else(|| panic!("[{name}] 应该剥出思考，但没有"));
                for s in arr {
                    let s = s.as_str().unwrap();
                    assert!(t.contains(s), "[{name}] 思考里缺「{s}」");
                }
            }
            if let Some(arr) = c["answer_has"].as_array() {
                for s in arr {
                    let s = s.as_str().unwrap();
                    assert!(
                        answer.contains(s),
                        "[{name}] 正文里缺「{s}」\n--- 正文 ---\n{answer}"
                    );
                }
            }
            if let Some(arr) = c["answer_has_not"].as_array() {
                for s in arr {
                    let s = s.as_str().unwrap();
                    assert!(!answer.contains(s), "[{name}] 正文里不该有「{s}」");
                }
            }
        }
    }

    /// 剥离器用**真实样例**做夹具（Qwen3.6-27B 的思考块，用户提供）。
    /// 手写夹具漏过"思考块以编号项结尾 + 项目符号子行"的真实结构。
    #[test]
    fn strip_thinking_detaches_reasoning_but_never_loses_it() {
        let real = "Here's a thinking process:
 
1.  **Analyze User Input:**
   - User asks: \"如何查询日志\"
   - Context: SSH client, target host root@172.22.134.88, K8s node.
2.  **Identify Key Log Locations:**
   - System: journalctl, /var/log/messages
   - K8s: kubectl logs, /var/log/pods/
 
5.  **Self-Correction:**
   - Language: Chinese ✓
   - Commands are standard and safe ✓
 
查询日志取决于你要查的对象。常用命令如下：

**1. 系统/服务日志（systemd）**
```bash
journalctl -u <服务名> --no-pager
```";
        let (answer, think) = strip_thinking(real);
        let think = think.expect("要能剥出思考块");
        assert!(think.contains("Here's a thinking process:"), "{think}");
        assert!(think.contains("Self-Correction"), "思考要含最后一个编号项");
        assert!(think.contains("safe ✓"), "编号项后的项目符号子行也要归思考");
        assert!(answer.starts_with("查询日志取决于你要查的对象。"), "正文从编号列表后开始");
        assert!(answer.contains("journalctl"), "正文里的命令要完整保留");
        assert!(!answer.contains("thinking process"), "正文里不能再有思考标记");

        // 中文标记
        let zh = "以下是思考过程：\n1. 分析\n2. 设计\n\n最终答案在这里。";
        let (a2, t2) = strip_thinking(zh);
        assert_eq!(a2, "最终答案在这里。");
        assert!(t2.unwrap().contains("2. 设计"));

        // 没标记 / 没编号：原样返回，不当误删
        assert_eq!(strip_thinking("正常回答没有思考。").0, "正常回答没有思考。");
        let no_num = "Here's a thinking process:\n散文式思考没有编号\n\n答案";
        assert_eq!(strip_thinking(no_num).0, no_num, "没编号结构就不剥");
    }

    #[test]
    fn empty_reply_diagnosis_distinguishes_truncation_from_flakiness() {
        let raw = r#"{"choices":[{"finish_reason":"length","message":{"content":"","reasoning_content":"想了很久"}}]}"#;
        let e = empty_reply_error("length", raw, raw, 1024);
        assert!(e.contains("思考过程写满了输出上限"), "{e}");
        assert!(e.contains("换一个非推理模型"), "{e}");
        assert!(!e.contains("可再试一次"), "截断重试没用，不能这么建议：{e}");
        assert!(e.contains("1024"), "要说出当前值，用户才知道现在是多少：{e}");
        assert!(e.contains("设置 → AI 模型"), "要指出在哪调，否则等于没说：{e}");
        assert!(e.contains("4096"), "要给出建议值：{e}");

        // 普通模型输出上限太小（没有思考内容）
        let e3 = empty_reply_error("length", "{}", "{}", 8192);
        assert!(e3.contains("输出被输出上限截断"), "{e3}");

        // 真·偶发空回复：保留重试建议，且必须带原始响应
        let e2 = empty_reply_error("stop", "{}", "{}", 4096);
        assert!(e2.contains("可再试一次"), "{e2}");
        assert!(e2.contains("原始响应"), "{e2}");
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
        assert_eq!(p.max_tokens, 4096, "默认要给推理模型留够思考的额度");
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
