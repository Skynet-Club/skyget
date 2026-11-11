//! 任务模型。JSON 形状就是给 agent 的契约：改字段等于改 API。

use serde::{Deserialize, Serialize};

/// 传输方向。下载是默认；上传是 0.20 的新面。
pub const DIR_DOWNLOAD: &str = "download";
pub const DIR_UPLOAD: &str = "upload";
/// 上传协议（下载任务为空）。
pub const PROTO_PUT: &str = "put";
pub const PROTO_TUS: &str = "tus";

pub const STATE_PENDING: &str = "pending";
pub const STATE_RUNNING: &str = "running";
pub const STATE_DONE: &str = "done";
pub const STATE_FAILED: &str = "failed";
/// 用户按了暂停：不是故障，已下的分片还在，下次接着下
pub const STATE_INTERRUPTED: &str = "interrupted";

/// 调度优先级。`interactive` = 有人正等着（用户按了按钮），
/// `batch` = 夜间预热/批量补数据，`normal` = 其余。
///
/// 只影响排队顺序，不影响同时跑几个：调度器先挑优先级高的来源，
/// 同优先级仍按「谁跑得少」轮转 —— 公平队列不能被优先级整个吃掉。
pub const PRIORITY_INTERACTIVE: &str = "interactive";
pub const PRIORITY_NORMAL: &str = "normal";
pub const PRIORITY_BATCH: &str = "batch";

/// 优先级 → 调度器用的数字（越大越先跑）；认不出的按 normal。
pub fn priority_rank(s: &str) -> u8 {
    match s {
        PRIORITY_INTERACTIVE => 2,
        PRIORITY_BATCH => 0,
        _ => 1,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Task {
    pub id: String,
    /// `download` / `upload`。老库迁移成 download。
    pub direction: String,
    /// 上传协议：`put` / `tus`；下载为空。
    pub protocol: String,
    /// 上传凭证的**引用**（`env:NAME` / `file:PATH`），不是凭证本身。
    pub credential_ref: String,
    pub url: String,
    /// 多源镜像（不含主 url，或包含亦可去重）。单源时为空。
    pub sources: Vec<String>,
    /// 最终落地路径（完成后才有意义）
    pub dest: String,
    pub state: String,
    /// 已完成字节
    pub bytes_done: u64,
    /// 总字节；服务器没给就是 0（未知）
    pub bytes_total: u64,
    /// 内容的 sha256（完成时计算；也是缓存键）
    pub sha256: String,
    /// 失败原因，人话
    pub error: String,
    /// 机器可读的失败分类：`http.404` / `verify.mismatch` / `part.busy` …
    /// 调用方靠它分支，不要再去解析上面那句人话。
    pub error_code: String,
    /// 这次失败重试有没有意义（网络抖动 / 限流 = true；校验不符 = false）
    pub retryable: bool,
    /// 建议等多久再试（毫秒）；0 = 不建议自动重试
    pub retry_after_ms: u64,
    /// 下一步该做什么，一句话，可直接转述给用户或 agent
    pub next_action: String,
    /// 谁发起的：human / agent:<name>
    pub source: String,
    /// 排队优先级：interactive / normal / batch（空 = normal）
    pub priority: String,
    /// 调用方要的校验值；不符即失败
    pub expected_sha256: String,
    /// 跑这个任务的进程号。进程没了但状态还是 running —— 那是被打断了，不是还在跑。
    pub pid: i64,
    /// 线路：BT 是 已连接 / 发现 的对等端；直链是分段连接数（seen = 0）
    pub links_live: u32,
    pub links_seen: u32,
    /// 这次下载的校验强度到底靠什么：
    /// `pinned` = 调用方给了 sha256；`indexed` = 从独立校验文件取到；
    /// `tofu-new` / `tofu-ok` = 首次记录 / 与历史一致；`torrent` = 种子自证；
    /// `none` = 没校验（诚实标注，不假装）
    pub trust: String,
    /// 运行时开关：地址内容变了也接受（不进台账）
    pub accept_change: bool,
    /// 这一次是秒传：内容本机早有，一字节没走网络。
    /// 落库是为了事后能算「命中率」——不然「省了多少网络」这件事只有当时的人知道。
    pub cached: bool,
    /// 这是「同一任务再来一次」（界面上的重试 / 继续）。
    ///
    /// 不落库，只在这一次运行里有效。它改两件事：
    /// 1. 落地路径不再另起 `(1)`/`(2)` 副本 —— 同一任务重试落回同一个文件；
    /// 2. 允许直接复用台账里记着的 dest（那个文件本来就是这条任务的）。
    pub rerun: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 对外输出（CLI 的 --json、以后 GUI/HTTP 用的是同一份）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    /// 下载还是上传；调用方据此解释 path / url。
    #[serde(default)]
    pub direction: String,
    /// 上传协议；下载为空。
    #[serde(default)]
    pub protocol: String,
    pub url: String,
    /// 这次任务可用的镜像地址（主 url 不一定在列表里）。
    #[serde(default)]
    pub sources: Vec<String>,
    pub state: String,
    pub path: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub sha256: String,
    pub error: String,
    /// 机器可读的失败分类（成功时为空）
    /// 0.19 之前的 daemon 没有这些字段：新客户端要能读旧 daemon 的返回，
    /// 否则启动就会报 "error decoding response body"。
    #[serde(default)]
    pub error_code: String,
    /// 重试有没有意义
    #[serde(default)]
    pub retryable: bool,
    /// 建议等多久再试（毫秒）
    #[serde(default)]
    pub retry_after_ms: u64,
    /// 下一步动作，一句话
    #[serde(default)]
    pub next_action: String,
    pub source: String,
    /// 排队优先级；老 daemon 不回这个字段，按 normal 看
    #[serde(default)]
    pub priority: String,
    pub cached: bool,
    pub elapsed_ms: u64,
    /// pinned / indexed / tofu-new / tofu-ok / torrent / none
    pub trust: String,
    /// 创建时刻（毫秒）：界面按它排序
    pub created_ms: i64,
    /// 线路：已连接 / 发现（BT 用；直链时 seen=0）
    pub links_live: u32,
    pub links_seen: u32,
}

impl Task {
    /// SDK 入口：给地址和目标目录就够了，其余字段都有合理默认。
    ///
    /// `dest` 可以是目录（文件名由服务器给）或完整路径（用它自己的名字）。
    pub fn new(url: &str, dest: &str) -> Self {
        Self {
            id: new_id(url),
            direction: DIR_DOWNLOAD.to_string(),
            protocol: String::new(),
            credential_ref: String::new(),
            url: url.to_string(),
            sources: Vec::new(),
            dest: dest.to_string(),
            state: STATE_PENDING.to_string(),
            bytes_done: 0,
            bytes_total: 0,
            sha256: String::new(),
            error: String::new(),
            error_code: String::new(),
            retryable: false,
            retry_after_ms: 0,
            next_action: String::new(),
            source: "sdk".to_string(),
            priority: PRIORITY_NORMAL.to_string(),
            expected_sha256: String::new(),
            pid: 0,
            links_live: 0,
            links_seen: 0,
            trust: String::new(),
            accept_change: false,
            cached: false,
            rerun: false,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// 链式设置：`Task::new(u, d).expecting(sha)` / `.from_agent("hermes")`
    pub fn expecting(mut self, sha256: &str) -> Self {
        self.expected_sha256 = sha256.to_ascii_lowercase();
        self.trust = "pinned".into();
        self
    }

    pub fn from_agent(mut self, name: &str) -> Self {
        self.source = format!("agent:{name}");
        self
    }

    /// 台账里可能留着「running 但进程早没了」的记录（被 kill、断电）。
    /// 显示时如实说成 interrupted，而不是让人看着一个永远不动的进度。
    pub fn state_for_display(&self) -> String {
        if self.state == STATE_RUNNING && self.pid > 0 && !pid_alive(self.pid) {
            return "interrupted".to_string();
        }
        self.state.clone()
    }

    pub fn view(&self, cached: bool, elapsed_ms: u64) -> TaskView {
        let state = self.state_for_display();
        TaskView {
            id: self.id.clone(),
            direction: self.direction.clone(),
            protocol: self.protocol.clone(),
            // 预签名 URL 里的签名/token 是凭证：台账内部保留真实值供重试，
            // 但所有对外视图（API / CLI --json / GUI / MCP）只给脱敏后的形状。
            url: crate::security::redact_url(&self.url),
            sources: self
                .sources
                .iter()
                .map(|s| crate::security::redact_url(s))
                .collect(),
            // 契约：path 只在 done 时有意义。pending/running 的 dest 可能是目录，
            // failed 的文件可能根本没落地 —— 报出去会让调用方以为拿到了成品。
            path: if state == STATE_DONE {
                self.dest.clone()
            } else {
                String::new()
            },
            state,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            sha256: self.sha256.clone(),
            error: crate::security::redact_text(&self.error),
            error_code: self.error_code.clone(),
            retryable: self.retryable,
            retry_after_ms: self.retry_after_ms,
            next_action: self.next_action.clone(),
            source: self.source.clone(),
            priority: if self.priority.is_empty() {
                PRIORITY_NORMAL.to_string()
            } else {
                self.priority.clone()
            },
            cached,
            elapsed_ms,
            trust: self.trust.clone(),
            created_ms: self.created_at,
            links_live: self.links_live,
            links_seen: self.links_seen,
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == STATE_DONE
    }
}

/// 新任务 id：时间有序 + pid + 进程内序号 + 48 位随机，40 个十六进制字符。
///
/// 以前是「毫秒时间戳 + URL hash 的前 2 字节」：同一毫秒提交多个任务时只有
/// 65536 种取值，agent 一重试风暴就开始撞 id；而且 id 里混着 URL 指纹，
/// 让「同一 URL 两次提交」看起来像同一条任务，其实不是。id 只负责唯一，
/// 幂等由单独的幂等键负责 —— 两件事不要挤在一个字段里。
pub fn new_id(_url: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut rnd = [0u8; 6];
    let _ = getrandom::getrandom(&mut rnd);
    let r = u64::from_be_bytes([0, 0, rnd[0], rnd[1], rnd[2], rnd[3], rnd[4], rnd[5]]);
    format!(
        "{:012x}{:08x}{:08x}{:012x}",
        stamp & 0xffff_ffff_ffff,
        std::process::id(),
        seq,
        r
    )
}

fn pid_alive(pid: i64) -> bool {
    crate::sys::pid_alive(pid)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{new_id, Task, TaskView, STATE_DONE};
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_at_agent_retry_rates() {
        // agent 重试风暴下最容易暴露：同一毫秒里连续生成。
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(new_id("https://example.org/x")));
        }
        assert_eq!(seen.len(), 10_000);
    }

    #[test]
    fn view_only_reports_a_path_when_the_file_is_actually_done() {
        let mut t = Task::new("https://example.org/x", "/tmp/out/");
        let v = t.view(false, 0);
        assert_eq!(v.path, "", "没完成时不能报目录当成品");
        t.state = STATE_DONE.to_string();
        let v = t.view(true, 0);
        assert_eq!(v.path, "/tmp/out/");
    }

    #[test]
    fn old_daemon_taskview_without_new_fields_still_decodes() {
        // 0.18 daemon 的真实形状：没有 direction/protocol/sources/error_code/
        // retryable/retry_after_ms/next_action。新 GUI/CLI 必须还能读，
        // 否则启动就报 "error decoding response body"。
        let old = r#"{
            "id":"1a167ff9398e580","url":"https://example.org/x","state":"done",
            "path":"/tmp/x","bytes_done":1,"bytes_total":1,"sha256":"abc",
            "error":"","source":"human","cached":false,"elapsed_ms":0,
            "trust":"tofu-new","created_ms":1,"links_live":0,"links_seen":0
        }"#;
        let v: TaskView = serde_json::from_str(old).expect("旧 daemon 的 TaskView 必须能解码");
        assert_eq!(v.direction, "");
        assert_eq!(v.protocol, "");
        assert!(v.sources.is_empty());
        assert_eq!(v.error_code, "");
        assert!(!v.retryable);
        assert_eq!(v.retry_after_ms, 0);
        assert_eq!(v.next_action, "");
    }

    #[test]
    fn id_shape_is_stable_and_sortable_by_time_prefix() {
        let id = new_id("https://example.org/x");
        assert_eq!(id.len(), 40, "id 形状变了等于改了下游的解析：{id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let a = new_id("a");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id("b");
        assert!(a[..12] <= b[..12], "时间前缀要能排序");
    }
}
