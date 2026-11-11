//! 这台机器到底在下什么：一次算清。
//!
//! 为什么要有它：失败分类只在出事那一瞬间说一次，事后再没人能回答
//! 「这周有多少次连不上源」「秒传省了多少网络」。这些都是台账里的现成事实，
//! 只是没人把它算出来。

use std::collections::BTreeMap;

use anyhow::Result;

use crate::engine::{classify_failure, Failure};
use crate::model::{Task, STATE_DONE, STATE_FAILED, STATE_INTERRUPTED, STATE_PENDING, STATE_RUNNING};
use crate::paths;
use crate::store::Store;

#[derive(Debug, Default, serde::Serialize)]
pub struct Stats {
    pub tasks: usize,
    pub by_state: BTreeMap<String, usize>,
    /// 完成的总字节
    pub done_bytes: u64,
    /// 其中「本机早有、一字节没走网络」的次数与字节
    pub cached_runs: usize,
    pub cached_bytes: u64,
    /// 按来源：人 / 命令行 / 各个 agent
    pub by_source: BTreeMap<String, usize>,
    /// 失败按性质分：网不通 / 被限流 / 其他
    pub failures: BTreeMap<String, usize>,
    /// 失败按机器可读的 error_code 分（给 /metrics 看板）
    pub failures_by_code: BTreeMap<String, usize>,
    pub blobs: usize,
    pub blob_bytes: u64,
    /// 最早 / 最近一条记录的时间（毫秒）
    pub first_ms: i64,
    pub last_ms: i64,
}

impl Stats {
    /// 秒传省下的比例（按字节）。没有完成记录时是 0。
    pub fn saved_ratio(&self) -> f64 {
        if self.done_bytes == 0 {
            0.0
        } else {
            self.cached_bytes as f64 / self.done_bytes as f64
        }
    }
}

pub fn collect(store: &Store) -> Result<Stats> {
    let tasks: Vec<Task> = store.list(100_000)?;
    let mut st = Stats::default();
    st.tasks = tasks.len();

    for t in &tasks {
        *st.by_state.entry(t.state.clone()).or_insert(0) += 1;
        let src = if t.source.contains(':') {
            t.source.clone()
        } else if t.source.is_empty() {
            "human".to_string()
        } else {
            t.source.clone()
        };
        *st.by_source.entry(src).or_insert(0) += 1;

        if t.state == STATE_DONE {
            st.done_bytes += t.bytes_done;
            if t.cached {
                st.cached_runs += 1;
                st.cached_bytes += t.bytes_done;
            }
        }
        if t.state == STATE_FAILED {
            let kind = match classify_failure(&t.error) {
                Failure::Network => "网不通",
                Failure::Throttle => "被限流",
                Failure::Other => "其他（源/内容问题）",
            };
            *st.failures.entry(kind.to_string()).or_insert(0) += 1;
            let code = if t.error_code.is_empty() {
                "unknown"
            } else {
                t.error_code.as_str()
            };
            *st.failures_by_code.entry(code.to_string()).or_insert(0) += 1;
        }
        if st.first_ms == 0 || (t.created_at > 0 && t.created_at < st.first_ms) {
            st.first_ms = t.created_at;
        }
        st.last_ms = st.last_ms.max(t.updated_at);
    }

    // 缓存占用：数一下块本身，不查台账（台账可能被清过，缓存还在）
    if let Ok(rd) = std::fs::read_dir(paths::blobs_dir()) {
        for d in rd.flatten() {
            let Ok(inner) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for f in inner.flatten() {
                if let Ok(m) = f.metadata() {
                    if m.is_file() {
                        st.blobs += 1;
                        st.blob_bytes += m.len();
                    }
                }
            }
        }
    }
    Ok(st)
}

/// 一行状态说明，给 CLI 打印用。
pub fn state_label(s: &str) -> &'static str {
    match s {
        STATE_DONE => "完成",
        STATE_FAILED => "失败",
        STATE_INTERRUPTED => "中断",
        STATE_RUNNING => "下载中",
        STATE_PENDING => "排队",
        _ => "其他",
    }
}
