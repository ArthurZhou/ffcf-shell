//! 事件定义与发射：optimizer://status、optimizer://best-ips-updated、optimizer://error。
//! 同时定义 StatusDto —— 前端 invoke('optimizer_get_status') 与 status 事件共用的负载结构。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::Emitter;

use crate::state::{DomainSpeedDto, OptimizerState};

pub const EV_STATUS: &str = "optimizer://status";
pub const EV_BEST_IPS_UPDATED: &str = "optimizer://best-ips-updated";
pub const EV_ERROR: &str = "optimizer://error";

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 状态 DTO：当前 IP、延迟、刷新时间、错误原因全部可观测（验收项 8）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub dns_running: bool,
    pub dns_port: Option<u16>,
    pub refreshing: bool,
    /// 最近一次成功测速时间（Unix 毫秒）；None = 尚未测速成功
    pub last_refresh_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub from_cache: bool,
    pub target_domains: Vec<String>,
    /// 域名 → 优选 IP（字符串形式，按优先级排序）
    pub best_ips: HashMap<String, Vec<String>>,
    /// 最近一次测速明细（含每个 IP 的延迟/丢包/下载速度/数据中心）
    pub speeds: Vec<DomainSpeedDto>,
    pub refresh_interval_secs: u64,
    pub top_n: usize,
    pub speedtest_bin: Option<String>,
}

impl StatusDto {
    /// 从状态快照生成 DTO（命令与事件共用，保证前端拿到的结构一致）。
    pub async fn from_state(state: &OptimizerState) -> Self {
        let cfg = state.config.read().await;
        let status = state.status.read().await;
        let best = state.best_ips.read().await;
        Self {
            dns_running: status.dns_running,
            dns_port: status.dns_port,
            refreshing: status.refreshing,
            last_refresh_unix_ms: status.last_refresh_at.map(unix_ms),
            last_error: status.last_error.clone(),
            from_cache: status.from_cache,
            target_domains: cfg.target_domains.clone(),
            best_ips: best
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().map(|ip| ip.to_string()).collect()))
                .collect(),
            speeds: status.speeds.clone(),
            refresh_interval_secs: cfg.refresh_interval_secs,
            top_n: cfg.top_n,
            speedtest_bin: state
                .speedtest_candidates
                .read()
                .await
                .first()
                .map(|p| p.display().to_string()),
        }
    }
}

/// 发射 optimizer://status 事件（全量状态）。
pub async fn emit_status(state: &Arc<OptimizerState>) {
    if let Some(app) = state.app.get() {
        let dto = StatusDto::from_state(state).await;
        if let Err(e) = app.emit(EV_STATUS, dto) {
            log::warn!("emit {EV_STATUS} failed: {e}");
        }
    }
}

/// 发射 optimizer://best-ips-updated 事件（仅 IP 映射 + 时间戳，负载小）。
pub async fn emit_best_ips(state: &Arc<OptimizerState>) {
    if let Some(app) = state.app.get() {
        let best = state.best_ips.read().await;
        let payload: HashMap<String, Vec<String>> = best
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().map(|ip| ip.to_string()).collect()))
            .collect();
        // Clone 是 tauri::Emitter::emit 的硬约束（payload 要复制给多个监听目标）
        #[derive(Serialize, Clone)]
        #[serde(rename_all = "camelCase")]
        struct P {
            best_ips: HashMap<String, Vec<String>>,
            updated_at_unix_ms: u64,
        }
        let p = P {
            best_ips: payload,
            updated_at_unix_ms: unix_ms(SystemTime::now()),
        };
        if let Err(e) = app.emit(EV_BEST_IPS_UPDATED, p) {
            log::warn!("emit {EV_BEST_IPS_UPDATED} failed: {e}");
        }
    }
}

/// 发射 optimizer://error 事件，并同步写入状态快照的 last_error。
pub async fn emit_error(state: &Arc<OptimizerState>, context: &str, message: &str) {
    log::error!("[{context}] {message}");
    {
        let mut st = state.status.write().await;
        st.last_error = Some(format!("{context}: {message}"));
    }
    if let Some(app) = state.app.get() {
        #[derive(Serialize, Clone)]
        #[serde(rename_all = "camelCase")]
        struct E {
            context: String,
            message: String,
            at_unix_ms: u64,
        }
        let e = E {
            context: context.to_string(),
            message: message.to_string(),
            at_unix_ms: unix_ms(SystemTime::now()),
        };
        let _ = app.emit(EV_ERROR, e);
    }
}
