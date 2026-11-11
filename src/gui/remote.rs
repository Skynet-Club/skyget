//! 跟守护进程说话的那一层。
//!
//! 界面自己不下载 —— 它只是守护进程的一个前端。这样才能做到「关掉窗口，下载继续」，
//! 也才能在界面崩了以后重开还看得见进度。

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use egui::Context;

use super::Msg;

/// 守护进程地址。默认 7788；换端口用 `SKYGET_API=http://127.0.0.1:7799`，
/// 或者 `SKYGET_API=unix:/path/daemon.sock` 走 Unix socket（界面就不用配 token）。
pub fn base() -> String {
    crate::uds::base_url()
}

fn client() -> reqwest::blocking::Client {
    // daemon 设了 SKYGET_API_TOKEN 时，界面也得带上同一个 token。
    // 没设就保持旧行为：本机回环、无鉴权。
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(t) = std::env::var("SKYGET_API_TOKEN") {
        let t = t.trim();
        if !t.is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
    }
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .default_headers(headers)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// 守护进程活着吗。顺带把「为什么连不上」带回来，界面能直说。
pub fn health(base: &str) -> Result<String, String> {
    match client().get(format!("{base}/health")).send() {
        Ok(r) if r.status().is_success() => Ok(r.text().unwrap_or_default()),
        Ok(r) => Err(format!("HTTP {}", r.status().as_u16())),
        Err(e) => Err(format!("{e}")),
    }
}

/// 没在跑就把它拉起来。
///
/// 找可执行文件的顺序：新名 `skynet-dataplaned` → 旧名 `skynet-downloadd`。
/// 起不来不是致命错误：界面上会说「没连上，先把它起起来」，控制按钮变灰。
pub fn ensure_daemon(base: &str) {
    // 和 CLI / MCP 共用同一份实现：先 systemd、再直启，返回值说明是谁拉起来的。
    if let crate::daemon::Ensured::Failed(why) = crate::daemon::ensure(base) {
        eprintln!("守护进程起不来：{why}");
    }
}

/// 一条命令。
#[derive(Clone, Debug)]
pub enum Cmd {
    Submit {
        /// 界面先插了一行占位（`new_id` 猜的 id）。守护进程会给真 id，
        /// 拿到之后要把那一行改名，否则列表里会永远挂着一个「排队」的幽灵。
        temp_id: String,
        url: String,
        dest: String,
        sha256: String,
        source: String,
    },
    Pause(String),
    Resume(String),
    Cancel(String),
    Delete(String),
}

/// 一个线程干两件事：把命令发出去、每 700ms 把台账拉回来。
///
/// 不并发是故意的：界面上的操作都是「点一下」，命令本身都很短（提交立刻回 202），
/// 排队执行反而让「点了取消又被轮询覆盖」这类竞态无从发生。
pub fn spawn(cmd_rx: Receiver<Cmd>, tx: Sender<Msg>, ctx: Context, base: String) {
    std::thread::spawn(move || {
        // 先确保守护进程在（没在就先 systemctl，再退而求其次自己拉起）。
        // 放在这个线程里，窗口不用等它。
        ensure_daemon(&base);
        // 新界面连旧 daemon 时，字段可能解释不了。提前说清楚“版本不兼容、
        // 去重启/升级 daemon”，不要把 reqwest 的 decode 错误直接甩给用户。
        if let Ok(body) = health(&base) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(dv) = v["version"].as_str() {
                    if major_minor(dv) != major_minor(env!("CARGO_PKG_VERSION")) {
                        let _ = tx.send(Msg::RemoteErr(format!(
                            "守护进程版本 {dv} 与界面 {} 不兼容；请重启或升级 skynet-dataplaned。",
                            env!("CARGO_PKG_VERSION")
                        )));
                        ctx.request_repaint();
                    }
                }
            }
        }
        let c = client();
        let mut was_ok = true;
        // 增量游标：只拉上次之后变过的。断线重连就归零，重来一次全量。
        let mut since: i64 = 0;
        // 机群视图：agent 身份/策略变化很慢，2 秒一次就够，不跟 700ms 的任务轮询同频。
        let mut last_fleet = std::time::Instant::now() - Duration::from_secs(10);
        loop {
            // 1) 命令
            while let Ok(cmd) = cmd_rx.try_recv() {
                if let Err(e) = run_cmd(&c, &base, &cmd, &tx) {
                    let _ = tx.send(Msg::RemoteErr(e));
                    ctx.request_repaint();
                }
            }
            // 2) 拉台账（增量：只有变过的和删掉的）
            match c
                .get(format!("{base}/jobs/changes?since={since}"))
                .send()
            {
                Ok(r) if r.status().is_success() => {
                    match r.json::<Changes>() {
                        Ok(ch) => {
                            if !was_ok {
                                was_ok = true;
                                let _ = tx.send(Msg::Link {
                                    ok: true,
                                    why: String::new(),
                                });
                            }
                            since = ch.now.max(since);
                            // 没变化就什么都别做：以前这里无条件 request_repaint，
                            // 结果是「什么都没发生也每秒重画 1.4 次」，空闲 CPU 白烧 1%。
                            // 界面空闲时应该真的一帧不画。
                            if !ch.items.is_empty() || !ch.deleted.is_empty() {
                                let _ = tx.send(Msg::Changes {
                                    items: ch.items,
                                    deleted: ch.deleted,
                                });
                                ctx.request_repaint();
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Link {
                                ok: false,
                                why: format!(
                                    "守护进程返回格式不兼容（{e}）。界面 {}；请重启或升级 skynet-dataplaned。",
                                    env!("CARGO_PKG_VERSION")
                                ),
                            });
                            was_ok = false;
                            since = 0;
                        }
                    }
                }
                Ok(r) => {
                    was_ok = false;
                    let _ = tx.send(Msg::Link {
                        ok: false,
                        why: format!("HTTP {}", r.status().as_u16()),
                    });
                }
                Err(e) => {
                    since = 0; // 下次连上先来一次全量，补上断线期间发生的事
                    if was_ok {
                        was_ok = false;
                        let _ = tx.send(Msg::Link {
                            ok: false,
                            why: format!("连不上守护进程（{}）。起一个：skynet-dataplaned（旧名 skynet-downloadd）", short(&e.to_string())),
                        });
                        ctx.request_repaint();
                    }
                }
            }
            // 3) 机群（agent 身份 / 策略 / 并发）。失败不清空已有快照：
            //    守护进程短暂重启时，界面不该整块变空。
            if last_fleet.elapsed() >= Duration::from_secs(2) {
                last_fleet = std::time::Instant::now();
                if let Ok(r) = c.get(format!("{base}/agents")).send() {
                    if r.status().is_success() {
                        if let Ok(v) = r.json::<AgentsView>() {
                            let _ = tx.send(Msg::Fleet(Some(v)));
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(700));
        }
    });
}

fn run_cmd(
    c: &reqwest::blocking::Client,
    base: &str,
    cmd: &Cmd,
    tx: &Sender<Msg>,
) -> Result<(), String> {
    // 提交要单独走一遍：它得把守护进程给的真 id 报回去，占位那行才能改名
    if let Cmd::Submit {
        temp_id,
        url,
        dest,
        sha256,
        source,
    } = cmd
    {
        let body = serde_json::json!({"url": url, "dest": dest, "sha256": sha256, "source": source});
        let sent = c.post(format!("{base}/jobs")).json(&body).send();
        let r = match sent {
            Ok(r) => r,
            Err(e) => {
                let why = format!("提交失败：{e}");
                let _ = tx.send(Msg::SubmitFailed {
                    temp: temp_id.clone(),
                    why: why.clone(),
                });
                return Err(why);
            }
        };
        let code = r.status().as_u16();
        let txt = r.text().unwrap_or_default();
        if !(200..=299).contains(&code) {
            let why = format!("提交失败（HTTP {code}）：{}", short(&txt));
            let _ = tx.send(Msg::SubmitFailed {
                temp: temp_id.clone(),
                why: why.clone(),
            });
            return Err(why);
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let _ = tx.send(Msg::Submitted {
                    temp: temp_id.clone(),
                    real: id.to_string(),
                });
            }
        }
        return Ok(());
    }

    let (url, body) = match cmd {
        // 上面已经 return 了；这里只是为了 match 穷尽
        Cmd::Submit { .. } => unreachable!(),
        Cmd::Pause(id) => (format!("{base}/jobs/{id}/pause"), serde_json::json!({})),
        Cmd::Resume(id) => (format!("{base}/jobs/{id}/resume"), serde_json::json!({})),
        Cmd::Cancel(id) => (format!("{base}/jobs/{id}/cancel"), serde_json::json!({})),
        Cmd::Delete(id) => {
            let r = c.delete(format!("{base}/jobs/{id}")).send().map_err(|e| format!("{e}"))?;
            return match r.status().as_u16() {
                200..=299 => Ok(()),
                404 => Ok(()), // 已经没了，等于做到
                code => Err(format!("删不掉（HTTP {code}）")),
            };
        }
    };
    let r = c.post(url).json(&body).send().map_err(|e| format!("{e}"))?;
    match r.status().as_u16() {
        200..=299 => Ok(()),
        404 => Err("这条已经不在台账里了".to_string()),
        409 => Err("这条不是守护进程在跑，停不了它（可能是命令行起的）".to_string()),
        code => Err(format!(
            "HTTP {code}：{}",
            short(&r.text().unwrap_or_default())
        )),
    }
}

/// `GET /agents` 的返回。字段是接口契约的一部分（docs/api-v1.md §7.6）。
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AgentsView {
    pub registry: bool,
    pub caller: Option<String>,
    pub scheduler: SchedulerView,
    pub agents: Vec<AgentInfo>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct SchedulerView {
    pub max_running: usize,
    pub max_queued: usize,
    pub running: usize,
    pub queued: usize,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub source: String,
    pub jobs: u64,
    pub running: usize,
    pub max_running: usize,
    pub policy: AgentPolicyView,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct AgentPolicyView {
    #[serde(default)]
    pub dest_roots: Vec<String>,
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,
    #[serde(default)]
    pub upload_roots: Vec<String>,
    #[serde(default)]
    pub allow_private: bool,
}

/// `/jobs/changes?since=` 的返回
#[derive(serde::Deserialize)]
struct Changes {
    now: i64,
    items: Vec<crate::model::TaskView>,
    deleted: Vec<String>,
}

/// `0.21.1` → `0.21`：patch 不同不提示，major/minor 不同才警告。
fn major_minor(v: &str) -> &str {
    v.rsplit_once('.').map(|(a, _)| a).unwrap_or(v)
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s.to_string()
    }
}
