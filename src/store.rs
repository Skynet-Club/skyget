//! 任务台账：SQLite。谁、什么时候、取了什么、hash、落地到哪 —— 一条不落。
//!
//! 这是给人类看的那一面（也回答「agent 到底在我机器上下过什么」），
//! 同时是 CLI 的 list/status 的数据源。

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::model::{Task, STATE_DONE};
use crate::paths;

/// 一列不改地读回一条任务。以前每条 SELECT 都手抄一遍列名，
/// 加列时漏一条就是「有的查询读出来字段全空」—— 这种 bug 只有真跑才看得见。
/// 列顺序必须与 `row_to_task` 的下标一致。
const COLS: &str = "id,url,dest,state,bytes_done,bytes_total,sha256,error,source,expected_sha256,created_at,updated_at,pid,links_live,links_seen,trust,cached,error_code,retryable,retry_after_ms,next_action,direction,protocol,credential_ref,sources,priority";

/// 一条事件。任务台账回答"现在是什么状态"，事件回答"怎么走到这一步的"。
/// agent 断线重连、人复盘、审计导出都靠它，所以它是 append-only 的。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub ts: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: serde_json::Value,
}

/// 一条缓存 pin。`expires_at=0` = 不过期；`agent` 为空 = 全局 pin。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Pin {
    pub sha256: String,
    pub agent: String,
    pub note: String,
    pub created_at: i64,
    pub expires_at: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open() -> Result<Self> {
        paths::ensure_dirs().context("创建数据目录失败")?;
        Self::open_at(&paths::db_path())
    }

    /// 指定台账文件打开。给测试和「便携模式/多实例」用，不碰全局 SKYGET_HOME。
    pub fn open_at(db: &Path) -> Result<Self> {
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).context("创建台账目录失败")?;
        }
        let conn = Connection::open(db).context("打开任务台账失败")?;
        // 界面上三个下载线程 + 主线程各开一条连接，同时写会锁库。
        // WAL 让读写不互相挡，busy_timeout 兜住写写冲突。
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        // 老台账没有 cached 这一列：补上。重复执行会报 duplicate column，忽略即可。
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN cached INTEGER NOT NULL DEFAULT 0",
            [],
        );
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                dest TEXT NOT NULL,
                state TEXT NOT NULL,
                bytes_done INTEGER NOT NULL DEFAULT 0,
                bytes_total INTEGER NOT NULL DEFAULT 0,
                sha256 TEXT NOT NULL DEFAULT '',
                error TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT 'human',
                expected_sha256 TEXT NOT NULL DEFAULT '',
                pid INTEGER NOT NULL DEFAULT 0,
                links_live INTEGER NOT NULL DEFAULT 0,
                links_seen INTEGER NOT NULL DEFAULT 0,
                trust TEXT NOT NULL DEFAULT '',
                cached INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                error_code TEXT NOT NULL DEFAULT '',
                retryable INTEGER NOT NULL DEFAULT 0,
                retry_after_ms INTEGER NOT NULL DEFAULT 0,
                next_action TEXT NOT NULL DEFAULT '',
                direction TEXT NOT NULL DEFAULT 'download',
                protocol TEXT NOT NULL DEFAULT '',
                credential_ref TEXT NOT NULL DEFAULT '',
                sources TEXT NOT NULL DEFAULT '[]',
                priority TEXT NOT NULL DEFAULT 'normal'
            );
            CREATE INDEX IF NOT EXISTS tasks_sha ON tasks(sha256);
            CREATE INDEX IF NOT EXISTS tasks_url ON tasks(url);
            -- 状态栏每秒查活跃任务；配额每次提交都要按来源/状态/时间求和。
            -- 没有索引时这两类查询会全表扫，台账大了以后轮询会拖慢 daemon。
            CREATE INDEX IF NOT EXISTS tasks_state_created ON tasks(state, created_at);
            CREATE INDEX IF NOT EXISTS tasks_usage ON tasks(source, state, updated_at);
            -- 幂等键 → 任务 id。agent 重试、脚本重放、用户连点，
            -- 同一个意图只能落成一条任务；键由调用方给，或由 (url,dest,sha) 自然生成。
            CREATE TABLE IF NOT EXISTS idempotency (
                key TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idem_job ON idempotency(job_id);
            -- 事件流：每个任务自己的 seq，断线重连带 since 就能接着读。
            CREATE TABLE IF NOT EXISTS events (
                job_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                ts INTEGER NOT NULL,
                type TEXT NOT NULL,
                data TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY (job_id, seq)
            );
            CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
            -- 缓存 pin：agent 说「这份内容我在用」。prune 必须绕开它。
            -- expires_at=0 表示不自动过期；否则是毫秒时间戳。
            CREATE TABLE IF NOT EXISTS pins (
                sha256 TEXT NOT NULL,
                agent TEXT NOT NULL DEFAULT '',
                note TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (sha256, agent)
            );
            CREATE INDEX IF NOT EXISTS pins_sha ON pins(sha256);
            -- 删除墓碑：客户端用 /jobs/changes?since= 增量同步时，
            -- 只靠 tasks 表看不见“已经删掉的任务”。墓碑落库，daemon 重启后
            -- 删除仍然能被 since>0 的客户端看到。保留期由维护线程清理。
            CREATE TABLE IF NOT EXISTS tombstones (
                id TEXT PRIMARY KEY,
                deleted_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS tombstones_ts ON tombstones(deleted_at);",
        )
        .context("初始化台账表失败")?;
        // 老库没有 pid 列：补上，补不了说明已经有了
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN pid INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN trust TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'",
            [],
        );
        for col in ["links_live", "links_seen"] {
            let _ = conn.execute(
                &format!("ALTER TABLE tasks ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0"),
                [],
            );
        }
        // 0.19：机器可读的失败分类。老库补列，历史记录当作「没分类」。
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN error_code TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN retryable INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN retry_after_ms INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN next_action TEXT NOT NULL DEFAULT ''",
            [],
        );
        // 0.20：上传方向。老库一律是 download。
        for (col, default) in [
            ("direction", "'download'"),
            ("protocol", "''"),
            ("credential_ref", "''"),
            ("sources", "'[]'"),
        ] {
            let _ = conn.execute(
                &format!(
                    "ALTER TABLE tasks ADD COLUMN {col} TEXT NOT NULL DEFAULT {default}"
                ),
                [],
            );
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn save(&self, t: &Task) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO tasks (id,url,dest,state,bytes_done,bytes_total,sha256,error,source,expected_sha256,created_at,updated_at,pid,links_live,links_seen,trust,cached,error_code,retryable,retry_after_ms,next_action,direction,protocol,credential_ref,sources,priority)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)
             ON CONFLICT(id) DO UPDATE SET
               dest=excluded.dest, state=excluded.state,
               bytes_done=excluded.bytes_done, bytes_total=excluded.bytes_total,
               sha256=excluded.sha256, error=excluded.error, updated_at=excluded.updated_at,
               pid=excluded.pid,
               links_live=excluded.links_live, links_seen=excluded.links_seen,
               trust=excluded.trust, cached=excluded.cached,
               error_code=excluded.error_code, retryable=excluded.retryable,
               retry_after_ms=excluded.retry_after_ms, next_action=excluded.next_action,
               direction=excluded.direction, protocol=excluded.protocol,
               credential_ref=excluded.credential_ref, sources=excluded.sources,
               priority=excluded.priority",
            params![
                t.id,
                t.url,
                t.dest,
                t.state,
                t.bytes_done as i64,
                t.bytes_total as i64,
                t.sha256,
                t.error,
                t.source,
                t.expected_sha256,
                t.created_at,
                t.updated_at,
                t.pid,
                t.links_live as i64,
                t.links_seen as i64,
                t.trust,
                t.cached as i64,
                t.error_code,
                t.retryable as i64,
                t.retry_after_ms as i64,
                t.next_action,
                t.direction,
                t.protocol,
                t.credential_ref,
                serde_json::to_string(&t.sources).unwrap_or_else(|_| "[]".to_string()),
                t.priority,
            ],
        )
        .context("写台账失败")?;
        Ok(())
    }

    /// 只更新已存在的行，**绝不插入**。
    ///
    /// 终态写库用这个：取消/删除可能已经发生在另一个进程里，
    /// 普通 `save` 的 INSERT .. ON CONFLICT 会把被删的任务重新写回来（幽灵复活）。
    /// 返回 false = 这行已经不在了，调用方应把这次运行当成被取消。
    pub fn save_if_exists(&self, t: &Task) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE tasks SET
               dest=?2, state=?3, bytes_done=?4, bytes_total=?5,
               sha256=?6, error=?7, updated_at=?8, pid=?9,
               links_live=?10, links_seen=?11, trust=?12, cached=?13,
               error_code=?14, retryable=?15, retry_after_ms=?16, next_action=?17,
               direction=?18, protocol=?19, credential_ref=?20, sources=?21,
               priority=?22
             WHERE id=?1",
            params![
                t.id,
                t.dest,
                t.state,
                t.bytes_done as i64,
                t.bytes_total as i64,
                t.sha256,
                t.error,
                t.updated_at,
                t.pid,
                t.links_live as i64,
                t.links_seen as i64,
                t.trust,
                t.cached as i64,
                t.error_code,
                t.retryable as i64,
                t.retry_after_ms as i64,
                t.next_action,
                t.direction,
                t.protocol,
                t.credential_ref,
                serde_json::to_string(&t.sources).unwrap_or_else(|_| "[]".to_string()),
                t.priority,
            ],
        )
        .context("写台账失败")?;
        Ok(n > 0)
    }

    /// 只更新进度，不碰状态 —— 给「跑着的任务让外面看得见」用。
    pub fn save_progress(&self, id: &str, bytes: u64, total: u64, live: u32, seen: u32) -> Result<()> {
        let c = self.conn.lock().unwrap();
        // 总大小也要写：只写已下字节的话，界面拿到 total=0，进度条永远是空的。
        // 线路同理 —— 不落库的话，界面看到的是「—」而不是「12/86」。
        let _ = c.execute(
            // 数字没变就别写：以前每次心跳都无条件写一遍 updated_at，
            // 一个卡住不动的任务会因此每 900ms 「变动」一次 —— 界面的增量轮询
            // 就以为有事发生、一直重画（实测空闲 2.2 fps 全是这种假变动）。
            // 数字真变了才写，界面才相信「变了」这个词。
            "UPDATE tasks SET bytes_done = ?2, bytes_total = max(bytes_total, ?3),
                 links_live = ?5, links_seen = ?6, updated_at = ?4
             WHERE id = ?1 AND state = 'running'
               AND (bytes_done <> ?2 OR links_live <> ?5 OR links_seen <> ?6 OR bytes_total < ?3)",
            params![
                id,
                bytes as i64,
                total as i64,
                crate::model::now_ms(),
                live as i64,
                seen as i64
            ],
        );
        Ok(())
    }

    /// 从台账里删掉一条（取消下载、或者用户手动删记录）。
    ///
    /// **只删记录，不动盘上的文件** —— 已落地的东西是用户的，不是我们的。
    /// 删除和墓碑写在同一个事务里：客户端靠 /jobs/changes 增量同步，
    /// 墓碑没落库（或只落在内存）的话，daemon 一重启删除就对它们不可见了。
    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let n = tx.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        // 任务没了，幂等键也要释放：同一条意图以后还能重新提交。
        // 不释放的话，取消/删除之后再提交会拿到一个不存在的 job id。
        let _ = tx.execute("DELETE FROM idempotency WHERE job_id = ?1", params![id]);
        if n > 0 {
            tx.execute(
                "INSERT INTO tombstones (id, deleted_at) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET deleted_at = excluded.deleted_at",
                params![id, crate::model::now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// 删除墓碑（`since` 排他，与事件 seq 的语义一致）。
    pub fn tombstones_since(&self, since: i64, limit: usize) -> Result<Vec<(String, i64)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, deleted_at FROM tombstones
             WHERE deleted_at > ?1
             ORDER BY deleted_at ASC, id ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![since, limit.clamp(1, 1000) as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push((r.get(0)?, r.get(1)?));
        }
        Ok(out)
    }

    pub fn tombstone_count(&self) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT COUNT(*) FROM tombstones", [], |r| r.get(0))?)
    }

    pub fn prune_tombstones(&self, older_than_ms: i64) -> Result<usize> {
        let cutoff = crate::model::now_ms().saturating_sub(older_than_ms.max(0));
        let c = self.conn.lock().unwrap();
        Ok(c.execute("DELETE FROM tombstones WHERE deleted_at < ?1", params![cutoff])?)
    }

    /// 原子地占一个幂等键。返回 true = 这次由我占住；false = 已经被别人占了。
    ///
    /// 用 `INSERT OR IGNORE` 而不是「先查再写」：daemon 每个请求一个线程，
    /// 两个 agent 同时重试时，先查再写会让两边都以为自己能提交。
    pub fn claim_idem(&self, key: &str, job_id: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "INSERT OR IGNORE INTO idempotency (key, job_id, created_at) VALUES (?1, ?2, ?3)",
            params![key, job_id, crate::model::now_ms()],
        )?;
        Ok(n > 0)
    }

    /// 幂等键对应的任务；任务已经被删掉时返回 None（键是陈旧的）。
    pub fn find_idem(&self, key: &str) -> Result<Option<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS} FROM tasks
             WHERE id = (SELECT job_id FROM idempotency WHERE key = ?1)"
        ))?;
        let mut rows = stmt.query(params![key])?;
        Ok(rows.next()?.map(row_to_task))
    }

    /// 清掉一个陈旧的幂等键（对应的任务已经不在了）。
    pub fn drop_idem(&self, key: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM idempotency WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// 追加一条事件；seq 在每个任务内连续，从 1 开始。
    ///
    /// 单条 `INSERT ... SELECT MAX(seq)+1` 是原子的：SQLite 的写锁保证
    /// 两个线程不会拿到同一个 seq，不需要先查再写。
    pub fn append_event(
        &self,
        job_id: &str,
        kind: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        // 事件是给别的 agent/界面看的：错误文本里的签名 URL 也要脱敏。
        // 真实 URL 只留在 tasks.url（重试要用），不进事件流。
        let safe = crate::security::redact_json(data);
        c.execute(
            "INSERT INTO events (job_id, seq, ts, type, data)
             SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4
             FROM events WHERE job_id = ?1",
            params![
                job_id,
                crate::model::now_ms(),
                kind,
                safe.to_string(),
            ],
        )
        .context("写事件失败")?;
        let seq: i64 = c.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )?;
        Ok(seq)
    }

    /// 读某个任务的事件；`since` 是上次读到的 seq（0 = 从头）。
    pub fn events_since(
        &self,
        job_id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT seq, ts, type, data FROM events
             WHERE job_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![job_id, since, limit.clamp(1, 1000) as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let ts: i64 = r.get(1)?;
            let kind: String = r.get(2)?;
            let raw: String = r.get(3).unwrap_or_else(|_| "{}".to_string());
            let data =
                serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
            out.push(EventRow { seq, ts, kind, data });
        }
        Ok(out)
    }

    /// 台账里每个来源有多少条任务。`/agents` 展示用，不扫全表。
    pub fn source_counts(&self) -> Result<Vec<(String, i64)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT source, COUNT(*) FROM tasks GROUP BY source ORDER BY COUNT(*) DESC, source",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push((r.get(0)?, r.get(1)?));
        }
        Ok(out)
    }

    pub fn event_count(&self) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(n)
    }

    /// 清掉旧的进度类事件（状态/错误/审计事件保留）。
    /// 每秒一条 progress 不清理的话，事件表会无限长；状态事件很少，留着复盘。
    pub fn prune_progress_events(&self, older_than_ms: i64) -> Result<usize> {
        let cutoff = crate::model::now_ms().saturating_sub(older_than_ms.max(0));
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "DELETE FROM events
             WHERE ts < ?1 AND type IN ('progress','progress_secs','links')",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// 更长的保留期：状态/错误/审计事件最终也要走，否则跑几个月的 daemon
    /// 事件表只增不减。任务行本身仍有最终状态，删除后的墓碑也还在。
    pub fn prune_old_events(&self, older_than_ms: i64) -> Result<usize> {
        let cutoff = crate::model::now_ms().saturating_sub(older_than_ms.max(0));
        let c = self.conn.lock().unwrap();
        Ok(c.execute("DELETE FROM events WHERE ts < ?1", params![cutoff])?)
    }

    pub fn last_event_seq(&self, job_id: &str) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        let n: i64 = c.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 钉住一份内容。`ttl_secs=0` = 不过期；重复 pin 是更新，不是报错。
    pub fn pin(&self, sha: &str, agent: &str, ttl_secs: u64, note: &str) -> Result<Pin> {
        let sha = sha.trim().to_ascii_lowercase();
        let agent = agent.trim();
        let now = crate::model::now_ms();
        let expires = if ttl_secs == 0 {
            0
        } else {
            now.saturating_add((ttl_secs as i64).saturating_mul(1000))
        };
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO pins (sha256, agent, note, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(sha256, agent) DO UPDATE SET
               note=excluded.note, created_at=excluded.created_at,
               expires_at=excluded.expires_at",
            params![sha, agent, note, now, expires],
        )
        .context("写 pin 失败")?;
        Ok(Pin {
            sha256: sha.to_string(),
            agent: agent.to_string(),
            note: note.to_string(),
            created_at: now,
            expires_at: expires,
        })
    }

    pub fn unpin(&self, sha: &str, agent: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "DELETE FROM pins WHERE sha256 = ?1 AND agent = ?2",
            params![sha.trim().to_ascii_lowercase(), agent.trim()],
        )?;
        Ok(n > 0)
    }

    /// 去掉某个 sha 的所有 pin（不指定 agent 时用）。
    pub fn unpin_all(&self, sha: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "DELETE FROM pins WHERE sha256 = ?1",
            params![sha.trim().to_ascii_lowercase()],
        )?;
        Ok(n > 0)
    }

    /// 所有 pin（含已过期的；调用方按 `expires_at` 过滤或展示）。
    pub fn pins(&self) -> Result<Vec<Pin>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT sha256, agent, note, created_at, expires_at
             FROM pins ORDER BY created_at DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(Pin {
                sha256: r.get(0)?,
                agent: r.get(1)?,
                note: r.get(2)?,
                created_at: r.get(3)?,
                expires_at: r.get(4)?,
            });
        }
        Ok(out)
    }

    /// 还没过期的 pin 对应的 sha 集合；prune 用它保护缓存块。
    pub fn pinned_shas(&self) -> Result<std::collections::HashSet<String>> {
        let now = crate::model::now_ms();
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT DISTINCT sha256 FROM pins
             WHERE expires_at = 0 OR expires_at > ?1",
        )?;
        let mut rows = stmt.query(params![now])?;
        let mut out = std::collections::HashSet::new();
        while let Some(r) = rows.next()? {
            out.insert(r.get::<_, String>(0)?);
        }
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE id = ?1"
        ))?;
        let mut rows = stmt.query(params![id])?;
        Ok(rows.next()?.map(row_to_task))
    }

    /// 只取活跃任务（running/pending/interrupted）。状态栏不能只看“最近 500 条”：
    /// 一条跑了几小时的老任务会被后来几百条短任务挤出窗口，界面就误报空闲。
    pub fn active_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS} FROM tasks
             WHERE state IN (?1, ?2, ?3)
             ORDER BY created_at DESC LIMIT ?4"
        ))?;
        let mut rows = stmt.query(params![
            crate::model::STATE_RUNNING,
            crate::model::STATE_PENDING,
            crate::model::STATE_INTERRUPTED,
            limit as i64
        ])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_task(r));
        }
        Ok(out)
    }

    /// 台账的一页 + 匹配总数。
    ///
    /// 过滤必须在 SQL 里做：先取 20 条再在 Rust 里筛，`--mine -l 20` 可能只剩 3 条，
    /// 而真正要看的那 20 条躺在后面 —— 用户看到的是「台账里只有 3 条」。
    /// 同时回一个总数，CLI 才能诚实地写「还有 37 条」。
    pub fn list_page(
        &self,
        limit: usize,
        source: Option<&str>,
        mine: bool,
        agents: bool,
    ) -> Result<(Vec<Task>, i64)> {
        let mut where_sql = String::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(p) = source {
            where_sql.push_str(" WHERE source LIKE ?1 || '%'");
            args.push(p.to_string());
        } else if mine {
            // 「我的」= 没有 `:` 的来源（human / cli / api），和 `is_mine` 同一套判据
            where_sql.push_str(" WHERE source NOT LIKE '%:%'");
        } else if agents {
            where_sql.push_str(" WHERE source LIKE '%:%'");
        }
        let c = self.conn.lock().unwrap();
        let total: i64 = c
            .query_row(
                &format!("SELECT COUNT(*) FROM tasks{where_sql}"),
                rusqlite::params_from_iter(args.iter()),
                |r| r.get(0),
            )
            .unwrap_or(0);
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS} FROM tasks{where_sql} ORDER BY created_at DESC LIMIT {}",
            limit as i64
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(args.iter()))?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_task(r));
        }
        Ok((out, total))
    }

    pub fn list(&self, limit: usize) -> Result<Vec<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let mut rows = stmt.query(params![limit as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_task(r));
        }
        Ok(out)
    }

    /// 还活着的任务（排队 / 下载中 / 中断）。
    ///
    /// 清缓存时用：这些任务引用的缓存块一个都不许动。以前是 `list(1000)` 再筛，
    /// 台账超过一千条时，排在后面的活跃任务就漏了 —— 它引用的块会被当成没人用。
    pub fn active(&self) -> Result<Vec<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE state IN ('pending','running','interrupted')"
        ))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_task(r));
        }
        Ok(out)
    }

    /// 增量：只要 `updated_at > since` 的那些。
    ///
    /// 界面每 700ms 拉一次，没必要把 200 条整份搬一遍；只有变动过的才值钱。
    /// `since=0` 就等于全量（客户端第一次连、或者断了重连时用）。
    pub fn since(&self, since_ms: i64, limit: usize) -> Result<Vec<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE updated_at > ?1 ORDER BY updated_at ASC LIMIT ?2"
        ))?;
        let mut rows = stmt.query(params![since_ms, limit as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_task(r));
        }
        Ok(out)
    }

    /// 同一个内容已经下过、而且文件还在 —— 这是「秒传」在本地的那一半。
    pub fn find_done_by_sha(&self, sha: &str) -> Result<Option<Task>> {
        if sha.is_empty() {
            return Ok(None);
        }
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE sha256 = ?1 AND state = ?2 ORDER BY updated_at DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![sha, STATE_DONE])?;
        let t = rows.next()?.map(row_to_task);
        drop(rows);
        drop(stmt);
        match t {
            Some(t) if std::path::Path::new(&t.dest).is_file() => Ok(Some(t)),
            _ => Ok(None),
        }
    }

    /// 同一个地址以前取到过什么（不看文件还在不在）。
    /// TOFU 就靠它：这次取到的 hash 必须与上次一致，否则说明「这个地址换了东西」。
    /// 某个来源从 `since_ms` 起完成的字节数（配额的日用量）。
    pub fn usage_since(&self, source: &str, since_ms: i64) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        // 缓存命中没走网络，不计入配额；否则“秒传”会偷偷占掉当天的额度。
        let mut stmt = c.prepare(
            "SELECT COALESCE(SUM(bytes_total), 0)
             FROM tasks WHERE source = ?1 AND state = ?2 AND updated_at >= ?3
               AND cached = 0",
        )?;
        let total: i64 = stmt.query_row(params![source, STATE_DONE, since_ms], |r| r.get(0))?;
        Ok(total.max(0) as u64)
    }

    pub fn last_done_for_url(&self, url: &str) -> Result<Option<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE url = ?1 AND state = ?2 AND sha256 != '' ORDER BY updated_at DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![url, STATE_DONE])?;
        Ok(rows.next()?.map(row_to_task))
    }

    /// 某个地址最近的一条记录（任意状态）：`skyget show --url` 用。
    pub fn latest_for_url(&self, url: &str) -> Result<Option<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE url = ?1 ORDER BY updated_at DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![url])?;
        Ok(rows.next()?.map(row_to_task))
    }

    /// 同一 URL 已经完整下过（dest 还在）：不用再问服务器。
    pub fn find_done_by_url(&self, url: &str) -> Result<Option<Task>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS}
             FROM tasks WHERE url = ?1 AND state = ?2 ORDER BY updated_at DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![url, STATE_DONE])?;
        let t = rows.next()?.map(row_to_task);
        drop(rows);
        drop(stmt);
        match t {
            Some(t) if std::path::Path::new(&t.dest).is_file() => Ok(Some(t)),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // row_to_task 在下面：读文件时先看到 SQL 更顺
mod tests {
    use super::*;
    use crate::model::Task;
    use std::sync::Arc;

    fn temp_store(tag: &str) -> (Arc<Store>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "skyget-store-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open_at(&dir.join("tasks.db")).unwrap();
        (Arc::new(s), dir)
    }

    #[test]
    fn idempotency_claim_is_atomic_under_concurrency() {
        let (store, dir) = temp_store("idem");
        // 8 个线程同时抢同一个键：只能有一个成功，其余必须看到 false。
        let mut handles = Vec::new();
        for i in 0..8 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                s.claim_idem("same-key", &format!("job-{i}")).unwrap()
            }));
        }
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(winners, 1, "同一个幂等键只能被一个请求占住");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn events_are_append_only_and_replayable_from_a_seq() {
        let (store, dir) = temp_store("events");
        let mut t = Task::new("https://example.org/x", "~/Downloads/");
        t.id = "job-e".into();
        store.save(&t).unwrap();
        for i in 0..5 {
            store
                .append_event("job-e", "progress", &serde_json::json!({"i": i}))
                .unwrap();
        }
        let all = store.events_since("job-e", 0, 100).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all.first().unwrap().seq, 1);
        assert_eq!(all.last().unwrap().seq, 5);
        assert_eq!(all[4].data["i"], 4);

        let tail = store.events_since("job-e", 3, 100).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
        assert_eq!(store.last_event_seq("job-e").unwrap(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quota_usage_ignores_cache_hits() {
        let (store, dir) = temp_store("quota-cache");
        for (id, cached) in [("job-net", false), ("job-cache", true)] {
            let mut t = Task::new("https://example.org/x", "/tmp/x");
            t.id = id.into();
            t.source = "agent:a".into();
            t.state = crate::model::STATE_DONE.into();
            t.bytes_total = 100;
            t.cached = cached;
            store.save(&t).unwrap();
        }
        assert_eq!(
            store.usage_since("agent:a", 0).unwrap(),
            100,
            "只有走网络的那条占配额"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pins_round_trip_and_protect_shas() {
        let (store, dir) = temp_store("pins");
        let sha = "ab".repeat(32);
        let p = store.pin(&sha, "agent:claude", 3600, "in use").unwrap();
        assert_eq!(p.sha256, sha);
        assert!(p.expires_at > p.created_at);
        assert!(store.pinned_shas().unwrap().contains(&sha));
        let all = store.pins().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].agent, "agent:claude");

        // 同一个 (sha, agent) 再 pin 是更新，不会多一行
        store.pin(&sha, "agent:claude", 0, "keep forever").unwrap();
        assert_eq!(store.pins().unwrap().len(), 1);
        assert_eq!(store.pins().unwrap()[0].note, "keep forever");
        assert_eq!(store.pins().unwrap()[0].expires_at, 0);

        assert!(store.unpin(&sha, "agent:claude").unwrap());
        assert!(store.pinned_shas().unwrap().is_empty());
        assert!(!store.unpin(&sha, "agent:claude").unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_if_exists_never_resurrects_a_deleted_task() {
        let (store, dir) = temp_store("no-resurrect");
        let mut t = Task::new("https://example.org/x", "~/Downloads/");
        t.id = "job-gone".into();
        store.save(&t).unwrap();
        store.remove("job-gone").unwrap();
        t.state = crate::model::STATE_FAILED.into();
        t.error = "late failure".into();
        assert!(!store.save_if_exists(&t).unwrap());
        assert!(store.get("job-gone").unwrap().is_none(), "被取消的任务不能被终态写回");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn retention_prunes_progress_before_status_events() {
        let (store, dir) = temp_store("retention");
        store
            .append_event("job-ret", "progress", &serde_json::json!({"bytes_done": 1}))
            .unwrap();
        store
            .append_event("job-ret", "done", &serde_json::json!({"sha256": "x"}))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(store.prune_progress_events(0).unwrap(), 1);
        let left = store.events_since("job-ret", 0, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, "done", "状态事件不能被进度保留策略吃掉");

        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(store.prune_old_events(0).unwrap(), 1);
        assert_eq!(store.event_count().unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tombstones_survive_a_daemon_restart() {
        let dir = std::env::temp_dir().join(format!("skyget-tomb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("tasks.db");
        let id;
        let ts;
        {
            let store = Store::open_at(&db).unwrap();
            let mut t = Task::new("https://example.org/x", "~/Downloads/");
            t.id = "job-deleted".into();
            store.save(&t).unwrap();
            id = t.id.clone();
            store.remove(&id).unwrap();
            let tombs = store.tombstones_since(0, 10).unwrap();
            assert_eq!(tombs.len(), 1);
            assert_eq!(tombs[0].0, id);
            ts = tombs[0].1;
            assert!(!store.remove("job-that-never-existed").unwrap());
        }
        // 重新打开同一个库 = daemon 重启；删除仍要对增量客户端可见。
        {
            let store = Store::open_at(&db).unwrap();
            let tombs = store.tombstones_since(0, 10).unwrap();
            assert_eq!(tombs, vec![(id.clone(), ts)], "墓碑必须落库，不能只在内存");
            assert!(store.tombstones_since(ts, 10).unwrap().is_empty());
            assert_eq!(store.tombstone_count().unwrap(), 1);
            std::thread::sleep(std::time::Duration::from_millis(2));
            store.prune_tombstones(0).unwrap();
            assert_eq!(store.tombstone_count().unwrap(), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_ledger_schema_is_migrated_with_sane_defaults() {
        let dir = std::env::temp_dir().join(format!("skyget-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("tasks.db");
        {
            // 0.18 时代的表：只有核心列，没有 pid/trust/error_code/direction/sources…
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE tasks (
                    id TEXT PRIMARY KEY, url TEXT NOT NULL, dest TEXT NOT NULL,
                    state TEXT NOT NULL, bytes_done INTEGER NOT NULL DEFAULT 0,
                    bytes_total INTEGER NOT NULL DEFAULT 0,
                    sha256 TEXT NOT NULL DEFAULT '', error TEXT NOT NULL DEFAULT '',
                    source TEXT NOT NULL DEFAULT 'human',
                    expected_sha256 TEXT NOT NULL DEFAULT '',
                    created_at INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO tasks (id,url,dest,state,sha256)
                VALUES ('old-job','https://example.org/x','/tmp/x','done','abc');",
            )
            .unwrap();
        }
        let store = Store::open_at(&db).unwrap();
        let mut t = store.get("old-job").unwrap().expect("老行还能读");
        assert_eq!(t.state, "done");
        assert_eq!(t.direction, "download", "老库默认下载方向");
        assert!(t.sources.is_empty());
        assert_eq!(t.protocol, "");
        // 新列可写回
        t.direction = "upload".into();
        t.protocol = "tus".into();
        t.credential_ref = "env:UP".into();
        t.sources = vec!["https://a/x".into(), "https://b/x".into()];
        store.save(&t).unwrap();
        let back = store.get("old-job").unwrap().unwrap();
        assert_eq!(back.direction, "upload");
        assert_eq!(back.protocol, "tus");
        assert_eq!(back.sources.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn events_do_not_store_presigned_urls_in_clear() {
        let (store, dir) = temp_store("event-redact");
        let url = "https://bucket.s3.amazonaws.com/x?X-Amz-Signature=deadbeef&X-Amz-Credential=AKIA";
        store
            .append_event(
                "job-e",
                "failed",
                &serde_json::json!({"error": format!("请求 {url} 失败")}),
            )
            .unwrap();
        let ev = store.events_since("job-e", 0, 10).unwrap();
        let s = ev[0].data.to_string();
        assert!(!s.contains("deadbeef"), "事件流泄露签名：{s}");
        assert!(!s.contains("AKIA"), "事件流泄露 credential：{s}");
        assert!(s.to_ascii_lowercase().contains("redacted"), "{s}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removing_a_task_releases_its_idempotency_keys() {
        let (store, dir) = temp_store("release");
        let mut t = Task::new("https://example.org/x", "~/Downloads/");
        t.id = "job-1".into();
        store.save(&t).unwrap();
        assert!(store.claim_idem("k", "job-1").unwrap());
        assert_eq!(store.find_idem("k").unwrap().unwrap().id, "job-1");

        store.remove("job-1").unwrap();
        assert!(store.find_idem("k").unwrap().is_none(), "任务删了，键不能再指向幽灵");
        assert!(store.claim_idem("k", "job-2").unwrap(), "键必须可重新使用");
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn row_to_task(r: &rusqlite::Row<'_>) -> Task {
    Task {
        id: r.get(0).unwrap_or_default(),
        url: r.get(1).unwrap_or_default(),
        dest: r.get(2).unwrap_or_default(),
        state: r.get(3).unwrap_or_default(),
        bytes_done: r.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
        bytes_total: r.get::<_, i64>(5).unwrap_or(0).max(0) as u64,
        sha256: r.get(6).unwrap_or_default(),
        error: r.get(7).unwrap_or_default(),
        source: r.get(8).unwrap_or_default(),
        expected_sha256: r.get(9).unwrap_or_default(),
        created_at: r.get(10).unwrap_or(0),
        updated_at: r.get(11).unwrap_or(0),
        pid: r.get(12).unwrap_or(0),
        links_live: r.get::<_, i64>(13).unwrap_or(0).max(0) as u32,
        links_seen: r.get::<_, i64>(14).unwrap_or(0).max(0) as u32,
        trust: r.get(15).unwrap_or_default(),
        cached: r.get::<_, i64>(16).unwrap_or(0) != 0,
        error_code: r.get(17).unwrap_or_default(),
        retryable: r.get::<_, i64>(18).unwrap_or(0) != 0,
        retry_after_ms: r.get::<_, i64>(19).unwrap_or(0).max(0) as u64,
        next_action: r.get(20).unwrap_or_default(),
        direction: r.get(21).unwrap_or_else(|_| crate::model::DIR_DOWNLOAD.to_string()),
        protocol: r.get(22).unwrap_or_default(),
        credential_ref: r.get(23).unwrap_or_default(),
        sources: r
            .get::<_, String>(24)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        priority: r.get(25).unwrap_or_default(),
        accept_change: false,
            rerun: false,
    }
}
