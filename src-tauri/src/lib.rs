// ffcf-shell 宿主入口：注册 cf-optimizer 插件，并在 app 级重复注册命令，
// 使前端可按开发规格直接 invoke('optimizer_get_status') 等裸命令名调用。

use tauri_plugin_cf_optimizer::commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_cf_optimizer::init())
        // app 级命令注册：与插件共享同一个 State<Arc<OptimizerState>>，
        // 前端裸名 invoke 与插件前缀 invoke 都可用。
        .invoke_handler(tauri::generate_handler![
            commands::optimizer_get_status,
            commands::optimizer_refresh,
            commands::optimizer_resolve,
            commands::optimizer_get_config,
            commands::optimizer_set_config,
            commands::api_request,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ffcf-shell");
}
