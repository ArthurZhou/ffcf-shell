# ⚡ ffcf-shell — Fucking Fast CloudFlare Shell

> 一个 **Tauri v2** 桌面应用，为「你自己的域名」自动优选 Cloudflare 边缘 IP 并加速访问。
> **不修改系统 DNS、不占用 53 端口**，本地 DNS 只监听 `127.0.0.1:10053`。
> SNI / Host 始终保持原始域名，只有底层 TCP 连接换成优选 IP。

名字来历：**F**ucking **F**ast **C**loud**F**lare **Shell**（2026-10-11 由 Tony 定）。

---

## 它解决什么问题

你的域名挂在 Cloudflare 后面，DNS 解析到的是 Cloudflare 的 **anycast** IP。
但同一个 anycast 段里，**哪个 IP 对你当前网络最快是不确定的** —— 官方 DNS 轮询不保证给你最优的那条。

ffcf-shell 做的事：
1. 后台用 [CloudflareST-Rust](https://github.com/GuangYu-yu/CloudflareST-Rust) 对你的目标域名**实测**每个候选 IP 的延迟/丢包/速度；
2. 选出最快的几个，写进本地内存表 `best_ips`；
3. 应用内所有 HTTP 请求（走 `api_request`）通过自定义 Resolver，把域名解析成这些**优选 IP**；
4. 定期重新测速，结果变了就通过事件通知前端。

---

## 架构

```
┌───────────────────────────── Tauri v2 App ─────────────────────────────┐
│                                                                         │
│  前端 (vanilla JS)  ──invoke──►  命令层 commands.rs                     │
│      ▲                                    │                             │
│      │ optimizer://status                 ▼                             │
│      │ optimizer://best-ips-updated   api_request                       │
│      │ optimizer://error                  │                             │
│      └─────────── events.rs          reqwest::Client                    │
│                                      + OptimizerResolver (resolver.rs)   │
│                                            │                            │
│                              三级回退: ① best_ips 内存                   │
│                                        ② 本地 DNS 127.0.0.1:10053        │
│                                        ③ 系统解析 (getaddrinfo)          │
│                                            │                            │
│                                            ▼                            │
│  engine.rs (周期测速) ──► speedtest.rs ──子进程──► CloudflareST-Rust     │
│      │                                              (CSV 解析)          │
│      └──► dns.rs (hickory-server)                                        │
│              目标域 → best_ips 返回 A/AAAA (TTL 30)                       │
│              其他域 → 透明转发 1.1.1.1 / 8.8.8.8                          │
└─────────────────────────────────────────────────────────────────────────┘
```

### 模块对照（`crates/tauri-plugin-cf-optimizer/src/`）

| 文件 | 职责 |
|------|------|
| `config.rs` | `OptimizerConfig`：目标域名、top_n、刷新间隔、DNS 端口。**硬校验拒绝 53 及 <1024 特权端口**，域名归一化（小写/去尾点/去重） |
| `state.rs` | `OptimizerState`：`best_ips` / `last_refresh` / `dns_server` / `config`（规格字段）+ HTTP 客户端、事件句柄、状态快照（工程扩展） |
| `speedtest.rs` | CloudflareST-Rust 子进程封装 + CSV 解析（IP/延迟/丢包/速度/数据中心）。二进制按优先级定位 |
| `dns.rs` | hickory-server 本地 DNS：目标域→优选 IP，其他→转发上游。绑定 `127.0.0.1`，**双保险拒绝 53/特权端口** |
| `resolver.rs` | 自定义 reqwest Resolver，三级回退。**SNI/Host 天然保持原域名**（只改域名→IP 这一步） |
| `engine.rs` | 启动装配、DNS 生命周期、周期测速循环、配置/缓存持久化、回退链 |
| `events.rs` | `optimizer://status` / `best-ips-updated` / `error` 三事件 + `StatusDto` |
| `commands.rs` | 6 个 Tauri 命令：`optimizer_get_status/refresh/resolve/get_config/set_config`、`api_request` |
| `logger.rs` | stderr + 滚动文件日志 |

---

## 构建 & 运行

### 前置
- Rust（rustc 1.97+）、Node 26+ / npm 11+
- macOS 上若 Xcode license 未同意，项目已用 `.cargo/config.toml` 把 linker/ar/SDKROOT 指向 Command Line Tools 绕过（见下「本机注意事项」）

### 1. 取测速二进制
```bash
scripts/fetch-speedtest-bin.sh          # 自动检测平台
# 或手动：把 CloudflareST-Rust 放到 src-tauri/resources/ 和 bin/
```
本仓库已附带 macOS arm64 的 `src-tauri/resources/CloudflareST-Rust`（已冒烟测试通过）。

### 2. 装前端依赖 + 构建
```bash
npm install
npm run build        # 产出 dist/
```

### 3. 编译 / 运行
```bash
cargo build --workspace                     # debug 编译（已验证通过）
cargo test -p tauri-plugin-cf-optimizer     # 14 个测试（已全绿）
cargo tauri dev                             # 开发模式（热重载）
cargo tauri build                           # 打包 release
```

> 直接跑 `target/debug/cf-ip-optimizer` 会拉起 GUI 窗口（不是 CLI，`--help` 也会开窗口）。

### 4. 验证本地 DNS（app 运行后）
```bash
dig @127.0.0.1 -p 10053 example.com A +short        # 目标域→优选/转发
dig @127.0.0.1 -p 10053 cloudflare.com A +short     # 非目标域→转发上游
dig @127.0.0.1 -p 10053 example.com A +tcp +short   # TCP 模式
lsof -nP -iUDP:10053 -iTCP:10053                    # 确认只监听 127.0.0.1
```

---

## 配置

配置持久化在 `app_config_dir/optimizer-config.json`，也可在前端「配置」卡片改。
默认目标域名是 `example.com`（演示用），**换成你自己的域名**后才会对它测速。

关键项：
- `targetDomains` — 要加速的自有域名（支持子域后缀匹配）
- `topN` — 每个域名保留几个优选 IP（默认 4）
- `refreshIntervalSecs` — 后台重测间隔（默认 600s，0=禁用）
- `httpingUrl` — 默认 `https://{domain}/cdn-cgi/trace`，`{domain}` 测速时替换成目标域名
- `dnsPort` — 默认 10053，**禁止 53 及 <1024**
- `defaultIps` — 测速彻底失败时的兜底 IP

---

## 验收对照

| 验收项 | 状态 | 证据 |
|--------|------|------|
| 不修改系统 DNS，不监听 53 | ✅ | 绑定地址写死 `127.0.0.1`；config + dns 双层拒绝 53/<1024；`lsof` 实测只有 10053 |
| 本地 DNS 10053 UDP/TCP 可用 | ✅ | `dig` UDP/TCP 均返回结果 |
| 目标域→优选 IP；非目标→转发 | ✅ | `dig example.com` 与 `dig cloudflare.com` 实测；单测 `target_domain_returns_optimized_ips` |
| api_request 实际连优选 IP，SNI/Host 正确 | ✅ | `api_request` 返回 `remote_addr`；Resolver 只改域名→IP，SNI/Host 取自 URL |
| 测速失败回退默认 IP/缓存 | ✅ | `engine::refresh_once` 回退链；旧缓存优先，再 `default_ips`，仍无则 Resolver 落系统 DNS |
| 定期更新 + 事件通知 | ✅ | 周期循环 + `optimizer://best-ips-updated` / `status` |
| 启动 5 秒内可请求 | ✅ | 磁盘缓存构造期同步预载，Resolver 第 1 级立即命中；DNS 启动异步不阻塞 |
| 日志可观测 | ✅ | `logger.rs` stderr + 文件；启动/测速/DNS 全链路日志实测打印 |
| CloudflareST-Rust 集成成功 | ✅ | 子进程 + CSV 解析，单测 `parses_real_csv_shape`；二进制冒烟测试通过 |

测试：`cargo test -p tauri-plugin-cf-optimizer` → **14 passed / 0 failed**（含 DNS 回环集成测试、Resolver 三级、CSV 解析、config 校验）。

---

## 本机注意事项（重要）

**Xcode license 未同意** 会导致 `cc`/`ar` 链接失败（exit 69，"You have not agreed to the Xcode license agreements"）。
本项目已在 `.cargo/config.toml` 里把 linker / CC / CXX / AR / SDKROOT 指向 **Command Line Tools** 绕过，无需 sudo。

一劳永逸的正解是手动执行一次（需要密码）：
```bash
sudo xcodebuild -license accept
```
同意后即可删除 `.cargo/config.toml`。

**reqwest 0.13 的 TLS feature 改名**：旧 `rustls-tls` → `rustls`（`Cargo.toml` 已用新名，并注明）。

---

## 关键设计决策

1. **测速引擎走子进程而非 crate 依赖** — CloudflareST-Rust 无 `lib.rs`（纯 binary crate）、未发布 crates.io。子进程 + CSV 解析是唯一稳定路径，也符合规格「现成 crate 或命令行」的命令行分支。
2. **插件具体化 Wry**（不做泛型 Runtime）— 桌面专用，减少 async_trait/泛型摩擦。
3. **HTTP client 放 `OnceLock`** — resolver 需要 `Arc<OptimizerState>`，client 又需要 resolver，用懒初始化打破构造循环。
4. **构造期注入缓存，不用 `blocking_write`** — setup() 可能已在 tokio runtime 上下文，`blocking_write` 会 panic；改用 `OptimizerState::with_cache`。
5. **命令双注册** — 插件内注册（`plugin:cf-optimizer|xxx`）+ 宿主 app 级重复注册同一批函数，让前端既能用裸名 `invoke('optimizer_get_status')`，两边共享同一 `State<Arc<OptimizerState>>`。

---

## 目录结构

```
ffcf-shell/
├── Cargo.toml                     # workspace
├── .cargo/config.toml             # CLT linker/SDK 绕过 Xcode license
├── package.json / vite.config.js / index.html / src/{main.js,style.css}
├── scripts/fetch-speedtest-bin.sh
├── bin/CloudflareST-Rust          # 测速二进制（macOS arm64）
├── crates/tauri-plugin-cf-optimizer/
│   ├── Cargo.toml / build.rs / permissions/default.toml
│   └── src/{config,state,speedtest,dns,resolver,engine,events,commands,logger,lib}.rs
└── src-tauri/
    ├── Cargo.toml / build.rs / tauri.conf.json / capabilities/default.json
    ├── icons/                     # PNG + .icns
    ├── resources/CloudflareST-Rust
    └── src/{main.rs,lib.rs}
```

MIT License.
