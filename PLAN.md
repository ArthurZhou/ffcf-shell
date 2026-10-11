# PLAN — ffcf-shell

> **名字来历**：F**king Fast CloudFlare Shell（2026-10-11 Tony 定，iMessage）。
> Cargo crate 名保留 `cf-ip-optimizer`（宿主）/ `tauri-plugin-cf-optimizer`（插件），产品名 ffcf-shell。

> 状态检查点文件。每完成一步打勾，避免长任务丢进度（AGENTS.md 大任务规范）。

## 需求来源
2026-10-11 iMessage (yitian.zhou@icloud.com)：Tauri v2 桌面应用，客户 0 配置，自动为自有域名优选 Cloudflare IP。
不改系统 DNS、不占 53 端口，本地 DNS 监听 127.0.0.1:10053，SNI/Host 保持原域名。

## 前置调研（已完成 2026-10-11）
- [x] 工具链：rustc 1.97.1 / cargo 1.97.1 / node v26.11.1 / npm 11.20.0 / sips+iconutil / PIL
- [x] CloudflareST-Rust = GitHub 仓库 GuangYu-yu/CloudflareST-Rust，**是 CLI 二进制不是 crate**（crates.io 无同名包）
      → 集成方式：子进程调用 + 解析 CSV（`-o` 输出），二进制已下载到 `bin/CloudflareST-Rust`（macOS ARM64, main-latest）
      → CSV 表头：`IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),数据中心`
      → 冒烟测试通过：`-ip 1.1.1.1 -t 2 -dd` 正常出结果
- [x] API 版本锁定：tauri 2.12 / tauri-plugin 2.7 / hickory-server 0.26.3 / reqwest 0.13.5 / tokio 1.53
- [x] reqwest 0.13 `dns::Resolve { fn resolve(&self, name: Name) -> Resolving }`、`Name::as_str()` 确认
- [x] hickory-server 0.26 `RequestHandler::handle_request` 签名确认（docs.rs）

## 实施步骤
- [x] 0. 项目目录 + PLAN
- [x] 1. 工作区骨架：Cargo workspace + package.json + vite + tauri.conf.json + capabilities + icons
- [x] 2. 插件 crate `crates/tauri-plugin-cf-optimizer`：
  - [x] config.rs（OptimizerConfig 默认值/校验/load/save + best-ips 缓存）
  - [x] state.rs（OptimizerState：best_ips/last_refresh/dns_server/config + 扩展字段）
  - [x] logger.rs（文件+stderr 日志，无额外插件依赖）
  - [x] speedtest.rs（CloudflareST-Rust 子进程封装 + CSV 解析 + 二进制定位）
  - [x] dns.rs（hickory-server handler：目标域→优选IP TTL30；其他→转发 1.1.1.1/8.8.8.8）
  - [x] resolver.rs（reqwest Resolve 三级回退：best_ips → 本地DNS → 系统DNS）
  - [x] events.rs（optimizer://status、optimizer://best-ips-updated、optimizer://error + StatusDto）
  - [x] engine.rs（DNS 启动/重启、周期刷新、回退链、事件广播）
  - [x] commands.rs（optimizer_get_status/refresh/resolve、api_request、get/set_config）
  - [x] lib.rs（tauri::plugin::Builder 组装 init()）
- [x] 3. 宿主 app src-tauri（main.rs 注册插件）
- [x] 4. 前端（vanilla JS + @tauri-apps/api：状态卡片、域名表、api_request 测试器、配置编辑、事件日志）
- [x] 5. CloudflareST-Rust 二进制 → src-tauri/resources/ + bundle.resources；scripts/fetch-speedtest-bin.sh
- [x] 6. npm install + vite build（dist/）
- [x] 7. cargo build（迭代修编译错）
- [x] 8. cargo test（CSV 解析 / config 校验 / DNS 回环集成测试 / resolver 单测）
- [x] 9. tauri build --debug --no-bundle（端到端编译验证，不弹窗）
- [x] 10. README.md（构建/运行/配置/验收对照）
- [x] 11. 交付总结 + memory 记录

## 关键决策记录
1. **测速引擎走子进程而非 crate 依赖**：CloudflareST-Rust 无 lib.rs（纯 binary crate），未发布 crates.io。
   子进程 + CSV 解析是唯一稳定集成路径，也符合规格"现成 crate 或命令行"的命令行分支。
2. **插件具体化 Wry**（不做泛型 Runtime）：桌面专用，减少 async_trait/泛型摩擦。
3. **http client 放 `tokio::sync::OnceCell`**：resolver 需要 Arc<OptimizerState>，client 又需要 resolver，
   用懒初始化打破循环（规格结构体字段全部保留）。
4. **DNS 端口硬校验**：配置层拒绝 53/特权端口，绑定地址写死 127.0.0.1 不可配置。
5. **回退链**：测速失败→保留旧 best_ips（缓存）→无缓存用 config.default_ips→仍无则 resolver 落到系统 DNS。
6. 测试用 15053 端口避免与开发实例冲突；DEFAULT_DNS_PORT==10053 由单测断言。

## 验收标准对照（2026-10-11 全部核对完成）
- [x] 不修改系统 DNS，不监听 53 端口 —— 绑定地址写死 127.0.0.1；config+dns 双层拒绝 53/<1024；`lsof` 实测只有 10053
- [x] 本地 DNS 127.0.0.1:10053 UDP/TCP 可用 —— `dig` UDP/TCP 双模式实测返回结果
- [x] 目标域名→优选 IP；非目标→转发 —— `dig example.com`(目标) / `dig cloudflare.com`(非目标) 实测；单测 target_domain_returns_optimized_ips
- [x] api_request 实际连接优选 IP，SNI/Host 正确 —— 返回 remote_addr；Resolver 只改域名→IP，SNI/Host 取自 URL 原域名
- [x] 测速失败回退默认 IP/缓存 —— engine::refresh_once 回退链（旧缓存→default_ips→系统 DNS）
- [x] 定期更新 + 事件通知 —— 周期循环 + optimizer://best-ips-updated / status
- [x] 启动 5 秒内可请求（缓存同步预载）—— 构造期注入磁盘缓存，Resolver 第 1 级立即命中；DNS 异步启动不阻塞
- [x] 日志可观测 —— logger.rs stderr+文件；启动/DNS/测速全链路日志实测打印
- [x] CloudflareST-Rust 集成成功 —— 子进程+CSV 解析，单测 parses_real_csv_shape，二进制冒烟测试通过

## 最终验证结果（2026-10-11 20:2x）
- `npm run build` ✅ dist/ 产出（3 资源，46ms）
- `cargo build --workspace` ✅ 零警告（宿主 2 个 unused import 已清）
- `cargo test -p tauri-plugin-cf-optimizer` ✅ **14 passed / 0 failed**
- 运行时实测 ✅ app 启动后本地 DNS 真实应答 dig 查询（UDP+TCP），lsof 确认只监听回环

## 编译期踩坑记录（已全部修复）
1. **reqwest 0.13 feature 改名**：`rustls-tls` → `rustls`（0.13.5 Cargo.toml: `default-tls = ["rustls"]`）
2. **Xcode license 未同意** → `cc`/`ar` exit 69。绕过：`.cargo/config.toml` 把 linker/CC/CXX/AR/SDKROOT 指向 Command Line Tools（无需 sudo）。正解 `sudo xcodebuild -license accept`
3. **tauri_plugin::Builder 硬要求** `Cargo.toml` 里 `links == package.name`
4. **`type SharedState = State<'_, ...>` → E0106**：类型别名定义位置没有可推导生命周期，参数必须直写 `State<'_, Arc<OptimizerState>>`
5. **`Record::data` 是公开字段不是方法**（`data()` 只存在于 `RecordRef`）—— hickory-proto 0.26.3 实测
6. **`Emitter::emit` 硬约束 `S: Serialize + Clone`** —— 事件负载 struct 必须 derive Clone
7. **`blocking_write` 在 setup() 里会 panic**（可能已在 tokio runtime 上下文）—— 改用 `OptimizerState::with_cache` 构造期注入
8. **`httping_url` 必须用 `{domain}` 占位**打到目标域名，否则测的不是用户访问该域名的真实延迟
9. **测试端口冲突**：dns.rs 与 resolver.rs 测试并行跑，共用 15054 会撞车 → 分到 15060
