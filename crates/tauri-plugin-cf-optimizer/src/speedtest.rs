//! 测速引擎：CloudflareST-Rust 子进程封装。
//!
//! CloudflareST-Rust（github.com/GuangYu-yu/CloudflareST-Rust）是一个**纯 CLI 二进制**
//! （无 lib.rs、未发布 crates.io），因此按开发规格中「现成 crate 或命令行」的命令行分支集成：
//! 子进程调用 `-o` 输出 CSV，再解析 CSV 得到「IP/延迟/丢包/下载速度/数据中心」。
//!
//! CSV 表头（实测 main-latest 版本）：
//! `IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),数据中心`

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

use crate::config::OptimizerConfig;

/// 二进制文件名（Windows 带 .exe）。
pub fn bin_name() -> &'static str {
    if cfg!(windows) {
        "CloudflareST-Rust.exe"
    } else {
        "CloudflareST-Rust"
    }
}

/// 按优先级定位 CloudflareST-Rust 二进制。
/// 候选列表由 engine 在 setup 时写入 state（resource_dir → exe 同目录 → 项目 bin/ → PATH）。
pub fn locate_binary(candidates: &[PathBuf], config_override: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = config_override {
        let p = PathBuf::from(p);
        if !p.is_relative() && p.exists() {
            return Some(p);
        }
        if p.is_relative() {
            // 相对路径按裸名处理，交给 PATH 解析
            return Some(p);
        }
    }
    for c in candidates {
        // 最后一个候选是裸名（走 PATH），exists() 为 false 但仍可尝试
        if c.exists() {
            return Some(c.clone());
        }
    }
    // PATH 兜底：返回裸名，spawn 时由操作系统解析
    Some(PathBuf::from(bin_name()))
}

/// 一条测速结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpSpeed {
    pub ip: IpAddr,
    /// 平均延迟（毫秒）
    pub latency_ms: f64,
    /// 丢包率 0.0–1.0
    pub loss: f64,
    /// 下载速度 MB/s（未做下载测速时为 None）
    pub speed_mbps: Option<f64>,
    /// Cloudflare 数据中心代码
    pub colo: String,
}

/// 对单个目标域名执行一轮测速，返回按延迟升序的前 top_n 个 IP。
///
/// httping 地址支持 `{domain}` 占位符：测速流量直接打到目标域名（经候选 IP），
/// 这样得到的延迟就是「用户访问该域名」的真实延迟。
pub async fn run_for_domain(
    cfg: &OptimizerConfig,
    bin: &Path,
    domain: &str,
    work_dir: &Path,
) -> Result<Vec<IpSpeed>, String> {
    let _ = std::fs::create_dir_all(work_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let out_file = work_dir.join(format!(
        "cfst-{}-{stamp}.csv",
        domain.replace('.', "_")
    ));

    let httping = if cfg.httping_url.contains("{domain}") {
        cfg.httping_url.replace("{domain}", domain)
    } else {
        cfg.httping_url.clone()
    };

    let mut cmd = Command::new(bin);
    cmd.arg("-ip")
        .arg(cfg.ip_ranges.join(","))
        .arg("-t")
        .arg(cfg.ping_times.to_string())
        .arg("-dn")
        .arg(cfg.test_count.to_string())
        .arg("-tl")
        .arg(cfg.max_delay_ms.to_string())
        .arg("-tlr")
        .arg("0.5")
        .arg("-httping")
        .arg(&httping)
        .arg("-p")
        .arg("20")
        .arg("-timeout")
        .arg("120")
        .arg("-o")
        .arg(&out_file);
    if cfg.download_url.is_empty() {
        // 仅延迟测速（规格允许；下载测速地址可后续在配置中开启）
        cmd.arg("-dd");
    } else {
        cmd.arg("-url").arg(&cfg.download_url);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());

    log::info!("speedtest[{domain}] spawn: {bin:?} httping={httping}");

    // 硬超时 = CloudflareST 自身 -timeout 120 再加 60s 余量，防止子进程卡死
    let output = tokio::time::timeout(Duration::from_secs(180), cmd.output())
        .await
        .map_err(|_| format!("CloudflareST-Rust 超时(180s)未退出: {bin:?}"))?
        .map_err(|e| format!("无法启动 CloudflareST-Rust ({bin:?}): {e}"))?;

    // 即使退出码非 0 也尝试读 CSV（可能已有部分结果）
    let text = tokio::fs::read_to_string(&out_file).await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&out_file).await;

    let mut list = parse_csv(&text)?;
    if list.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!(
            "测速无结果 (exit={:?}): {}",
            output.status.code(),
            tail.trim()
        ));
    }
    list.sort_by(|a, b| a.latency_ms.partial_cmp(&b.latency_ms).unwrap_or(std::cmp::Ordering::Equal));
    list.truncate(cfg.top_n.max(1));
    log::info!(
        "speedtest[{domain}] ok: {} ips, best={} {:.1}ms",
        list.len(),
        list[0].ip,
        list[0].latency_ms
    );
    Ok(list)
}

/// 解析 CloudflareST-Rust 输出的 CSV。
/// 表头行自动跳过；`IP 地址` 列可能带端口（-sp 时）或 IPv6 方括号，均做归一化。
pub fn parse_csv(text: &str) -> Result<Vec<IpSpeed>, String> {
    let mut out = Vec::new();
    // 数据行计数：区分「只有表头（合法空结果）」与「有内容但全解析失败（格式异常）」。
    let mut data_lines_seen = 0usize;
    for line in text.lines() {
        let line = line.trim();
        // 去掉可能的 UTF-8 BOM，再判空行/表头
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if line.is_empty() || line.starts_with("IP") {
            continue;
        }
        data_lines_seen += 1;
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 5 {
            continue;
        }
        let Some(ip) = parse_ip_field(cols[0].trim()) else {
            log::debug!("skip unparsable ip field: {:?}", cols[0]);
            continue;
        };
        let loss = cols.get(3).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0);
        let latency = match cols.get(4).and_then(|s| s.trim().parse::<f64>().ok()) {
            Some(v) => v,
            None => continue, // 无延迟数据 = ping 不通，丢弃
        };
        let speed = cols
            .get(5)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| *v > 0.0);
        let colo = cols.get(6).map(|s| s.trim().to_string()).unwrap_or_default();
        out.push(IpSpeed {
            ip,
            latency_ms: latency,
            loss,
            speed_mbps: speed,
            colo,
        });
    }
    // 只有出现过数据行、却一条都没解析出来时才算错误；纯表头/纯空文本 → Ok(空)
    if data_lines_seen > 0 && out.is_empty() {
        return Err("CSV 无有效数据行".to_string());
    }
    Ok(out)
}

/// 归一化 IP 字段：纯 IP / `1.2.3.4:443` / `[2606:4700::1]:443` / `2606:4700::1`。
fn parse_ip_field(s: &str) -> Option<IpAddr> {
    if let Ok(ip) = IpAddr::from_str(s) {
        return Some(ip);
    }
    // [v6]:port
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((v6, _)) = rest.split_once(']') {
            return IpAddr::from_str(v6).ok();
        }
    }
    // v4:port —— 只有最后一个冒号右侧是纯数字才当作端口剥离
    if let Some((head, tail)) = s.rsplit_once(':') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return IpAddr::from_str(head).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),数据中心
104.16.1.2,4,4,0.00,45.32,12.50,HKG
172.64.3.4,4,3,0.25,88.10,,SJC
1.1.1.1:443,4,4,0.00,12.00,30.00,LAX
[2606:4700::1]:443,4,4,0.00,30.00,,
bad-line
104.16.9.9,4,0,1.00,,,
";

    #[test]
    fn parses_real_csv_shape() {
        let v = parse_csv(SAMPLE).unwrap();
        // bad-line 丢弃；104.16.9.9 无延迟丢弃；其余 4 条保留
        assert_eq!(v.len(), 4, "{v:?}");
        assert_eq!(v[0].ip.to_string(), "104.16.1.2");
        assert!((v[0].latency_ms - 45.32).abs() < 1e-9);
        assert_eq!(v[0].colo, "HKG");
        assert_eq!(v[1].loss, 0.25);
        assert!(v[1].speed_mbps.is_none());
        // 带端口的 v4
        assert_eq!(v[2].ip.to_string(), "1.1.1.1");
        // 带方括号端口的 v6
        assert_eq!(v[3].ip.to_string(), "2606:4700::1");
    }

    #[test]
    fn ip_field_normalization() {
        assert_eq!(parse_ip_field("104.16.0.1").unwrap().to_string(), "104.16.0.1");
        assert_eq!(parse_ip_field("104.16.0.1:8443").unwrap().to_string(), "104.16.0.1");
        assert_eq!(
            parse_ip_field("[2606:4700::1]:443").unwrap().to_string(),
            "2606:4700::1"
        );
        assert_eq!(
            parse_ip_field("2606:4700::1").unwrap().to_string(),
            "2606:4700::1"
        );
        assert!(parse_ip_field("not-an-ip").is_none());
    }

    #[test]
    fn empty_csv_is_error() {
        assert!(parse_csv("").is_ok()); // 空文本 → 空列表（调用方决定是否为错误）
        assert!(parse_csv("IP 地址,已发送\n").is_ok());
        assert!(parse_csv("garbage\nmore,garbage\n").is_err());
    }
}
