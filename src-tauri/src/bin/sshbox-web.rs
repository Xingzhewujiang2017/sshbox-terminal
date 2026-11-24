//! `sshbox-web` —— 浏览器当界面的验证版入口。
//!
//! 用法：`sshbox-web [端口]`（默认 8787），`SSHBOX_DIST=<dist 目录>`。
//! 构建：`cargo build --release --features web --bin sshbox-web`

fn main() -> anyhow::Result<()> {
    let port = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(8787);

    // 这个验证件不需要 tauri 的运行时，自己起一个 tokio 多线程 runtime。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(sshbox_lib::web::run(port))
}
