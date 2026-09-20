//! AI 的 Tauri 命令层：profile 增删改、密钥存取、测试连接、流式对话。
//!
//! 密钥一律走系统凭据管理器（`store::set_secret("ai:<id>", …)`），
//! 只在前端问「有没有」时回一个布尔值 —— 明文永不回传、不落 settings.json。

use tauri::{AppHandle, Emitter};

use crate::ai::{self, AiProfile, AiSettings, ChatMessage, Protocol};
use crate::store;

/// 前端要的 profile 视图：带上 `has_key`，但不带密钥本身。
fn view(mut p: AiProfile) -> AiProfile {
    p.has_key = store::get_secret(&ai::key_entry(&p.id)).is_ok();
    p
}

fn active_id(s: &AiSettings) -> Option<String> {
    let id = s.active_profile_id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// 取当前生效的 profile（不存在就报一句人话，而不是 panic）。
fn active_profile() -> Result<AiProfile, String> {
    let s = store::load_settings().ai;
    let Some(id) = active_id(&s) else {
        return Err("还没有选择模型，去 设置 → AI 模型 里挑一个".into());
    };
    s.profiles
        .into_iter()
        .find(|p| p.id == id)
        .map(view)
        .ok_or_else(|| format!("当前模型 {id} 已经不存在了，去设置里重新选一个"))
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn ai_settings() -> AiSettings {
    let mut s = store::load_settings().ai;
    s.profiles = s.profiles.into_iter().map(view).collect();
    s
}

#[tauri::command]
pub fn ai_profile_save(profile: AiProfile, key: Option<String>) -> Result<AiSettings, String> {
    let mut settings = store::load_settings();
    let mut p = profile;
    if p.id.trim().is_empty() {
        // 先补 id 再写密钥 —— 顺序反了密钥就落到空 key 上（v0.4.1 踩过同一个坑）。
        p.id = uuid::Uuid::new_v4().to_string();
    }
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        store::set_secret(&ai::key_entry(&p.id), k.trim()).map_err(|e| format!("{e:#}"))?;
    }
    match settings.ai.profiles.iter_mut().find(|x| x.id == p.id) {
        Some(existing) => *existing = p.clone(),
        None => settings.ai.profiles.push(p.clone()),
    }
    // 第一个配置好的 profile 自动生效，省得用户还要多点一次
    if active_id(&settings.ai).is_none() {
        settings.ai.active_profile_id = p.id.clone();
    }
    store::save_settings(&settings).map_err(|e| format!("{e:#}"))?;
    Ok(ai_settings())
}

#[tauri::command]
pub fn ai_profile_delete(id: String) -> Result<AiSettings, String> {
    let mut settings = store::load_settings();
    settings.ai.profiles.retain(|p| p.id != id);
    let _ = store::delete_secret(&ai::key_entry(&id));
    if active_id(&settings.ai).as_deref() == Some(id.as_str()) {
        // 删掉正在用的那个就**留空**，而不是自动挑下一个：
        // 自动挑等于把用户没验证过的接入点设成生效项，点一下 AI 才发现连不上。
        // 留空时 AI 入口会禁用并提示"去设置里挑一个"。
        settings.ai.active_profile_id = String::new();
    }
    store::save_settings(&settings).map_err(|e| format!("{e:#}"))?;
    Ok(ai_settings())
}

#[tauri::command]
pub fn ai_set_active(id: String) -> Result<AiSettings, String> {
    let mut settings = store::load_settings();
    settings.ai.active_profile_id = id;
    store::save_settings(&settings).map_err(|e| format!("{e:#}"))?;
    Ok(ai_settings())
}

#[tauri::command]
pub fn ai_set_report_summary(on: bool) -> Result<(), String> {
    let mut settings = store::load_settings();
    settings.ai.report_ai_summary = on;
    store::save_settings(&settings).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn ai_key_has(id: String) -> bool {
    store::get_secret(&ai::key_entry(&id)).is_ok()
}

#[tauri::command]
pub fn ai_key_delete(id: String) -> Result<(), String> {
    store::delete_secret(&ai::key_entry(&id)).map_err(|e| format!("{e:#}"))
}

/// 恢复出厂预设（不会动已填的密钥）。
#[tauri::command]
pub fn ai_presets() -> Vec<AiProfile> {
    ai::presets().into_iter().map(view).collect()
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// 测试连接：发一条极短的非流式请求，回报耗时与模型回显。
#[tauri::command]
pub async fn ai_test(id: Option<String>) -> Result<String, String> {
    let p = match id.filter(|s| !s.trim().is_empty()) {
        Some(id) => {
            let s = store::load_settings().ai;
            let found = s
                .profiles
                .into_iter()
                .find(|x| x.id == id)
                .map(view)
                .ok_or_else(|| format!("没有这个模型配置：{id}"))?;
            found
        }
        None => active_profile()?,
    };
    let key = store::get_secret(&ai::key_entry(&p.id)).ok();
    let msgs = vec![ChatMessage::user("ping")];
    let started = std::time::Instant::now();
    let reply = ai::send_once(&p, key.as_deref(), &msgs)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let ms = started.elapsed().as_millis();
    let echo: String = reply.trim().chars().take(60).collect();
    Ok(format!(
        "{} · {} · {}ms · 回显「{}」",
        p.name,
        p.model,
        ms,
        if echo.is_empty() { "（空）" } else { &echo }
    ))
}

/// 流式对话：增量通过 `ssh://ai/delta` 事件推给前端，结束时 `ssh://ai/done`。
#[tauri::command]
pub async fn ai_chat(
    app: AppHandle,
    req_id: String,
    kind: String,
    messages: Vec<ChatMessage>,
    profile_id: Option<String>,
) -> Result<String, String> {
    let p = match profile_id.filter(|s| !s.trim().is_empty()) {
        Some(id) => {
            let s = store::load_settings().ai;
            s.profiles
                .into_iter()
                .find(|x| x.id == id)
                .map(view)
                .ok_or_else(|| format!("没有这个模型配置：{id}"))?
        }
        None => active_profile()?,
    };
    let key = store::get_secret(&ai::key_entry(&p.id)).ok();
    let rid = req_id.clone();
    let app2 = app.clone();
    // 每次 AI 请求都留一行日志：模型、地址、耗时、结果。
    // 没有这行，"答案怎么是空的"只能靠猜——是密钥、模型名、超时还是模型真没说话，
    // 全靠这里的 endpoint + 耗时 + 错误文本区分。
    let t0 = std::time::Instant::now();
    let has_key = key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false);
    log::info!(
        "[ai] 请求 req={} kind={} profile={} 协议={} model={} 端点={} 密钥={} 消息={} 条",
        req_id,
        kind,
        p.name,
        p.protocol,
        p.model,
        ai::endpoint(&p),
        if has_key { "有" } else { "无" },
        messages.len()
    );
    let result = ai::send_stream(&p, key.as_deref(), &messages, &req_id, move |text| {
        let _ = app2.emit(
            "ssh://ai/delta",
            serde_json::json!({ "req_id": rid, "text": text }),
        );
    })
    .await;
    match result {
        Ok(full) => {
            log::info!(
                "[ai] 完成 {}ms，回答 {} 字",
                t0.elapsed().as_millis(),
                full.chars().count()
            );
            let _ = app.emit(
                "ssh://ai/done",
                serde_json::json!({ "req_id": req_id, "text": full, "profile": p.name, "model": p.model }),
            );
            Ok(full)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::warn!(
                "[ai] 失败 {}ms：{}",
                t0.elapsed().as_millis(),
                msg.chars().take(300).collect::<String>()
            );
            let _ = app.emit(
                "ssh://ai/error",
                serde_json::json!({ "req_id": req_id, "message": msg }),
            );
            Err(msg)
        }
    }
}

#[tauri::command]
pub fn ai_cancel(req_id: String) {
    ai::cancel(&req_id);
}

/// 给前端展示用：这个 profile 走的是哪套协议（设置页要显示协议名）。
#[tauri::command]
pub fn ai_protocol_label(protocol: String) -> String {
    match Protocol::parse(&protocol) {
        Protocol::OpenAi => "OpenAI 兼容".into(),
        Protocol::Anthropic => "Anthropic".into(),
        Protocol::Gemini => "Google Gemini".into(),
    }
}

// ---------------------------------------------------------------------------
// 上下文组装（决定回答质量的就是这一步）
// ---------------------------------------------------------------------------

/// 终端输出最多带多少字符进上下文。太短 AI 看不到报错，太长既贵又容易被无关日志带偏。
pub const TAIL_BUDGET: usize = 6000;

/// 主机背景，让 AI 知道自己在看什么机器。
pub fn host_brief(host: &str, port: u16, username: &str, os: &str, hostname: &str) -> String {
    format!("目标主机：{username}@{host}:{port}，系统 {os}，主机名 {hostname}。")
}

/// 「解释这段」：贴选中的内容 + 终端尾部做背景。
pub fn explain_messages(brief: &str, selection: &str, tail: &str) -> Vec<ChatMessage> {
    let system = format!(
        "你是一名 Linux 运维工程师，帮用户看懂终端里的报错或输出。要求：\n\
         1) 先用一句话说这段内容在表达什么；\n\
         2) 如果有报错，指出**最可能的 2-3 个原因**，按可能性排序；\n\
         3) 每个原因给一条可直接执行的排查命令；\n\
         4) 只依据给到的内容推断，信息不足就直说还需要看什么；\n\
         5) 中文回答，简短，不要复述原文。\n\n{brief}"
    );
    let mut user = String::new();
    if !selection.trim().is_empty() {
        user.push_str("需要解释的内容：\n```\n");
        user.push_str(selection.trim());
        user.push_str("\n```\n\n");
    }
    if !tail.trim().is_empty() {
        user.push_str("终端最近输出（背景，可能包含上文）：\n```\n");
        user.push_str(&crate::ai::truncate_middle(tail, TAIL_BUDGET));
        user.push_str("\n```\n");
    }
    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// 「自然语言 → 命令」：只要一条命令，不要解释（解释由前端自己给提示）。
pub fn command_messages(brief: &str, ask: &str, tail: &str) -> Vec<ChatMessage> {
    let system = format!(
        "你是一名 Linux 运维工程师。用户会用自然语言描述需求，你要给出一条**可直接执行**的命令。要求：\n\
         1) 只输出命令本身，一行，不要 Markdown 代码块、不要引号、不要任何解释；\n\
         2) 优先用常见且安全的写法（只读、不改系统）；\n\
         3) 需要多个命令时用 && 连接；\n\
         4) 无法安全完成时输出以 # 开头的说明。\n\n{brief}"
    );
    let mut user = String::new();
    if !tail.trim().is_empty() {
        user.push_str("当前终端上下文（参考，别直接复述）：\n```\n");
        user.push_str(&crate::ai::truncate_middle(tail, TAIL_BUDGET / 2));
        user.push_str("\n```\n\n");
    }
    user.push_str(&format!("需求：{}", ask.trim()));
    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// 侧栏对话：多轮，系统提示带上主机背景**和终端最近的输出**。
///
/// 三条约定，每条都对应一个踩过的坑：
/// 1. `history` 是**之前的**轮次，当前提问由 `ask` 单独传入并追加在最后 ——
///    少了它模型看不到问题（只看到终端输出），只能靠猜；
/// 2. 消息列表必须**以 user 结尾**：Ollama 的 OpenAI 兼容层遇到末尾是
///    assistant 的消息会直接返回空字符串（`finish_reason: stop`、0 字），
///    不报错也不说话；
/// 3. 终端尾部放系统消息里，用户看到的对话历史保持原样。
pub fn chat_messages(
    brief: &str,
    history: Vec<ChatMessage>,
    ask: &str,
    tail: &str,
) -> Vec<ChatMessage> {
    let mut system = format!(
        "你是嵌在 SSH 客户端里的运维助手，帮用户分析和操作这台服务器。\
         回答用中文、简短、给可执行的命令；不确定就说不确定，不要编造输出。\n\n{brief}"
    );
    if !tail.trim().is_empty() {
        system.push_str("\n\n当前终端最近的输出（用户可能就着这段提问）：\n");
        system.push_str(&crate::ai::truncate_middle(tail, TAIL_BUDGET));
    }
    let mut out = vec![ChatMessage::system(system)];
    out.extend(history);
    if !ask.trim().is_empty() {
        out.push(ChatMessage::user(ask.trim()));
    }
    out
}

/// 统一入口：按 `kind` 组装上下文并流式回答。
#[tauri::command]
pub async fn ai_ask(
    app: AppHandle,
    req_id: String,
    kind: String,
    sid: Option<String>,
    selection: Option<String>,
    ask: Option<String>,
    tail: Option<String>,
    history: Option<Vec<ChatMessage>>,
    profile_id: Option<String>,
    state: tauri::State<'_, crate::ssh::SessionManager>,
) -> Result<String, String> {
    // 主机背景来自会话信息 + 静态信息缓存，前端不用重复传
    let brief = {
        let sessions = state.sessions.lock().await;
        match sid.as_deref().and_then(|s| sessions.get(s)) {
            Some(sess) => {
                let info = &sess.info;
                let cached = crate::monitor::cached_static(&info.sid);
                host_brief(
                    &info.host,
                    info.port,
                    &info.username,
                    cached
                        .as_ref()
                        .map(|c| c.os_pretty.clone())
                        .unwrap_or_else(|| "未知".into())
                        .as_str(),
                    cached
                        .as_ref()
                        .map(|c| c.hostname.clone())
                        .unwrap_or_default()
                        .as_str(),
                )
            }
            None => String::new(),
        }
    };

    let sel = selection.unwrap_or_default();
    let ask = ask.unwrap_or_default();
    let tail = tail.unwrap_or_default();
    let messages = match kind.as_str() {
        "explain" => explain_messages(&brief, &sel, &tail),
        "command" => command_messages(&brief, &ask, &tail),
        _ => chat_messages(&brief, history.unwrap_or_default(), &ask, &tail),
    };
    ai_chat(app, req_id, kind, messages, profile_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> String {
        host_brief("127.0.0.1", 22, "root", "Ubuntu 22.04", "r9000p")
    }

    #[test]
    fn chat_carries_terminal_tail_so_you_can_point_at_it() {
        let hist = vec![ChatMessage::user("上一问")];
        let m = chat_messages(
            &brief(),
            hist,
            "刚才那条命令为什么失败",
            "cat: /etc/nope: No such file or directory",
        );
        assert_eq!(m.len(), 3, "系统消息 + 一轮历史 + 当前提问");
        assert_eq!(m[0].role, "system");
        assert!(
            m[0].content.contains("No such file"),
            "终端尾部要进系统消息：{}",
            m[0].content
        );
        assert!(m[0].content.contains("当前终端最近的输出"));
    }

    /// 这条钉住两个真实 bug：当前提问必须进请求体；列表必须以 user 结尾
    /// （Ollama 遇到末尾是 assistant 会返回 0 字且不报错）。
    #[test]
    fn chat_ends_with_the_current_question() {
        let hist = vec![
            ChatMessage::user("第一问"),
            ChatMessage {
                role: "assistant".into(),
                content: "第一答".into(),
            },
        ];
        let m = chat_messages(&brief(), hist, "第二问", "");
        assert_eq!(m.len(), 4, "system + 第一问 + 第一答 + 第二问");
        let last = m.last().unwrap();
        assert_eq!(last.role, "user", "必须以 user 结尾：{m:?}");
        assert_eq!(last.content, "第二问");
        // 历史保持原样
        assert_eq!(m[1].content, "第一问");
        assert_eq!(m[2].content, "第一答");
    }

    #[test]
    fn chat_without_tail_has_no_empty_section() {
        let m = chat_messages(&brief(), vec![ChatMessage::user("你好")], "在吗", "   ");
        assert!(
            !m[0].content.contains("当前终端最近的输出"),
            "空尾部不该留一个空标题"
        );
    }

    #[test]
    fn host_brief_carries_identity() {
        let b = brief();
        assert!(b.contains("root@127.0.0.1:22"), "{b}");
        assert!(b.contains("Ubuntu 22.04"));
        assert!(b.contains("r9000p"));
    }

    #[test]
    fn explain_puts_selection_and_tail_in_the_user_turn() {
        let msgs = explain_messages(&brief(), "Permission denied (publickey)", "ssh root@host\nPermission denied");
        assert_eq!(msgs.len(), 2, "一条 system + 一条 user");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[0].content.contains("排查命令"), "系统提示要要求给排查命令");
        assert!(msgs[1].content.contains("Permission denied (publickey)"));
        assert!(msgs[1].content.contains("终端最近输出"));
    }

    #[test]
    fn explain_works_without_a_selection() {
        // 右键菜单在没选中时也应该能问（前端只传 tail）
        let msgs = explain_messages(&brief(), "", "only tail here");
        assert_eq!(msgs.len(), 2);
        assert!(!msgs[1].content.contains("需要解释的内容"), "没选中就别写这一节");
        assert!(msgs[1].content.contains("only tail here"));
    }

    #[test]
    fn long_tail_is_truncated_not_dropped() {
        let long = "x".repeat(50_000);
        let msgs = explain_messages(&brief(), "sel", &long);
        let body = &msgs[1].content;
        assert!(body.len() < 20_000, "必须裁掉，实际 {}", body.len());
        assert!(body.contains("省略"), "要留一句说明省略了多少");
        assert!(body.contains('x'), "裁完还得有内容");
    }

    #[test]
    fn command_prompt_demands_a_bare_command() {
        let msgs = command_messages(&brief(), "找出占用 80 端口的进程", "");
        assert!(msgs[0].content.contains("只输出命令本身"));
        assert!(msgs[0].content.contains("不要 Markdown"));
        assert!(msgs[1].content.contains("需求：找出占用 80 端口的进程"));
    }

    #[test]
    fn chat_keeps_history_order_after_the_system_prompt() {
        let history = vec![
            ChatMessage::user("第一问"),
            ChatMessage {
                role: "assistant".into(),
                content: "第一答".into(),
            },
            ChatMessage::user("第二问"),
        ];
        let msgs = chat_messages(&brief(), history, "", "");
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].content, "第一问");
        assert_eq!(msgs[2].content, "第一答");
        assert_eq!(msgs[3].content, "第二问");
        assert!(msgs[0].content.contains("r9000p"), "系统提示要带主机背景");
    }
}
