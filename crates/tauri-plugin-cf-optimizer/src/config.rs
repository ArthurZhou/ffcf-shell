use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// 本地 DNS 服务监听端口。硬编码为 10053，规格要求禁止使用 53 端口。
pub const DEFAULT_DNS_PORT: u16 = 10053;
/// 本地 DNS 监听地址。规格要求固定为回环地址，不可配置，避免暴露到公网。
pub const DNS_BIND_ADDR: &str = "127.0.0.1";
/// 优选结果 A/AAAA 记录的 TTL（秒）。规格要求 30 秒。
pub const OPTIMIZED_TTL: u32 = 30;
/// 上游转发 DNS（非目标域名）。优先 Cloudflare，其次 Google。
pub const UPSTREAM_DNS: [&str; 2] = ["1.1.1.1:53", "8.8.8.8:53"];

/// 插件全局配置。可由前端 optimizer_set_config 覆盖，并持久化到磁盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizerConfig {
    /// 需要加速的自有域名列表（小写）。只有这些域名会走优选 IP。
    pub target_domains: Vec<String>,
    /// 每个域名保留的优选 IP 数量。
    pub top_n: usize,
    /// CloudflareST-Rust 可执行文件路径。None 表示由 speedtest 模块自动定位。
    pub speedtest_bin: Option<String>,
    /// 测速候选来源：CIDR 或 IP 段（传给 CloudflareST-Rust 的 -ip 参数）。
    pub ip_ranges: Vec<String>,
    /// HTTPing 测速地址。用于探测到目标域名的真实 TLS 延迟。
    pub httping_url: String,
    /// 下载测速地址（可选）。为空则仅做延迟测速（-dd）。
    pub download_url: String,
    /// 单次延迟测速次数（-t）。
    pub ping_times: u16,
    /// 下载测速结果数量（-dn）。
    pub test_count: usize,
    /// 延迟上限毫秒（-tl）。
    pub max_delay_ms: u64,
    /// 后台自动刷新间隔（秒）。0 表示禁用自动刷新。
    pub refresh_interval_secs: u64,
    /// 测速失败/无结果时的回退 IP（每个目标域名共用）。
    pub default_ips: Vec<IpAddr>,
    /// 本地 DNS 端口。默认 10053，禁止 53 及 <1024 特权端口。
    pub dns_port: u16,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            // 演示默认域名；客户可在配置中替换为自有域名。
            target_domains: vec!["example.com".to_string()],
            top_n: 4,
            speedtest_bin: None,
            // Cloudflare 常见 anycast 段，够快且覆盖广。
            ip_ranges: vec![
                "104.16.0.0/13=100".to_string(),
                "104.24.0.0/14=100".to_string(),
                "172.64.0.0/13=100".to_string(),
            ],
            // `{domain}` 占位符在测速时替换为目标域名：httping 直接打到
            // 「候选 IP + 目标域名 Host/SNI」，测出的才是用户访问该域名的真实延迟。
            // /cdn-cgi/trace 由 Cloudflare 边缘对任意 hostname 提供，无需源站配合。
            httping_url: "https://{domain}/cdn-cgi/trace".to_string(),
            download_url: String::new(),
            ping_times: 4,
            test_count: 8,
            max_delay_ms: 800,
            refresh_interval_secs: 600,
            default_ips: vec![
                "104.16.0.1".parse().unwrap(),
                "172.64.0.1".parse().unwrap(),
            ],
            dns_port: DEFAULT_DNS_PORT,
        }
    }
}

impl OptimizerConfig {
    /// 校验并归一化配置。返回 Err(原因) 时调用方应拒绝该配置。
    ///
    /// 关键约束：
    /// - dns_port 不得为 53，也不得为 <1024 的特权端口（规格硬要求）。
    /// - 域名统一转小写、去重、去空白。
    pub fn validate(&mut self) -> Result<(), String> {
        if self.dns_port == 53 || self.dns_port < 1024 {
            return Err(format!(
                "dns_port {} 非法：禁止使用 53 端口及 <1024 特权端口",
                self.dns_port
            ));
        }
        if self.top_n == 0 {
            self.top_n = 1;
        }
        if self.ping_times == 0 {
            self.ping_times = 1;
        }
        // 归一化域名：小写 + 去尾点 + 去重。
        let mut seen = std::collections::HashSet::new();
        let mut norm = Vec::new();
        for d in self.target_domains.drain(..) {
            let mut d = d.trim().to_lowercase();
            while d.ends_with('.') {
                d.pop();
            }
            if d.is_empty() || !seen.insert(d.clone()) {
                continue;
            }
            norm.push(d);
        }
        self.target_domains = norm;
        Ok(())
    }

    /// 判断某域名（已小写归一）是否为加速目标。支持精确匹配与后缀匹配（子域）。
    pub fn is_target(&self, domain: &str) -> bool {
        let d = domain.trim_end_matches('.');
        self.target_domains.iter().any(|t| {
            d == t.as_str()
                || d.ends_with(&format!(".{}", t))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_port_53() {
        let mut c = OptimizerConfig::default();
        c.dns_port = 53;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_privileged_port() {
        let mut c = OptimizerConfig::default();
        c.dns_port = 535;
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_default_port() {
        let mut c = OptimizerConfig::default();
        assert_eq!(c.dns_port, DEFAULT_DNS_PORT);
        assert!(c.validate().is_ok());
        assert_eq!(c.dns_port, 10053);
    }

    #[test]
    fn normalizes_domains() {
        let mut c = OptimizerConfig::default();
        c.target_domains = vec![
            "Foo.Example.COM.".into(),
            "foo.example.com".into(),
            "  ".into(),
            "Bar.test".into(),
        ];
        c.validate().unwrap();
        assert_eq!(c.target_domains, vec!["foo.example.com", "bar.test"]);
    }

    #[test]
    fn target_suffix_match() {
        let mut c = OptimizerConfig::default();
        c.target_domains = vec!["example.com".into()];
        c.validate().unwrap();
        assert!(c.is_target("example.com"));
        assert!(c.is_target("api.example.com"));
        assert!(c.is_target("deep.sub.example.com"));
        assert!(!c.is_target("notexample.com"));
        assert!(!c.is_target("example.org"));
    }
}
