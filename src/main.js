// ffcf-shell 前端：状态卡片 / 优选 IP 表 / 解析诊断 / api_request 测试器 / 配置 / 事件日志。
// 所有后端交互走 Tauri invoke（裸命令名，宿主 app 级注册），事件走 listen。
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------- 小工具 ----------
const $ = (id) => document.getElementById(id);

function logLine(kind, text) {
  const el = $("log");
  const t = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  const div = document.createElement("div");
  div.className = `log-line ${kind}`;
  div.textContent = `[${t}] ${text}`;
  el.prepend(div);
  while (el.children.length > 200) el.lastChild.remove();
}

function fmtAgo(ms) {
  if (ms == null) return "从未";
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  return `${Math.floor(s / 3600)} 小时前`;
}

// ---------- 状态渲染 ----------
let lastStatus = null;

function renderStatus(st) {
  lastStatus = st;
  $("st-dns").textContent = st.dnsRunning ? "运行中" : "未运行";
  $("dns-badge").className = `badge ${st.dnsRunning ? "on" : "off"}`;
  $("st-port").textContent = st.dnsPort ? `127.0.0.1:${st.dnsPort}` : "—";
  $("st-refresh").textContent = fmtAgo(st.lastRefreshUnixMs);
  $("refresh-badge").textContent = st.refreshing ? "测速中…" : fmtAgo(st.lastRefreshUnixMs);
  $("refresh-badge").className = `badge ${st.refreshing ? "busy" : ""}`;
  $("st-source").textContent = st.fromCache ? "磁盘缓存" : "实时测速";
  const bin = st.speedtestBin || "未找到";
  $("st-bin").textContent = bin.split("/").pop();
  $("st-bin").title = bin;
  $("st-error").textContent = st.lastError || "无";
  $("st-error").className = st.lastError ? "err" : "";

  // 配置表单回填（仅在用户未聚焦时）
  if (document.activeElement !== $("cfg-domains"))
    $("cfg-domains").value = (st.targetDomains || []).join(", ");
  if (document.activeElement !== $("cfg-interval"))
    $("cfg-interval").value = st.refreshIntervalSecs ?? 600;
  if (document.activeElement !== $("cfg-topn")) $("cfg-topn").value = st.topN ?? 4;

  renderBestTable(st);
}

function renderBestTable(st) {
  const tbody = $("tbl-best").querySelector("tbody");
  const speeds = st.speeds || [];
  const rows = [];
  for (const d of speeds) {
    for (const ip of d.ips || []) {
      rows.push(
        `<tr><td>${d.domain}</td><td>${ip.ip}</td><td>${ip.latencyMs.toFixed(1)}</td>` +
        `<td>${(ip.loss * 100).toFixed(0)}%</td><td>${ip.speedMbps?.toFixed(1) ?? "—"}</td>` +
        `<td>${ip.colo || "—"}</td></tr>`
      );
    }
  }
  // 缓存态（无 speeds 明细）也要能看到 bestIps
  if (rows.length === 0) {
    for (const [domain, ips] of Object.entries(st.bestIps || {})) {
      for (const ip of ips) {
        rows.push(
          `<tr class="cached"><td>${domain}</td><td>${ip}</td><td colspan="4">磁盘缓存（本次会话未测速）</td></tr>`
        );
      }
    }
  }
  tbody.innerHTML = rows.length
    ? rows.join("")
    : `<tr><td colspan="6" class="empty">${st.refreshing ? "测速中，请稍候…" : "暂无数据 — 点击「手动刷新测速」"}</td></tr>`;
}

// ---------- 事件订阅 ----------
listen("optimizer://status", (e) => {
  renderStatus(e.payload);
});
listen("optimizer://best-ips-updated", (e) => {
  const n = Object.values(e.payload.bestIps || {}).reduce((a, v) => a + v.length, 0);
  logLine("ok", `best-ips-updated: ${Object.keys(e.payload.bestIps || {}).length} 域名 / ${n} IP`);
  invoke("optimizer_get_status").then(renderStatus).catch(() => {});
});
listen("optimizer://error", (e) => {
  logLine("error", `${e.payload.context}: ${e.payload.message}`);
});

// ---------- 按钮 ----------
$("btn-refresh").onclick = async () => {
  $("btn-refresh").disabled = true;
  try {
    const st = await invoke("optimizer_refresh");
    renderStatus(st);
    logLine("info", "已触发手动刷新（后台执行）");
  } catch (e) {
    logLine("error", `refresh 失败: ${e}`);
  } finally {
    setTimeout(() => ($("btn-refresh").disabled = false), 1500);
  }
};

$("btn-resolve").onclick = async () => {
  const domain = $("in-resolve").value.trim();
  if (!domain) return;
  const out = $("resolve-out");
  out.classList.remove("hidden");
  out.textContent = "诊断中…";
  try {
    const r = await invoke("optimizer_resolve", { domain });
    out.textContent = JSON.stringify(r, null, 2);
    logLine("info", `resolve ${domain}: best=${r.bestIps.length} local=${r.localDnsIps?.length ?? "-"} sys=${r.systemIps.length}`);
  } catch (e) {
    out.textContent = `错误: ${e}`;
  }
};

$("btn-request").onclick = async () => {
  const out = $("req-out");
  const meta = $("req-meta");
  out.classList.remove("hidden");
  meta.classList.remove("hidden");
  meta.textContent = "请求中…";
  out.textContent = "";
  let headers = {};
  const hraw = $("req-headers").value.trim();
  if (hraw) {
    try {
      headers = JSON.parse(hraw);
    } catch {
      meta.textContent = "headers 不是合法 JSON";
      return;
    }
  }
  const body = $("req-body").value.trim() || null;
  try {
    const r = await invoke("api_request", {
      req: {
        method: $("req-method").value,
        url: $("req-url").value.trim(),
        headers,
        body,
      },
    });
    meta.innerHTML =
      `<b>HTTP ${r.status}</b> · ${r.elapsedMs}ms · 实际连接 <code>${r.remoteAddr ?? "未知"}</code>` +
      (r.bodyTruncated ? " · body 已截断" : "");
    let bodyText = r.body;
    try {
      bodyText = JSON.stringify(JSON.parse(r.body), null, 2);
    } catch { /* 非 JSON 原样显示 */ }
    out.textContent = bodyText || "(空响应体)";
    logLine(r.status < 400 ? "ok" : "error", `api_request ${r.method ?? ""} ${$("req-url").value} → ${r.status} via ${r.remoteAddr ?? "?"} ${r.elapsedMs}ms`);
  } catch (e) {
    meta.textContent = "请求失败";
    out.textContent = String(e);
    logLine("error", `api_request 失败: ${e}`);
  }
};

$("btn-save-cfg").onclick = async () => {
  try {
    const current = await invoke("optimizer_get_config");
    const next = {
      ...current,
      targetDomains: $("cfg-domains").value.split(",").map((s) => s.trim()).filter(Boolean),
      refreshIntervalSecs: Number($("cfg-interval").value) || 0,
      topN: Number($("cfg-topn").value) || 4,
    };
    const st = await invoke("optimizer_set_config", { config: next });
    renderStatus(st);
    logLine("ok", `配置已保存: ${next.targetDomains.join(", ") || "(空)"} / 间隔 ${next.refreshIntervalSecs}s / topN ${next.topN}`);
  } catch (e) {
    logLine("error", `保存配置失败: ${e}`);
    alert(`保存配置失败:\n${e}`);
  }
};

$("btn-toggle-cfg").onclick = async () => {
  const el = $("cfg-full");
  if (el.classList.contains("hidden")) {
    el.textContent = JSON.stringify(await invoke("optimizer_get_config"), null, 2);
    el.classList.remove("hidden");
    $("btn-toggle-cfg").textContent = "隐藏完整 JSON";
  } else {
    el.classList.add("hidden");
    $("btn-toggle-cfg").textContent = "显示完整 JSON";
  }
};

// ---------- 启动 ----------
(async () => {
  try {
    renderStatus(await invoke("optimizer_get_status"));
    logLine("info", "前端就绪，已订阅 optimizer:// 事件");
  } catch (e) {
    logLine("error", `初始状态获取失败: ${e}`);
  }
  // 每秒刷新「x 秒前」显示
  setInterval(() => {
    if (lastStatus) renderStatus(lastStatus);
  }, 5000);
})();
