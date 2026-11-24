//! 事件出口抽象。
//!
//! 为什么要有这层：桌面版（Tauri 窗口）和 web/Electron 版（本地服务 + 浏览器）
//! 要跑**同一套业务代码**。业务层（ssh/monitor/sftp/ai/report）原来直接
//! `app.emit(...)`，等于把 AppHandle 焊死在里面，第二个壳就复用不了。
//!
//! 现在业务层统一调 `emit::emit()`，壳在启动时注入一个 sink：
//! - 桌面壳：转给 `tauri::AppHandle::emit`（见 [`tauri_sink`]）
//! - web 壳：转给 WebSocket 广播通道
//!
//! 进程级单例是刻意的：一个进程只会有一个壳。

use serde_json::Value;
use std::sync::OnceLock;

type Sink = Box<dyn Fn(&str, Value) + Send + Sync + 'static>;

static SINK: OnceLock<Sink> = OnceLock::new();

/// 壳启动时调用一次；重复调用会被忽略（先注册的赢）。
pub fn set_sink(f: impl Fn(&str, Value) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(f));
}

/// 业务层发事件统一走这里。没注册 sink 时静默丢弃（单测里就是这个状态）。
///
/// 泛型是为了兼容两种调用：`emit("x", json!({...}))` 和
/// `emit("x", some_struct)` —— Tauri 的 `emit` 本来就是吃 `Serialize`。
pub fn emit<T: serde::Serialize>(event: &str, payload: T) {
    let Some(sink) = SINK.get() else { return };
    match serde_json::to_value(payload) {
        Ok(v) => sink(event, v),
        Err(e) => log::warn!("[emit] {} 序列化失败: {}", event, e),
    }
}

/// 桌面壳的适配器：把事件转发给窗口。
pub fn tauri_sink(app: tauri::AppHandle) -> impl Fn(&str, Value) + Send + Sync + 'static {
    move |event, payload| {
        use tauri::Emitter;
        let _ = app.emit(event, payload);
    }
}
