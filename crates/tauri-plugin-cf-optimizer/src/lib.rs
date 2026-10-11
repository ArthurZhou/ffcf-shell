//! tauri-plugin-cf-optimizer
//!
//! Tauri v2 插件：为自有域名自动优选 Cloudflare IP 并加速访问。
//! - 本地 DNS 服务监听 127.0.0.1:10053（UDP+TCP），不修改系统 DNS、不占用 53 端口
//! - 自定义 reqwest Resolver 三级回退：best_ips → 本地 DNS → 系统解析
//! - 测速引擎集成 CloudflareST-Rust（CLI 子进程 + CSV 解析）
//! - 命令：optimizer_get_status / optimizer_refresh / optimizer_resolve /
//!   optimizer_get_config / optimizer_set_config / api_request
//! - 事件：optimizer://status / optimizer://best-ips-updated / optimizer://error

pub mod commands;
pub mod config;
pub mod dns;
pub mod engine;
pub mod events;
pub mod logger;
pub mod resolver;
pub mod speedtest;
pub mod state;

pub use config::OptimizerConfig;
pub use state::OptimizerState;

use tauri::plugin::TauriPlugin;
use tauri::Wry;

/// 构造插件。宿主 app 里 `.plugin(tauri_plugin_cf_optimizer::init())` 即可。
///
/// setup() 中完成规格要求的全部初始化：
/// OptimizerState 构建 → 本地 DNS 服务启动 → 后台测速任务启动（异步，不阻塞启动）。
pub fn init() -> TauriPlugin<Wry> {
    tauri::plugin::Builder::new("cf-optimizer")
        .invoke_handler(tauri::generate_handler![
            commands::optimizer_get_status,
            commands::optimizer_refresh,
            commands::optimizer_resolve,
            commands::optimizer_get_config,
            commands::optimizer_set_config,
            commands::api_request,
        ])
        .setup(|app, _api| {
            engine::boot(app)?;
            Ok(())
        })
        .build()
}
