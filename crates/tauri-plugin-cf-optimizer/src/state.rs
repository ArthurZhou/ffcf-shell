//! 插件核心状态（OptimizerState）。
//!
//! 前四个字段严格按开发规格定义；其余为工程扩展字段
//! （HTTP 客户端、事件句柄、运行状态快照、测速二进制定位等）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use serde::Serialize;
use tauri::AppHandle;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::OptimizerConfig;

/// 单个 IP 的测速明细（供前端展示与事件负载）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpSpeedDto {
    pub ip: String,
    pub latency_ms: f64,
    pub loss: f64,
    pub speed_mbps: Option<f64>,
    /// Cloudflare 数据中心代码（如 HKG/SJC），来自 CloudflareST-Rust CSV 第 7 列
    pub colo: String,
}

/// 单个目标域名的测速明细。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainSpeedDto {
    pub domain: String,
    pub ips: Vec<IpSpeedDto>,
}

/// 运行状态快照（best_ips/last_refresh 之外的可变状态集中在这里）。
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    /// 本地 DNS 服务是否成功绑定并运行
    pub dns_running: bool,
    /// 实际绑定的 DNS 端口（绑定成功后写入）
    pub dns_port: Option<u16>,
    /// 是否有测速任务正在进行
    pub refreshing: bool,
    /// 最近一次成功测速的墙钟时间
    pub last_refresh_at: Option<SystemTime>,
    /// 最近一次错误描述（测速失败/DNS 绑定失败等）
    pub last_error: Option<String>,
    /// 当前 best_ips 是否来自磁盘缓存（本次会话尚未测速成功）
    pub from_cache: bool,
    /// 最近一次测速的每域名明细
    pub speeds: Vec<DomainSpeedDto>,
}

/// 插件全局共享状态。所有命令、DNS handler、Resolver、后台任务持有 `Arc<OptimizerState>`。
pub struct OptimizerState {
    // ===== 规格要求的字段 =====
    /// 域名（小写）→ 优选 IP 列表（按延迟升序，越靠前越优）
    pub best_ips: Arc<RwLock<HashMap<String, Vec<IpAddr>>>>,
    /// 最近一次成功测速的时刻（单调时钟，用于判断缓存年龄）
    pub last_refresh: Arc<RwLock<Instant>>,
    /// 本地 DNS 服务后台任务句柄（None = 未启动/启动失败）
    pub dns_server: Arc<RwLock<Option<JoinHandle<()>>>>,
    /// 全局配置
    pub config: Arc<RwLock<OptimizerConfig>>,

    // ===== 工程扩展字段 =====
    /// 注入了 OptimizerResolver 的 reqwest 客户端。
    /// 用 OnceLock 打破「state → client → resolver → state」的构造循环：
    /// setup 阶段先建 state，再建 client 并 set 进来。
    pub http_client: OnceLock<reqwest::Client>,
    /// 用于向前端发事件的 AppHandle。单元测试场景为 None（事件静默丢弃）。
    pub app: OnceLock<AppHandle>,
    /// 运行状态快照
    pub status: RwLock<StatusSnapshot>,
    /// 配置文件 / best-ips 缓存 / 测速临时文件所在目录（app_config_dir）
    pub config_dir: PathBuf,
    /// CloudflareST-Rust 二进制候选路径，按优先级排列，最后一个是裸名（走 PATH）
    pub speedtest_candidates: RwLock<Vec<PathBuf>>,
    /// 测速任务防重入标志（手动刷新与周期刷新共用）
    pub refresh_inflight: AtomicBool,
    /// 关停标志（refresh_loop 轮询）
    pub shutdown: AtomicBool,
}

impl OptimizerState {
    pub fn new(config: OptimizerConfig, config_dir: PathBuf) -> Self {
        Self::with_cache(config, config_dir, HashMap::new(), Vec::new())
    }

    /// 构造期直接注入磁盘缓存与二进制候选路径。
    ///
    /// 为什么不用 `blocking_write`：本构造函数在 tauri 插件 `setup()` 里被调用，
    /// 该时机可能已处于 tokio runtime 上下文，`blocking_write` 会 panic
    /// ("Cannot block the current thread from within a runtime")。构造期注入零风险，
    /// 且能让「冷启动即可解析」成立。
    pub fn with_cache(
        config: OptimizerConfig,
        config_dir: PathBuf,
        cached_ips: HashMap<String, Vec<IpAddr>>,
        speedtest_candidates: Vec<PathBuf>,
    ) -> Self {
        let from_cache = !cached_ips.is_empty();
        Self {
            best_ips: Arc::new(RwLock::new(cached_ips)),
            last_refresh: Arc::new(RwLock::new(Instant::now())),
            dns_server: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(config)),
            http_client: OnceLock::new(),
            app: OnceLock::new(),
            status: RwLock::new(StatusSnapshot {
                from_cache,
                ..Default::default()
            }),
            config_dir,
            speedtest_candidates: RwLock::new(speedtest_candidates),
            refresh_inflight: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    /// 读取当前配置快照（克隆，避免长期持锁）。
    pub async fn config_snapshot(&self) -> OptimizerConfig {
        self.config.read().await.clone()
    }
}
