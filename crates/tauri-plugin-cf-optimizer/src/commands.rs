//! Tauri 命令层。命令在插件内注册（前缀 `plugin:cf-optimizer|xxx`），
//! 宿主 app 又用 generate_handler! 重复注册了同一批函数，
//! 使前端可以按开发规格直接 `invoke('optimizer_get_status')` 裸名调用。

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Instant;

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::config::OptimizerConfig;
use crate::events::StatusDto;
use crate::resolver::query_local_dns;
use crate::state::OptimizerState;
use crate::{engine, events};

/// 当前全量状态（含 best_ips、延迟明细、刷新时间、错误原因）。
///
/// 注：参数类型必须直写 `State<'_, Arc<OptimizerState>>`，不能藏进类型别名 ——
/// `type SharedState = State<'_, ...>` 里的 `'_` 在别名定义位置没有可推导的生命周期
/// (E0106 missing lifetime specifier)。
#[tauri::command]
pub async fn optimizer_get_status(
    state: State<'_, Arc<OptimizerState>>,
) -> Result<StatusDto, String> {
    Ok(StatusDto::from_state(state.inner()).await)
}

/// 手动触发一轮测速刷新（后台执行，立即返回当前状态；
/// 结果通过 optimizer://best-ips-updated / optimizer://status 事件通知前端）。
#[tauri::command]
pub async fn optimizer_refresh(
    state: State<'_, Arc<OptimizerState>>,
) -> Result<StatusDto, String> {
    engine::trigger_refresh(state.inner().clone());
    Ok(StatusDto::from_state(state.inner()).await)
}

/// 解析诊断：展示三级解析链各自的结果，便于验证「实际连接使用优选 IP」。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveReport {
    pub domain: String,
    pub is_target: bool,
    /// 第 1 级：内存优选缓存
    pub best_ips: Vec<String>,
    /// 第 2 级：本地 DNS 127.0.0.1:port（None = 服务未运行或查询失败）
    pub local_dns_ips: Option<Vec<String>>,
    /// 第 3 级：系统解析
    pub system_ips: Vec<String>,
}

#[tauri::command]
pub async fn optimizer_resolve(
    state: State<'_, Arc<OptimizerState>>,
    domain: String,
) -> Result<ResolveReport, String> {
    let domain = domain.trim().trim_end_matches('.').to_lowercase();
    if domain.is_empty() {
        return Err("domain 不能为空".into());
    }
    let cfg = state.config_snapshot().await;
    let best: Vec<String> = state
        .best_ips
        .read()
        .await
        .get(&domain)
        .map(|v| v.iter().map(|ip| ip.to_string()).collect())
        .unwrap_or_default();

    let dns_port = state.status.read().await.dns_port;
    let local = match dns_port {
        Some(port) => query_local_dns(&domain, port)
            .await
            .map(|v| v.iter().map(|ip| ip.to_string()).collect()),
        None => None,
    };

    let d2 = domain.clone();
    let system: Vec<String> = tokio::task::spawn_blocking(move || {
        (d2.as_str(), 0u16)
            .to_socket_addrs()
            .map(|it| {
                let mut seen = Vec::new();
                for sa in it {
                    let s = sa.ip().to_string();
                    if !seen.contains(&s) {
                        seen.push(s);
                    }
                }
                seen
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    Ok(ResolveReport {
        is_target: cfg.is_target(&domain),
        domain,
        best_ips: best,
        local_dns_ips: local,
        system_ips: system,
    })
}

#[tauri::command]
pub async fn optimizer_get_config(
    state: State<'_, Arc<OptimizerState>>,
) -> Result<OptimizerConfig, String> {
    Ok(state.config_snapshot().await)
}

/// 更新配置（含目标域名、刷新间隔等）。校验失败返回 Err；DNS 端口变化自动重启服务。
#[tauri::command]
pub async fn optimizer_set_config(
    state: State<'_, Arc<OptimizerState>>,
    config: OptimizerConfig,
) -> Result<StatusDto, String> {
    engine::apply_config(state.inner(), config).await?;
    // 目标域名可能变化：后台刷新一轮
    engine::trigger_refresh(state.inner().clone());
    events::emit_status(state.inner()).await;
    Ok(StatusDto::from_state(state.inner()).await)
}

// ===================== api_request：HTTP 代理命令 =====================

/// 前端请求体。headers 为可选 map；body 为可选文本（JSON/表单等）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// 响应 + 诊断信息。`remote_addr` 是本次连接**实际使用的对端 IP**，
/// 用于验证「实际连接使用优选 IP」。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    /// 响应体文本；超过 256KB 截断
    pub body: String,
    pub body_truncated: bool,
    pub remote_addr: Option<String>,
    pub elapsed_ms: u128,
}

const BODY_LIMIT: usize = 256 * 1024;

/// HTTP 代理命令：内部 reqwest::Client 已注入 OptimizerResolver。
/// 前端所有目标域名请求必须走此命令，不得直接 fetch（规格模块 5）。
#[tauri::command]
pub async fn api_request(
    state: State<'_, Arc<OptimizerState>>,
    req: ApiRequest,
) -> Result<ApiResponse, String> {
    let client = state
        .http_client
        .get()
        .ok_or("HTTP 客户端未初始化（插件 setup 未完成？）")?;

    let url = reqwest::Url::parse(&req.url).map_err(|e| format!("非法 URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("仅支持 http/https，收到 scheme={}", url.scheme()));
    }
    if let Some(host) = url.host_str() {
        let cfg = state.config_snapshot().await;
        if !cfg.is_target(host) {
            // 不做通用代理：非目标域名仅告警并放行（解析走普通链路，不加速）
            log::warn!("api_request 目标 {host} 不在加速域名列表内，将走普通解析");
        }
    }

    let method = reqwest::Method::from_bytes(req.method.to_uppercase().as_bytes())
        .map_err(|_| format!("非法 HTTP 方法: {}", req.method))?;

    let mut rb = client.request(method, url);
    for (k, v) in &req.headers {
        match (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            (Ok(name), Ok(val)) => rb = rb.header(name, val),
            _ => log::warn!("忽略非法 header: {k}"),
        }
    }
    if let Some(b) = req.body {
        rb = rb.body(b);
    }

    let t0 = Instant::now();
    let resp = rb.send().await.map_err(|e| {
        // 把底层解析/连接错误转成人话，保留 source 链
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg.push_str(&format!(" <- {s}"));
            src = s.source();
        }
        msg
    })?;
    let elapsed_ms = t0.elapsed().as_millis();
    let remote_addr = resp.remote_addr().map(|a| a.to_string());
    let status = resp.status().as_u16();
    let headers: HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();

    let raw = resp.text().await.unwrap_or_default();
    let (body, truncated) = truncate_utf8(&raw, BODY_LIMIT);

    log::info!(
        "api_request {} {} -> {status} remote={remote_addr:?} {elapsed_ms}ms",
        req.method.to_uppercase(),
        req.url
    );
    Ok(ApiResponse {
        status,
        headers,
        body,
        body_truncated: truncated,
        remote_addr,
        elapsed_ms,
    })
}

/// UTF-8 安全的截断（不在多字节字符中间切断）。
fn truncate_utf8(s: &str, limit: usize) -> (String, bool) {
    if s.len() <= limit {
        return (s.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…(已截断)", &s[..end]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "中文内容测试".repeat(100);
        let (out, t) = truncate_utf8(&s, 10);
        assert!(t);
        assert!(out.ends_with("…(已截断)"));
        // 前缀必须是合法 UTF-8（能 format 就说明没切断）
        assert!(out.starts_with("中"));
        let (full, t2) = truncate_utf8("short", 100);
        assert!(!t2);
        assert_eq!(full, "short");
    }


}
