//! 引擎层：状态装配（boot）、DNS 服务生命周期、周期测速循环、配置/缓存持久化、回退链。

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tauri::{AppHandle, Manager};

use crate::config::OptimizerConfig;
use crate::resolver::OptimizerResolver;
use crate::speedtest;
use crate::state::{DomainSpeedDto, IpSpeedDto, OptimizerState};
use crate::{dns, events, logger};

const CONFIG_FILE: &str = "optimizer-config.json";
const CACHE_FILE: &str = "best-ips.json";

// ===================== 持久化 =====================

/// 从 config_dir 加载配置；文件不存在/损坏时返回默认配置（0 配置启动）。
pub fn load_config(dir: &Path) -> OptimizerConfig {
    let p = dir.join(CONFIG_FILE);
    match std::fs::read_to_string(&p) {
        Ok(text) => serde_json::from_str::<OptimizerConfig>(&text).unwrap_or_else(|e| {
            log::warn!("配置文件损坏({p:?}): {e}，使用默认配置");
            OptimizerConfig::default()
        }),
        Err(_) => OptimizerConfig::default(),
    }
}

pub fn save_config(dir: &Path, cfg: &OptimizerConfig) {
    let p = dir.join(CONFIG_FILE);
    match serde_json::to_string_pretty(cfg) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&p, text) {
                log::error!("写配置失败({p:?}): {e}");
            }
        }
        Err(e) => log::error!("序列化配置失败: {e}"),
    }
}

/// 加载上次会话的优选结果缓存。用途：
/// 1. 冷启动 5 秒内可正常请求（验收项）——不必等首轮测速完成；
/// 2. 测速失败时的「回退缓存」（验收项）。
pub fn load_best_ips_cache(dir: &Path) -> HashMap<String, Vec<IpAddr>> {
    let p = dir.join(CACHE_FILE);
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_best_ips_cache(dir: &Path, map: &HashMap<String, Vec<IpAddr>>) {
    let p = dir.join(CACHE_FILE);
    if let Ok(text) = serde_json::to_string_pretty(map) {
        if let Err(e) = std::fs::write(&p, text) {
            log::error!("写 IP 缓存失败({p:?}): {e}");
        }
    }
}

/// CloudflareST-Rust 二进制候选路径（按优先级）。
/// 打包后位于 resource_dir/resources/；开发期可用项目 bin/ 或 PATH。
pub fn binary_candidates(app: &AppHandle) -> Vec<PathBuf> {
    let name = speedtest::bin_name();
    let mut v = Vec::new();
    if let Ok(rd) = app.path().resource_dir() {
        v.push(rd.join("resources").join(name));
        v.push(rd.join(name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.join(name));
        }
    }
    v.push(PathBuf::from("bin").join(name));
    v.push(PathBuf::from(name)); // PATH 兜底
    v
}

// ===================== 启动装配 =====================

/// 插件 setup 阶段调用：初始化日志/配置/状态/HTTP 客户端，注册 state，
/// 并把「启动 DNS + 周期测速」丢到异步运行时。
///
/// 注意：所有耗时/网络操作都在 spawn 的任务里，setup 本身只做毫秒级的本地 IO，
/// 保证「应用启动 5 秒内可正常请求」。
pub fn boot(app: &AppHandle) -> Result<Arc<OptimizerState>, Box<dyn std::error::Error>> {
    // 日志：app_log_dir/ffcf-shell.log + stderr
    let log_dir = app.path().app_log_dir()?;
    logger::init(Some(&log_dir.join("ffcf-shell.log")));
    log::info!("ffcf-shell cf-optimizer 插件启动");

    let config_dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&config_dir)?;

    let mut cfg = load_config(&config_dir);
    if let Err(e) = cfg.validate() {
        log::error!("配置非法，回退默认配置: {e}");
        cfg = OptimizerConfig::default();
        let _ = cfg.validate();
    }

    // 同步预载 IP 缓存 + 定位测速二进制候选：构造期注入（非 blocking_write，避免 setup 期 panic）。
    // 启动瞬间 Resolver 第 1 级即可命中磁盘缓存，满足「启动 5 秒内可请求」。
    let cache = load_best_ips_cache(&config_dir);
    let candidates = binary_candidates(app);
    let state = Arc::new(OptimizerState::with_cache(
        cfg,
        config_dir.clone(),
        cache,
        candidates,
    ));
    let _ = state.app.set(app.clone());

    // 构造注入 OptimizerResolver 的 reqwest 客户端（api_request 专用）
    let resolver = Arc::new(OptimizerResolver::new(state.clone()));
    let client = reqwest::Client::builder()
        .dns_resolver(resolver)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let _ = state.http_client.set(client);

    app.manage(state.clone());

    // 异步：启动本地 DNS + 首轮测速 + 周期刷新
    let st = state.clone();
    tauri::async_runtime::spawn(async move {
        start_services(st).await;
    });

    Ok(state)
}

/// 启动本地 DNS 服务与周期测速循环。DNS 绑定失败不 panic：
/// 发 optimizer://error，Resolver 自动走系统 DNS 回退。
pub async fn start_services(state: Arc<OptimizerState>) {
    let port = state.config.read().await.dns_port;
    match dns::spawn_dns_server(state.clone(), port) {
        Ok(handle) => {
            *state.dns_server.write().await = Some(handle);
            log::info!("DNS 服务句柄已登记 (port={port})");
        }
        Err(e) => {
            events::emit_error(&state, "dns", &e).await;
        }
    }
    events::emit_status(&state).await;

    // 首轮测速延迟 2s：让窗口/前端先就绪，也避免和启动 IO 抢资源
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        refresh_once(&state).await;
        loop {
            let interval = state.config.read().await.refresh_interval_secs;
            if state.shutdown.load(Ordering::SeqCst) {
                log::info!("刷新循环收到关停信号，退出");
                break;
            }
            if interval == 0 {
                // 自动刷新被禁用：轮询关停标志即可
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            refresh_once(&state).await;
        }
    });
}

// ===================== 测速刷新 =====================

/// 触发一次后台刷新（手动刷新命令使用）。已有任务在跑则忽略（防重入）。
/// 返回 true = 成功触发。
pub fn trigger_refresh(state: Arc<OptimizerState>) -> bool {
    if state.refresh_inflight.load(Ordering::SeqCst) {
        log::info!("刷新任务已在进行中，忽略本次触发");
        return false;
    }
    tauri::async_runtime::spawn(async move {
        refresh_once(&state).await;
    });
    true
}

/// 一轮完整测速：对每个目标域名调用 CloudflareST-Rust，更新 best_ips、
/// 持久化缓存、发事件。
///
/// 回退链（验收项 5）：
/// - 某域名测速失败 → 保留其旧 best_ips（缓存）；
/// - 无旧缓存 → 写入 config.default_ips；
/// - default_ips 也为空 → 不写入，Resolver 自然回退系统 DNS。
pub async fn refresh_once(state: &Arc<OptimizerState>) {
    // 防重入
    if state
        .refresh_inflight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        log::info!("refresh_once: 已有任务在跑，跳过");
        return;
    }
    {
        let mut st = state.status.write().await;
        st.refreshing = true;
    }
    events::emit_status(state).await;

    let started = Instant::now();
    let cfg = state.config_snapshot().await;
    let candidates = state.speedtest_candidates.read().await.clone();
    let bin = speedtest::locate_binary(&candidates, cfg.speedtest_bin.as_deref());
    let work_dir = state.config_dir.join("speedtest-tmp");

    let mut all_speeds: Vec<DomainSpeedDto> = Vec::new();
    let mut updated_domains: Vec<String> = Vec::new();
    let mut failures = 0usize;

    for domain in &cfg.target_domains {
        let result = match &bin {
            Some(b) => speedtest::run_for_domain(&cfg, b, domain, &work_dir).await,
            None => Err(format!(
                "未找到 CloudflareST-Rust 二进制（候选: {candidates:?}）"
            )),
        };
        match result {
            Ok(list) => {
                let ips: Vec<IpAddr> = list.iter().map(|s| s.ip).collect();
                state
                    .best_ips
                    .write()
                    .await
                    .insert(domain.clone(), ips.clone());
                updated_domains.push(domain.clone());
                all_speeds.push(DomainSpeedDto {
                    domain: domain.clone(),
                    ips: list
                        .iter()
                        .map(|s| IpSpeedDto {
                            ip: s.ip.to_string(),
                            latency_ms: s.latency_ms,
                            loss: s.loss,
                            speed_mbps: s.speed_mbps,
                            colo: s.colo.clone(),
                        })
                        .collect(),
                });
            }
            Err(e) => {
                failures += 1;
                events::emit_error(state, "speedtest", &format!("{domain}: {e}")).await;
            }
        }
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
    }

    // 回退：无任何结果的域名 → default_ips（仅当该域名当前也没有缓存时）
    if !cfg.default_ips.is_empty() {
        let mut best = state.best_ips.write().await;
        for domain in &cfg.target_domains {
            if !best.contains_key(domain) {
                best.insert(domain.clone(), cfg.default_ips.clone());
                log::warn!("域名 {domain} 无测速结果，回退默认 IP {:?}", cfg.default_ips);
            }
        }
    }

    // 更新状态快照
    let any_success = !updated_domains.is_empty();
    {
        let mut st = state.status.write().await;
        st.refreshing = false;
        if any_success {
            *state.last_refresh.write().await = Instant::now();
            st.last_refresh_at = Some(SystemTime::now());
            st.from_cache = false;
            st.speeds = all_speeds.clone();
            if st.last_error.is_some() && failures == 0 {
                st.last_error = None; // 本轮全部成功，清除旧错误
            }
        }
    }

    if any_success {
        let snapshot = state.best_ips.read().await.clone();
        save_best_ips_cache(&state.config_dir, &snapshot);
        events::emit_best_ips(state).await;
        log::info!(
            "刷新完成: {} 个域名更新, {} 个失败, 耗时 {:.1}s",
            updated_domains.len(),
            failures,
            started.elapsed().as_secs_f64()
        );
    } else {
        log::warn!("刷新完成但无任何成功结果 (failures={failures})，保留旧缓存");
    }
    events::emit_status(state).await;
    state.refresh_inflight.store(false, Ordering::SeqCst);
}

// ===================== 配置应用 / DNS 重启 =====================

/// 应用新配置：校验 → 持久化 → 端口变化时重启 DNS。不自动触发测速
/// （调用方决定是否 trigger_refresh）。
pub async fn apply_config(
    state: &Arc<OptimizerState>,
    mut cfg: OptimizerConfig,
) -> Result<(), String> {
    cfg.validate()?;
    let old_port = state.config.read().await.dns_port;
    *state.config.write().await = cfg.clone();
    save_config(&state.config_dir, &cfg);
    log::info!("配置已更新: {} 个目标域名, dns_port={}", cfg.target_domains.len(), cfg.dns_port);
    if cfg.dns_port != old_port {
        restart_dns(state, cfg.dns_port).await?;
    }
    Ok(())
}

/// 重启本地 DNS 服务（换端口时用）。
pub async fn restart_dns(state: &Arc<OptimizerState>, port: u16) -> Result<(), String> {
    if let Some(h) = state.dns_server.write().await.take() {
        h.abort();
    }
    {
        let mut st = state.status.write().await;
        st.dns_running = false;
        st.dns_port = None;
    }
    // 等旧 socket 释放
    tokio::time::sleep(Duration::from_millis(150)).await;
    match dns::spawn_dns_server(state.clone(), port) {
        Ok(h) => {
            *state.dns_server.write().await = Some(h);
            Ok(())
        }
        Err(e) => {
            events::emit_error(state, "dns", &e).await;
            Err(e)
        }
    }
}
