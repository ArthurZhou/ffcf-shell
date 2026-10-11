//! 本地 DNS 服务（hickory-server 0.26）。
//!
//! 设计要点（对应开发规格模块 2）：
//! - 监听 **127.0.0.1:10053**（UDP + TCP）。绑定地址写死回环，端口来自配置但
//!   config.rs 已硬性拒绝 53 与 <1024 特权端口 —— 双保险满足验收项 1/2。
//! - 目标域名（config.is_target）→ 从 best_ips 读取优选 IP，按查询类型返回
//!   A/AAAA 记录，TTL 30 秒；缓存未命中时透明转发上游（保证可用性）。
//! - 其他域名 → 原样转发到上游 1.1.1.1 / 8.8.8.8（UDP），把上游应答的
//!   answers/authorities/additionals 重组回响应。
//! - 启动失败（端口被占等）→ 返回 Err，engine 发 optimizer://error 事件，
//!   Resolver 自动走系统 DNS 回退（验收项：DNS 启动失败回退系统 DNS）。
//!
//! 不修改系统 DNS：本服务只是被动监听，只有本应用内的 Resolver 会来查询。

use std::iter;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::op::{
    Header, HeaderCounts, Message, MessageType, Metadata, OpCode, ResponseCode,
};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_server::net::runtime::Time;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo, Server};
use hickory_server::zone_handler::MessageResponseBuilder;
use tokio::task::JoinHandle;

use crate::config::{DNS_BIND_ADDR, OPTIMIZED_TTL, UPSTREAM_DNS};
use crate::state::OptimizerState;

/// RequestHandler 实现：持有共享状态，按「目标域→优选IP / 其他→转发」分发。
pub struct OptimizerHandler {
    pub state: Arc<OptimizerState>,
}

impl OptimizerHandler {
    /// 查询目标域名的优选 IP（按查询类型过滤 v4/v6）。
    async fn optimized_ips(&self, domain: &str, qtype: RecordType) -> Vec<IpAddr> {
        let map = self.state.best_ips.read().await;
        let Some(ips) = map.get(domain) else {
            return Vec::new();
        };
        ips.iter()
            .filter(|ip| match qtype {
                RecordType::A => ip.is_ipv4(),
                RecordType::AAAA => ip.is_ipv6(),
                _ => false,
            })
            .copied()
            .collect()
    }

    /// 把 IP 列表组装成 A/AAAA 应答记录，TTL = 30s。
    fn build_records(qname: &Name, qtype: RecordType, ips: &[IpAddr]) -> Vec<Record> {
        ips.iter()
            .filter_map(|ip| match (qtype, ip) {
                (RecordType::A, IpAddr::V4(v4)) => {
                    Some(Record::from_rdata(qname.clone(), OPTIMIZED_TTL, RData::A(A(*v4))))
                }
                (RecordType::AAAA, IpAddr::V6(v6)) => {
                    Some(Record::from_rdata(qname.clone(), OPTIMIZED_TTL, RData::AAAA(AAAA(*v6))))
                }
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl RequestHandler for OptimizerHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let builder = MessageResponseBuilder::new(&request.queries, request.edns.as_ref());

        // 只处理标准查询；其余一律 NotImp/FormErr
        if request.metadata.op_code != OpCode::Query {
            return finish(
                response_handle
                    .send_response(builder.error_msg(&request.metadata, ResponseCode::NotImp))
                    .await,
                request,
            );
        }
        let Some(query) = request.queries.queries().first() else {
            return finish(
                response_handle
                    .send_response(builder.error_msg(&request.metadata, ResponseCode::FormErr))
                    .await,
                request,
            );
        };

        let qtype = query.query_type();
        let qname: Name = query.original().name().clone();
        let domain = qname.to_utf8().to_lowercase();
        let domain = domain.trim_end_matches('.').to_string();

        let cfg = self.state.config.read().await;
        let is_target = cfg.is_target(&domain)
            && matches!(qtype, RecordType::A | RecordType::AAAA);
        drop(cfg);

        if is_target {
            let ips = self.optimized_ips(&domain, qtype).await;
            if !ips.is_empty() {
                // ===== 目标域名命中优选缓存：直接应答 A/AAAA，TTL 30 =====
                let records = Self::build_records(&qname, qtype, &ips);
                log::debug!("dns[{domain}] serve optimized {:?} -> {:?}", qtype, ips);
                let mut meta = Metadata::response_from_request(&request.metadata);
                meta.response_code = ResponseCode::NoError;
                meta.recursion_available = true;
                let resp = builder.build(
                    meta,
                    records.iter(),
                    iter::empty(),
                    iter::empty(),
                    iter::empty(),
                );
                return finish(response_handle.send_response(resp).await, request);
            }
            log::debug!("dns[{domain}] target but cache miss -> forward upstream");
            // 缓存未命中：落到下面的转发逻辑，保证域名始终可解析
        }

        // ===== 非目标域名 / 目标域名缓存未命中：转发上游 =====
        match forward_upstream(request.as_slice()).await {
            Some(upstream) => {
                let mut meta = Metadata::response_from_request(&request.metadata);
                meta.response_code = upstream.response_code;
                meta.recursion_available = true;
                let resp = builder.build(
                    meta,
                    upstream.answers.iter(),
                    upstream.authorities.iter(),
                    iter::empty(),
                    upstream.additionals.iter(),
                );
                finish(response_handle.send_response(resp).await, request)
            }
            None => {
                log::warn!("dns[{domain}] all upstreams failed -> ServFail");
                finish(
                    response_handle
                        .send_response(builder.error_msg(&request.metadata, ResponseCode::ServFail))
                        .await,
                    request,
                )
            }
        }
    }
}

/// 统一收敛 send_response 的结果；失败时构造 ServFail 响应头。
/// （ResponseInfo::serve_failed 是 hickory 的 pub(crate)，这里手工构造等价物。）
fn finish(result: Result<ResponseInfo, impl std::fmt::Display>, request: &Request) -> ResponseInfo {
    match result {
        Ok(info) => info,
        Err(e) => {
            log::error!("send_response failed: {e}");
            let mut metadata = Metadata::new(
                request.metadata.id,
                MessageType::Response,
                request.metadata.op_code,
            );
            metadata.response_code = ResponseCode::ServFail;
            ResponseInfo::from(Header {
                metadata,
                counts: HeaderCounts::default(),
            })
        }
    }
}

/// 把原始查询报文转发到上游 DNS（UDP，依次尝试 1.1.1.1 / 8.8.8.8）。
/// 返回解析后的上游应答；全部失败返回 None。
pub async fn forward_upstream(raw_query: &[u8]) -> Option<Message> {
    for upstream in UPSTREAM_DNS {
        let Ok(addr) = SocketAddr::from_str(upstream) else {
            continue;
        };
        let fut = async {
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
            socket.send_to(raw_query, addr).await.ok()?;
            let mut buf = vec![0u8; 4096];
            let (n, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buf))
                .await
                .ok()?
                .ok()?;
            Message::from_vec(&buf[..n]).ok()
        };
        if let Some(msg) = fut.await {
            return Some(msg);
        }
        log::debug!("upstream {upstream} failed, trying next");
    }
    None
}

/// 启动本地 DNS 服务（UDP + TCP 同时监听 127.0.0.1:port）。
///
/// 成功返回后台任务 JoinHandle（存入 state.dns_server，规格要求）；
/// 绑定失败返回 Err(原因)，由 engine 发 optimizer://error 并让 Resolver 回退系统 DNS。
///
/// 端口绑定必须在调用方（spawn 前）同步完成，这样「端口被占用」能立刻被发现，
/// 而不是悄悄消失在后台任务里。
pub fn spawn_dns_server(
    state: Arc<OptimizerState>,
    port: u16,
) -> Result<JoinHandle<()>, String> {
    // 双保险：即使有人绕过 config 校验直接调这里，也拒绝 53/特权端口。
    if port == 53 || port < 1024 {
        return Err(format!("拒绝绑定端口 {port}：禁止 53 及 <1024 特权端口"));
    }
    let bind_addr: SocketAddr = format!("{DNS_BIND_ADDR}:{port}")
        .parse()
        .map_err(|e| format!("非法监听地址: {e}"))?;

    // 先在当前线程同步绑定（用 std socket），失败立即返回 Err
    let std_udp = std::net::UdpSocket::bind(bind_addr)
        .map_err(|e| format!("UDP 绑定 {bind_addr} 失败: {e}"))?;
    let std_tcp = std::net::TcpListener::bind(bind_addr)
        .map_err(|e| format!("TCP 绑定 {bind_addr} 失败: {e}"))?;
    std_udp
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking 失败: {e}"))?;
    std_tcp
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking 失败: {e}"))?;

    let handle = tokio::spawn(async move {
        let udp = match tokio::net::UdpSocket::from_std(std_udp) {
            Ok(s) => s,
            Err(e) => {
                log::error!("UDP socket 转换失败: {e}");
                return;
            }
        };
        let tcp = match tokio::net::TcpListener::from_std(std_tcp) {
            Ok(l) => l,
            Err(e) => {
                log::error!("TCP listener 转换失败: {e}");
                return;
            }
        };

        let handler = OptimizerHandler {
            state: state.clone(),
        };
        let mut server = Server::new(handler);
        server.register_socket(udp);
        server.register_listener(tcp, Duration::from_secs(30), 4096);

        {
            let mut st = state.status.write().await;
            st.dns_running = true;
            st.dns_port = Some(port);
        }
        log::info!("本地 DNS 服务已启动: {bind_addr} (UDP+TCP)");

        // block_until_done 会一直运行直到所有 socket 任务结束（进程退出/任务 abort）
        if let Err(e) = server.block_until_done().await {
            log::error!("DNS 服务异常退出: {e}");
            let mut st = state.status.write().await;
            st.dns_running = false;
        }
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OptimizerConfig;
    use std::collections::HashMap;

    /// 测试端口，避开开发实例的 10053。
    const TEST_PORT: u16 = 15053;

    /// 手工发一个 A/AAAA 查询到本地 DNS，收集应答里的 IP。
    /// （与 resolver.rs 的查询逻辑一致，这里独立实现用于集成测试。）
    async fn query_a(server_port: u16, domain: &str, qtype: RecordType) -> (ResponseCode, Vec<IpAddr>) {
        let id: u16 = 4242;
        let mut msg = Message::new(id, MessageType::Query, OpCode::Query);
        msg.add_query(hickory_proto::op::Query::query(
            Name::from_str(domain).unwrap(),
            qtype,
        ));
        let bytes = msg.to_vec().unwrap();

        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket
            .send_to(&bytes, ("127.0.0.1", server_port))
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .expect("dns query timeout")
            .unwrap();
        let resp = Message::from_vec(&buf[..n]).unwrap();
        let ips = resp
            .answers
            .iter()
            .filter_map(|r| match &r.data {
                RData::A(a) => Some(IpAddr::V4(a.0)),
                RData::AAAA(a) => Some(IpAddr::V6(a.0)),
                _ => None,
            })
            .collect();
        (resp.response_code, ips)
    }

    async fn make_state(domains: &[&str], ips: HashMap<&str, Vec<IpAddr>>) -> Arc<OptimizerState> {
        let mut cfg = OptimizerConfig::default();
        cfg.target_domains = domains.iter().map(|s| s.to_string()).collect();
        cfg.dns_port = TEST_PORT;
        cfg.validate().unwrap();
        let state = Arc::new(OptimizerState::new(cfg, std::env::temp_dir()));
        let mut map = HashMap::new();
        for (d, v) in ips {
            map.insert(d.to_string(), v);
        }
        *state.best_ips.write().await = map;
        state
    }

    #[tokio::test]
    async fn target_domain_returns_optimized_ips() {
        let state = make_state(
            &["target.example"],
            HashMap::from([(
                "target.example",
                vec![
                    "104.16.1.2".parse().unwrap(),
                    "172.64.3.4".parse().unwrap(),
                ],
            )]),
        )
        .await;
        let handle = spawn_dns_server(state.clone(), TEST_PORT).expect("bind failed");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let (rc, ips) = query_a(TEST_PORT, "target.example", RecordType::A).await;
        assert_eq!(rc, ResponseCode::NoError);
        assert_eq!(ips.len(), 2);
        assert!(ips.contains(&"104.16.1.2".parse().unwrap()));

        handle.abort();
    }

    #[tokio::test]
    async fn refuses_port_53_and_privileged() {
        let state = make_state(&[], HashMap::new()).await;
        assert!(spawn_dns_server(state.clone(), 53).is_err());
        assert!(spawn_dns_server(state.clone(), 535).is_err());
        // 10053 本身合法（可能被 dev 实例占用，被占则 Err 也符合预期，只验证不是端口规则拒绝）
    }

    #[tokio::test]
    async fn second_bind_on_same_port_fails_loudly() {
        let state = make_state(&[], HashMap::new()).await;
        // 用独立端口，避免与 resolver.rs 并行测试（15054）撞车
        const BIND_PORT: u16 = 15060;
        let h1 = spawn_dns_server(state.clone(), BIND_PORT).expect("first bind");
        let h2 = spawn_dns_server(state.clone(), BIND_PORT);
        assert!(h2.is_err(), "端口占用必须立即报错而不是静默失败");
        h1.abort();
    }
}
