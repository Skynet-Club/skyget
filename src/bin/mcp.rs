//! 天网盒子 · MCP server（stdio）。
//!
//! 这是 agent 发现天网盒子的入口：Claude Code / Codex / 其它 MCP 客户端
//! 在配置里加一行就能拿到 `skyget_want` / `skyget_watch` / `skyget_status`
//! / `skyget_cancel` 四个工具。
//!
//! 它**自己不下载**，只把调用转给本机守护进程 —— 否则每个 agent 会话
//! 都会变成一个新的数据盒子，缓存、并发和台账又分裂了。
//!
//! 协议：MCP 的 stdio 传输是「一行一个 JSON-RPC 2.0 对象」。
//! stdout 只放协议消息，日志一律走 stderr。

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 转给守护进程的那一层。抽成 trait 是为了不启动真 daemon 也能测协议。
trait Daemon {
    fn post_job(&self, body: &Value) -> Result<Value, String>;
    fn post_upload(&self, body: &Value) -> Result<Value, String>;
    fn get_job(&self, id: &str) -> Result<Value, String>;
    fn events(&self, id: &str, since: i64, wait_ms: u64) -> Result<Value, String>;
    fn cancel(&self, id: &str) -> Result<Value, String>;
    fn retry(&self, id: &str) -> Result<Value, String>;
    fn pin(&self, sha256: &str, agent: &str, ttl_secs: u64, note: &str) -> Result<Value, String>;
    fn unpin(&self, sha256: &str, agent: Option<&str>) -> Result<Value, String>;
    /// 这台数据盒子上有哪些 agent、各自的策略和并发（`GET /agents`）。
    fn agents(&self) -> Result<Value, String>;
    /// 当前最值得显示的活跃任务（`GET /statusline`）。
    fn active(&self, source: Option<&str>) -> Result<Value, String>;
    /// 预检：`POST /check`。**4xx 也是答案**，不是传输失败。
    fn check(&self, body: &Value) -> Result<Value, String>;
}

struct HttpDaemon {
    base: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

impl HttpDaemon {
    fn new() -> Self {
        // 支持 `SKYGET_API=unix:/path`（进程内回环转发，身份由 socket 决定）。
        let base = skyfetch::uds::base_url();
        let token = std::env::var("SKYGET_API_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let client = reqwest::blocking::Client::builder()
            // watch 最长长轮询 25 秒，留点余量；其余请求也会在这个上限内。
            .timeout(Duration::from_secs(40))
            .build()
            .expect("构造 MCP HTTP 客户端失败");
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            client,
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        let mut rb = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "DELETE" => self.client.delete(&url),
            _ => return Err(format!("不支持的 HTTP 方法：{method}")),
        };
        if let Some(t) = &self.token {
            rb = rb.bearer_auth(t);
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let r = rb.send().map_err(|e| {
            format!("连不上守护进程（{url}）：{e}。先启动 skynet-dataplaned（旧名 skynet-downloadd）。")
        })?;
        let status = r.status();
        let text = r.text().unwrap_or_default();
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| json!({"raw": text.clone()}));
        if status.is_success() {
            Ok(v)
        } else {
            Err(format!("HTTP {}：{}", status.as_u16(), text))
        }
    }
}

/// `POST /check` 专用的调用：403/400 是「会被拒」这个结论本身，
/// 不能像 `call()` 那样当异常抛掉，否则 agent 拿不到 checks 和 next_action。
impl HttpDaemon {
    fn call_any_status(&self, path: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        let mut rb = self.client.post(&url);
        if let Some(t) = &self.token {
            rb = rb.bearer_auth(t);
        }
        let r = rb.json(body).send().map_err(|e| {
            format!("连不上守护进程（{url}）：{e}。先启动 skynet-dataplaned（旧名 skynet-downloadd）。")
        })?;
        let status = r.status();
        let text = r.text().unwrap_or_default();
        let mut v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| json!({"raw": text.clone(), "status": status.as_u16()}));
        if let Some(o) = v.as_object_mut() {
            o.insert("http_status".into(), json!(status.as_u16()));
        }
        Ok(v)
    }
}

impl Daemon for HttpDaemon {
    fn post_job(&self, body: &Value) -> Result<Value, String> {
        self.call("POST", "/jobs", Some(body))
    }
    fn post_upload(&self, body: &Value) -> Result<Value, String> {
        self.call("POST", "/uploads", Some(body))
    }
    fn get_job(&self, id: &str) -> Result<Value, String> {
        self.call("GET", &format!("/jobs/{id}"), None)
    }
    fn events(&self, id: &str, since: i64, wait_ms: u64) -> Result<Value, String> {
        self.call(
            "GET",
            &format!("/jobs/{id}/events?since={since}&wait_ms={wait_ms}"),
            None,
        )
    }
    fn cancel(&self, id: &str) -> Result<Value, String> {
        self.call("POST", &format!("/jobs/{id}/cancel"), Some(&json!({})))
    }
    fn retry(&self, id: &str) -> Result<Value, String> {
        self.call("POST", &format!("/jobs/{id}/retry"), Some(&json!({})))
    }
    fn pin(&self, sha256: &str, agent: &str, ttl_secs: u64, note: &str) -> Result<Value, String> {
        self.call(
            "POST",
            "/pins",
            Some(&json!({
                "sha256": sha256, "agent": agent,
                "ttl_secs": ttl_secs, "note": note,
            })),
        )
    }
    fn unpin(&self, sha256: &str, agent: Option<&str>) -> Result<Value, String> {
        let path = match agent {
            Some(a) => format!("/pins/{sha256}?agent={a}"),
            None => format!("/pins/{sha256}"),
        };
        self.call("DELETE", &path, None)
    }
    fn check(&self, body: &Value) -> Result<Value, String> {
        self.call_any_status("/check", body)
    }
    fn agents(&self) -> Result<Value, String> {
        self.call("GET", "/agents", None)
    }
    fn active(&self, source: Option<&str>) -> Result<Value, String> {
        let path = match source {
            Some(s) => format!("/statusline?source={s}"),
            None => "/statusline".to_string(),
        };
        self.call("GET", &path, None)
    }
}

fn main() {
    let daemon = HttpDaemon::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("JSON 解析失败：{e}")}
                });
                let _ = writeln!(stdout, "{resp}");
                let _ = stdout.flush();
                continue;
            }
        };
        {
            let mut notifier = Notifier {
                out: &mut stdout,
                token: None,
            };
            if let Some(resp) = handle_message_with(&req, &daemon, &mut notifier) {
                let _ = writeln!(notifier.out, "{resp}");
                let _ = notifier.out.flush();
            }
        }
    }
}

/// 进度通知的出口。`token` 来自请求的 `_meta.progressToken`；没有就不发。
struct Notifier<'a> {
    out: &'a mut dyn Write,
    token: Option<Value>,
}

impl Notifier<'_> {
    fn progress(&mut self, progress: u64, total: u64, message: &str) {
        let Some(token) = &self.token else {
            return;
        };
        let mut params = json!({
            "progressToken": token,
            "progress": progress,
            "message": message,
        });
        if total > 0 {
            params["total"] = json!(total);
        }
        let note = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        });
        let _ = writeln!(self.out, "{note}");
        let _ = self.out.flush();
    }
}

/// 测试和不需要进度的调用方用这个：通知写到 sink。
#[cfg(test)]
fn handle_message(req: &Value, daemon: &dyn Daemon) -> Option<Value> {
    let mut sink = std::io::sink();
    let mut notifier = Notifier {
        out: &mut sink,
        token: None,
    };
    handle_message_with(req, daemon, &mut notifier)
}

/// 处理一条 JSON-RPC 消息。通知（没有 id）返回 None，不回包。
fn handle_message_with(
    req: &Value,
    daemon: &dyn Daemon,
    notifier: &mut Notifier,
) -> Option<Value> {
    if notifier.token.is_none() {
        notifier.token = req.pointer("/params/_meta/progressToken").cloned();
    }
    let id = req.get("id").cloned()?;
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            // 客户端可能报 2024-11-05 / 2025-03-26 / 2025-06-18。
            // 我们只用到 tools 这一小块，版本号原样回即可；没报就给当前常用的。
            let version = req
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "skynet-mcp", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_specs()})),
        "tools/call" => tools_call(req.get("params"), daemon, notifier),
        _ => Err((-32601, format!("不认识的方法：{method}"))),
    };

    Some(match result {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": code, "message": message}
        }),
    })
}

fn tool_specs() -> Value {
    json!([
        {
            "name": "skyget_check",
            "description": "提交前预检：这条地址会被接受还是被拒、卡在哪一项。不排队、不联网、不产生任务。返回 ok + checks[{name,ok,detail}]；被拒时带 code / next_action。提交前先问一句，能省掉一次失败提交。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "要检查的地址"},
                    "dest": {"type": "string", "description": "落地目录（默认 daemon 的默认目录）"},
                    "name": {"type": "string", "description": "文件名（配合 dest 目录）"},
                    "sha256": {"type": "string", "description": "期望 sha256；多源时必填"},
                    "sources": {"type": "array", "items": {"type": "string"}, "description": "额外镜像"},
                    "credential_ref": {"type": "string", "description": "env:NAME / file:PATH"},
                    "path": {"type": "string", "description": "上传源文件（检查 SKYGET_UPLOAD_ROOTS）"}
                },
                "required": ["url"]
            }
        },
        {
            "name": "skyget_want",
            "description": "把一份内容取到本机并校验。给 sha256 就是硬门槛：不符会失败并丢弃文件。返回 job_id；用 skyget_watch 跟进度。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "http/https 直链、magnet、.torrent 或 .m3u8"},
                    "sources": {"type": "array", "items": {"type": "string"}, "description": "额外 http(s) 镜像；多源必须同时给 sha256"},
                    "sha256": {"type": "string", "description": "期望的 sha256（强烈建议）；不符即失败，不落地"},
                    "credential_ref": {"type": "string", "description": "env:NAME / file:PATH；作为 Authorization: Bearer，值不落台账"},
                    "dest": {"type": "string", "description": "落地目录或完整路径；默认 ~/Downloads"},
                    "idempotency_key": {"type": "string", "description": "同一意图重复调用用同一个键，避免重复下载"},
                    "accept_change": {"type": "boolean", "description": "地址内容变了也接受（正常更新时用）"},
                    "agent": {"type": "string", "description": "调用者名字，记进台账；默认取 SKYGET_AGENT 或 mcp"}
                },
                "required": ["url"]
            }
        },
        {
            "name": "skyget_upload",
            "description": "把本地文件上传到远端（默认 HTTP PUT；protocol=tus 可断点续传）。源文件必须在 SKYGET_UPLOAD_ROOTS 内，默认禁止上传。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "本地源文件绝对路径"},
                    "url": {"type": "string", "description": "上传目标地址"},
                    "protocol": {"type": "string", "enum": ["put", "tus"], "description": "默认 put"},
                    "sha256": {"type": "string", "description": "本地文件期望 sha256；不符不上传"},
                    "credential_ref": {"type": "string", "description": "env:NAME / file:PATH；凭证不落台账"},
                    "agent": {"type": "string"}
                },
                "required": ["path", "url"]
            }
        },
        {
            "name": "skyget_watch",
            "description": "跟一个任务的事件流。返回 done=true 就说明任务已结束，不要再调用；否则用返回的 since 接着读。events 只含最近 20 条，历史不要重放。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": {"type": "string"},
                    "since": {"type": "integer", "description": "上次读到的 seq；0 或不给 = 从头"},
                    "wait_ms": {"type": "integer", "description": "没有新事件时最多等多久（0-25000）"}
                },
                "required": ["job_id"]
            }
        },
        {
            "name": "skyget_status",
            "description": "查一条任务的当前状态（台账里的那一行）。",
            "inputSchema": {
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"]
            }
        },
        {
            "name": "skyget_cancel",
            "description": "取消一条任务：停止下载、清掉未完成分片、删记录。已落地的文件不动。",
            "inputSchema": {
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"]
            }
        },
        {
            "name": "skyget_retry",
            "description": "重跑一条失败/中断的任务：同一条记录、同一个期望 sha256，已下的分片接着用。",
            "inputSchema": {
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"]
            }
        },
        {
            "name": "skyget_pin",
            "description": "钉住一份缓存内容（「这个模型我在用，别清」）。ttl_secs=0 表示不过期，默认 7 天。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sha256": {"type": "string"},
                    "ttl_secs": {"type": "integer", "description": "保留秒数；0 = 不过期；默认 604800（7 天）"},
                    "note": {"type": "string"}
                },
                "required": ["sha256"]
            }
        },
        {
            "name": "skyget_unpin",
            "description": "解除 pin；不给 agent 就去掉所有 agent 的 pin。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sha256": {"type": "string"},
                    "agent": {"type": "string", "description": "只解除这个 agent 的 pin；默认全部"}
                },
                "required": ["sha256"]
            }
        },
        {
            "name": "skyget_agents",
            "description": "看这台数据盒子上有哪些 agent、各自的任务数/并发上限/落地根/允许主机/上传根。token 不会出现在返回里；用 agent token 调用时只看得到自己。",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "skyget_active",
            "description": "当前最值得显示的活跃任务（running > pending > interrupted）。statusline/TUI 用，不需要 job_id；没有任务时 state=idle。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "按来源前缀过滤，例如 agent:claude"}
                }
            }
        }
    ])
}

/// 任务 id 的合法形状。job_id 会被拼进 URL path：
/// `"x/../../jobs"` 会被 reqwest/url 规范化成 `/jobs`，等于让 MCP 变成
/// “读全台账 / 取消别人任务”的入口。只接受 40 位十六进制。
fn valid_job_id(id: &str) -> bool {
    id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn job_arg(args: &Value) -> Result<&str, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "缺少 job_id".to_string())?;
    if !valid_job_id(id) {
        return Err("job_id 必须是 40 位十六进制".to_string());
    }
    Ok(id)
}

fn error_result(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn agent_name(args: &Value) -> String {
    args.get("agent")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SKYGET_AGENT").ok())
        .unwrap_or_else(|| "mcp".to_string())
}

fn source_name(name: &str) -> String {
    let v = name.trim();
    if v.is_empty() || v == "cli" || v == "human" || v.contains(':') {
        v.to_string()
    } else {
        format!("agent:{v}")
    }
}

/// 一行可读的进度：条 + 百分比 + 已处理/总量。
///
/// 为什么要自己排：MCP 只给了 `progress`/`total`/`message` 三个字段，各 TUI 的渲染
/// 由客户端决定。pi 直接把原始字节数摊出来（`downloading 13771/10485760`），很丑；
/// 改成百分比 + 预排好的条，渲染再朴素也不会难看。
fn progress_line(done: u64, total: u64, unit: &str) -> (u64, String) {
    const W: usize = 10;
    let pct = done.saturating_mul(100).checked_div(total).unwrap_or(0).min(100);
    let filled = (pct as usize * W) / 100;
    let bar = format!("{}{}", "━".repeat(filled), "─".repeat(W - filled));
    let (d, t) = if unit == "s" {
        (fmt_secs(done), fmt_secs(total))
    } else {
        (fmt_bytes(done), fmt_bytes(total))
    };
    (pct, format!("skyget {bar} {pct:>3}%  {d}/{t}"))
}

fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n}B") } else { format!("{v:.1}{}", U[i]) }
}

fn fmt_secs(s: u64) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

/// 给一页原始事件补上「结束了没有」和最近进度，让调用方能自己停。
fn annotate(page: &mut Value) {
    let mut terminal = false;
    let mut done_bytes = 0u64;
    let mut total_bytes = 0u64;
    if let Some(events) = page.get("events").and_then(|x| x.as_array()) {
        for e in events {
            let kind = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let data = e.get("data").unwrap_or(&Value::Null);
            match kind {
                "progress" => {
                    done_bytes = data.get("bytes_done").and_then(|x| x.as_u64()).unwrap_or(done_bytes);
                    total_bytes = data.get("bytes_total").and_then(|x| x.as_u64()).unwrap_or(total_bytes);
                }
                "done" | "failed" | "interrupted" | "cancelled" => terminal = true,
                _ => {}
            }
        }
    }
    page["done"] = json!(terminal);
    page["bytes_done"] = json!(done_bytes);
    page["bytes_total"] = json!(total_bytes);
}

/// 带 MCP 进度通知的 watch：每秒取一页事件，边收边发 notifications/progress。
fn watch_events(
    daemon: &dyn Daemon,
    id: &str,
    since0: i64,
    wait: u64,
    notifier: &mut Notifier,
) -> Result<Value, String> {
    if notifier.token.is_none() || wait == 0 {
        // 没有进度通道时也要告诉调用方「结束了没有」：
        // 只回一页原始事件，agent 无法判断该不该继续轮询（pi 实测连轮 8 次）。
        let mut page = daemon.events(id, since0, wait)?;
        annotate(&mut page);
        return Ok(page);
    }
    let deadline = Instant::now() + Duration::from_millis(wait);
    let mut since = since0;
    let mut all: Vec<Value> = Vec::new();
    let mut last = json!({});
    let mut last_done = 0u64;
    let mut last_total = 0u64;
    // 历史重放可能有几百条 progress；MCP 通知按 500ms 节流，终态不节流。
    let mut last_note = Instant::now() - Duration::from_secs(10);
    let mut terminal = false;
    loop {
        let page = daemon.events(id, since, 1_000)?;
        if let Some(events) = page.get("events").and_then(|x| x.as_array()) {
            for e in events {
                if let Some(seq) = e.get("seq").and_then(|x| x.as_i64()) {
                    since = since.max(seq);
                }
                let kind = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
                let data = e.get("data").unwrap_or(&Value::Null);
                match kind {
                    "progress" => {
                        last_done = data.get("bytes_done").and_then(|x| x.as_u64()).unwrap_or(0);
                        last_total = data.get("bytes_total").and_then(|x| x.as_u64()).unwrap_or(0);
                        if last_note.elapsed() >= Duration::from_millis(500) {
                            let (pct, line) = progress_line(last_done, last_total, "b");
                            notifier.progress(pct, 100, &line);
                            last_note = Instant::now();
                        }
                    }
                    "progress_secs" => {
                        last_done = data.get("seconds_done").and_then(|x| x.as_u64()).unwrap_or(0);
                        last_total = data
                            .get("seconds_total")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                        if last_note.elapsed() >= Duration::from_millis(500) {
                            let (pct, line) = progress_line(last_done, last_total, "s");
                            notifier.progress(pct, 100, &line);
                            last_note = Instant::now();
                        }
                    }
                    "done" => {
                        notifier.progress(100, 100, "skyget 完成");
                        terminal = true;
                    }
                    "failed" | "interrupted" | "cancelled" => {
                        notifier.progress(100, 100, &format!("skyget {kind}"));
                        terminal = true;
                    }
                    _ => {}
                }
                all.push(e.clone());
            }
        }
        last = page;
        if terminal || Instant::now() >= deadline {
            break;
        }
    }
    // 只回最近 20 条：整段历史对 agent 的上下文是纯浪费。
    // 实测（pi 跑一次 1MB 下载）：它靠这个接口轮询了 8 次，上下文冲到 95.7%。
    // agent 真正需要的只有三件事：现在什么状态、还要不要继续等、从哪接着读。
    if all.len() > 20 {
        let cut = all.len() - 20;
        all.drain(0..cut);
    }
    last["events"] = Value::Array(all);
    last["since"] = json!(since);
    last["done"] = json!(terminal);
    last["bytes_done"] = json!(last_done);
    last["bytes_total"] = json!(last_total);
    Ok(last)
}

fn tools_call(
    params: Option<&Value>,
    daemon: &dyn Daemon,
    notifier: &mut Notifier,
) -> Result<Value, (i64, String)> {
    let params = params.ok_or((-32602, "tools/call 缺少 params".to_string()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "tools/call 缺少 name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let out: Result<Value, String> = match name {
        "skyget_check" => {
            let mut body = json!({});
            for k in ["url", "dest", "name", "sha256", "credential_ref", "path"] {
                if let Some(v) = args.get(k).and_then(|x| x.as_str()) {
                    body[k] = json!(v);
                }
            }
            if let Some(src) = args.get("sources").and_then(|v| v.as_array()) {
                body["sources"] = json!(src);
            }
            match daemon.check(&body) {
                Ok(v) => Ok(Value::String(serde_json::to_string(&v).unwrap_or_default())),
                Err(e) => Ok(error_result(e)),
            }
        }
        "skyget_want" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "缺少 url".to_string())
                .map(|s| s.to_string());
            match url {
                Ok(url) => {
                    if let Some(sha) = args.get("sha256").and_then(|v| v.as_str()) {
                        let sha = sha.trim();
                        if !sha.is_empty() && !skyfetch::security::is_sha256_hex(sha) {
                            return Ok(error_result("sha256 必须是 64 位十六进制".into()));
                        }
                    }
                    let has_mirrors = args
                        .get("sources")
                        .and_then(|v| v.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false);
                    let sha_missing = args
                        .get("sha256")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    if has_mirrors && sha_missing {
                        return Ok(error_result(
                            "多源下载必须同时给 sha256（用来判断哪份内容是对的）".into(),
                        ));
                    }
                    let agent = agent_name(&args);
                    let mut body = json!({
                        "url": url,
                        "source": format!("agent:{agent}"),
                    });
                    for key in [
                        "sha256",
                        "dest",
                        "idempotency_key",
                        "accept_change",
                        "sources",
                        "credential_ref",
                    ] {
                        if let Some(v) = args.get(key) {
                            body[key] = v.clone();
                        }
                    }
                    daemon.post_job(&body)
                }
                Err(e) => Err(e),
            }
        }
        "skyget_upload" => {
            let path = args.get("path").and_then(|v| v.as_str());
            let url = args.get("url").and_then(|v| v.as_str());
            match (path, url) {
                (Some(path), Some(url)) if !path.trim().is_empty() && !url.trim().is_empty() => {
                    let protocol = args
                        .get("protocol")
                        .and_then(|v| v.as_str())
                        .unwrap_or("put")
                        .to_ascii_lowercase();
                    if protocol != "put" && protocol != "tus" {
                        return Ok(error_result("protocol 只支持 put / tus".into()));
                    }
                    let mut body = json!({
                        "path": path, "url": url, "protocol": protocol,
                        "source": format!("agent:{}", agent_name(&args)),
                    });
                    for key in ["sha256", "credential_ref"] {
                        if let Some(v) = args.get(key) {
                            body[key] = v.clone();
                        }
                    }
                    if let Some(sha) = args.get("sha256").and_then(|v| v.as_str()) {
                        if !sha.trim().is_empty()
                            && !skyfetch::security::is_sha256_hex(sha.trim())
                        {
                            return Ok(error_result(
                                "sha256 必须是 64 位十六进制".into(),
                            ));
                        }
                    }
                    daemon.post_upload(&body)
                }
                _ => Err("缺少 path / url".to_string()),
            }
        }
        "skyget_watch" => match job_arg(&args) {
            Ok(id) => {
                let since = args.get("since").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
                let wait = args
                    .get("wait_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(25_000)
                    .min(25_000);
                watch_events(daemon, id, since, wait, notifier)
            }
            Err(e) => Err(e),
        },
        "skyget_status" => match job_arg(&args) {
            Ok(id) => daemon.get_job(id),
            Err(e) => Err(e),
        },
        "skyget_cancel" => match job_arg(&args) {
            Ok(id) => daemon.cancel(id),
            Err(e) => Err(e),
        },
        "skyget_retry" => match job_arg(&args) {
            Ok(id) => daemon.retry(id),
            Err(e) => Err(e),
        },
        "skyget_pin" => {
            let sha = args
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "缺少 sha256".to_string());
            match sha {
                Ok(sha) if skyfetch::security::is_sha256_hex(sha) => {
                    let ttl = args
                        .get("ttl_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(7 * 86_400);
                    let note = args.get("note").and_then(|v| v.as_str()).unwrap_or("");
                    daemon.pin(sha, &source_name(&agent_name(&args)), ttl, note)
                }
                Ok(_) => Err("sha256 必须是 64 位十六进制".to_string()),
                Err(e) => Err(e),
            }
        }
        "skyget_unpin" => {
            let sha = args
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "缺少 sha256".to_string());
            match sha {
                Ok(sha) if skyfetch::security::is_sha256_hex(sha) => {
                    let agent = args
                        .get("agent")
                        .and_then(|v| v.as_str())
                        .map(source_name);
                    daemon.unpin(sha, agent.as_deref())
                }
                Ok(_) => Err("sha256 必须是 64 位十六进制".to_string()),
                Err(e) => Err(e),
            }
        }
        "skyget_agents" => daemon.agents(),
        "skyget_active" => {
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            daemon.active(source)
        }
        _ => return Err((-32602, format!("不认识的工具：{name}"))),
    };

    Ok(match out {
        Ok(v) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
            "isError": false,
        }),
        Err(e) => error_result(e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mock {
        posted: Mutex<Option<Value>>,
        events: Mutex<Vec<Value>>,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                posted: Mutex::new(None),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl Daemon for Mock {
        fn check(&self, body: &Value) -> Result<Value, String> {
            *self.posted.lock().unwrap() = Some(body.clone());
            Ok(json!({
                "ok": false, "http_status": 403,
                "code": "policy.egress_denied",
                "error": "策略拒绝：私有地址",
                "next_action": "内网源要 SKYGET_ALLOW_PRIVATE=1",
                "checks": [{"name": "egress", "ok": false, "detail": "127.0.0.1"}],
            }))
        }
        fn post_job(&self, body: &Value) -> Result<Value, String> {
            *self.posted.lock().unwrap() = Some(body.clone());
            Ok(json!({"id": "j1", "state": "queued", "events": "/jobs/j1/events"}))
        }
        fn post_upload(&self, body: &Value) -> Result<Value, String> {
            *self.posted.lock().unwrap() = Some(body.clone());
            Ok(json!({"id": "u1", "state": "queued", "events": "/jobs/u1/events"}))
        }
        fn get_job(&self, id: &str) -> Result<Value, String> {
            Ok(json!({"id": id, "state": "running"}))
        }
        fn events(&self, id: &str, since: i64, wait_ms: u64) -> Result<Value, String> {
            let mut q = self.events.lock().unwrap();
            let evs = if q.is_empty() {
                Vec::new()
            } else {
                vec![q.remove(0)]
            };
            Ok(json!({"job_id": id, "since": since, "wait_ms": wait_ms, "events": evs, "next": since}))
        }
        fn cancel(&self, id: &str) -> Result<Value, String> {
            Ok(json!({"id": id, "state": "cancelled"}))
        }
        fn retry(&self, id: &str) -> Result<Value, String> {
            Ok(json!({"id": id, "state": "queued"}))
        }
        fn pin(&self, sha256: &str, agent: &str, ttl_secs: u64, note: &str) -> Result<Value, String> {
            Ok(json!({
                "sha256": sha256, "agent": agent, "ttl_secs": ttl_secs, "note": note
            }))
        }
        fn unpin(&self, sha256: &str, agent: Option<&str>) -> Result<Value, String> {
            Ok(json!({"sha256": sha256, "agent": agent, "unpinned": true}))
        }
        fn agents(&self) -> Result<Value, String> {
            Ok(json!({"registry": true, "caller": "agent:claude", "agents": []}))
        }
        fn active(&self, source: Option<&str>) -> Result<Value, String> {
            Ok(json!({"job_id": "j1", "state": "running", "pct": 42, "source": source}))
        }
    }

    #[test]
    fn initialize_and_tools_list_are_well_formed() {
        let d = Mock::new();
        let init = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        });
        let out = handle_message(&init, &d).unwrap();
        assert_eq!(out["result"]["protocolVersion"], "2025-06-18");
        assert!(out["result"]["capabilities"]["tools"].is_object());

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let out = handle_message(&list, &d).unwrap();
        let names: Vec<&str> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"skyget_check"));
        assert!(names.contains(&"skyget_want"));
        assert!(names.contains(&"skyget_upload"));
        assert!(names.contains(&"skyget_watch"));
        assert!(names.contains(&"skyget_status"));
        assert!(names.contains(&"skyget_cancel"));
        assert!(names.contains(&"skyget_retry"));
        assert!(names.contains(&"skyget_pin"));
        assert!(names.contains(&"skyget_unpin"));
        assert!(names.contains(&"skyget_agents"));
        assert!(names.contains(&"skyget_active"));

        // 预检的关键：被拒（403）也是**答案**，要带着 checks / next_action 返回，
        // 不能像普通接口那样当异常。
        let call = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "skyget_check", "arguments": {"url": "http://127.0.0.1/x"}}
        });
        let out = handle_message(&call, &d).unwrap();
        assert_eq!(out["result"]["isError"], false, "{out}");
        let text = out["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("policy.egress_denied"), "{text}");
        assert!(text.contains("next_action"), "{text}");
        assert!(text.contains("checks"), "{text}");
        // 请求体确实转过来了
        assert_eq!(d.posted.lock().unwrap().clone().unwrap()["url"], "http://127.0.0.1/x");

        let call = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "skyget_agents", "arguments": {}}
        });
        let out = handle_message(&call, &d).unwrap();
        assert_eq!(out["result"]["isError"], false);

        let active = json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": "skyget_active", "arguments": {"source": "agent:claude"}}
        });
        let out = handle_message(&active, &d).unwrap();
        assert_eq!(out["result"]["isError"], false);
        assert!(out["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent:claude"));
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("agent:claude"), "{text}");
    }

    #[test]
    fn want_sets_agent_identity_and_passes_pin() {
        let d = Mock::new();
        let sha = "ab".repeat(32);
        let call = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "skyget_want", "arguments": {
                "url": "https://example.org/x.iso",
                "sha256": sha,
                "dest": "~/Downloads/",
                "idempotency_key": "run-7",
                "agent": "claude"
            }}
        });
        let out = handle_message(&call, &d).unwrap();
        assert_eq!(out["result"]["isError"], false);
        let posted = d.posted.lock().unwrap().clone().unwrap();
        assert_eq!(posted["source"], "agent:claude");
        assert_eq!(posted["sha256"], sha);
        assert_eq!(posted["idempotency_key"], "run-7");
    }

    #[test]
    fn job_id_path_injection_is_rejected() {
        let d = Mock::new();
        for bad in ["x/../../jobs", "../jobs", "abc"] {
            let call = json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "skyget_status", "arguments": {"job_id": bad}}
            });
            let out = handle_message(&call, &d).unwrap();
            assert_eq!(out["result"]["isError"], true, "job_id {bad} 不该被接受：{out}");
        }
    }

    #[test]
    fn bad_sha_is_rejected_before_it_reaches_the_daemon() {
        let d = Mock::new();
        let call = json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {"name": "skyget_want", "arguments": {
                "url": "https://example.org/x", "sha256": "中"
            }}
        });
        let out = handle_message(&call, &d).unwrap();
        assert_eq!(out["result"]["isError"], true);
        assert!(d.posted.lock().unwrap().is_none(), "畸形 sha 不该发到 daemon");
    }

    #[test]
    fn notifications_get_no_reply() {
        let d = Mock::new();
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&note, &d).is_none());
    }

    #[test]
    fn unknown_tool_is_an_error_result_not_a_crash() {
        let d = Mock::new();
        let call = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "nope", "arguments": {}}
        });
        let out = handle_message(&call, &d).unwrap();
        assert_eq!(out["error"]["code"], -32602);
    }

    #[test]
    fn annotate_says_when_to_stop() {
        // pi 实测的教训：只回一页原始事件，agent 判断不了「结束了没有」，会一直轮。
        let mut page = json!({"events": [
            {"type": "progress", "data": {"bytes_done": 5, "bytes_total": 9}},
            {"type": "done", "data": {}}
        ]});
        annotate(&mut page);
        assert_eq!(page["done"], json!(true));
        assert_eq!(page["bytes_done"], json!(5));
        assert_eq!(page["bytes_total"], json!(9));

        let mut running = json!({"events": [{"type": "progress", "data": {"bytes_done": 1, "bytes_total": 9}}]});
        annotate(&mut running);
        assert_eq!(running["done"], json!(false));
    }

    #[test]
    fn watch_emits_progress_notifications() {
        let d = Mock::new();
        *d.events.lock().unwrap() = vec![
            json!({"seq":1,"type":"progress","data":{"bytes_done":50,"bytes_total":100}}),
            json!({"seq":2,"type":"done","data":{}}),
        ];
        let call = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": "skyget_watch",
                "arguments": {"job_id": "ab".repeat(20), "wait_ms": 5000},
                "_meta": {"progressToken": "tok-1"}
            }
        });
        let mut out = Vec::new();
        let mut notifier = Notifier { out: &mut out, token: None };
        let resp = handle_message_with(&call, &d, &mut notifier).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("notifications/progress"), "{text}");
        assert!(text.contains("\"progress\":50"), "{text}");
        assert!(text.contains("\"progressToken\":\"tok-1\""), "{text}");
        let body = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(body.contains("done"), "{body}");
    }
}
