//! 进程内安全计数，给 `/metrics` 用。重启归零；不做持久化。
//!
//! 只记“被拒绝/失败”的次数，不记每次请求的明细 —— 明细在事件流里。
//! 标签集合是固定的，不接收调用方字符串，避免 Prometheus 基数被放大。

use std::sync::atomic::{AtomicU64, Ordering};

static AUTH: AtomicU64 = AtomicU64::new(0);
static POLICY: AtomicU64 = AtomicU64::new(0);
static CREDENTIAL: AtomicU64 = AtomicU64::new(0);
static QUOTA: AtomicU64 = AtomicU64::new(0);
static WEBHOOK_FAILED: AtomicU64 = AtomicU64::new(0);
static CACHE_EVICTED_BLOBS: AtomicU64 = AtomicU64::new(0);
static CACHE_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
/// 缓存当前大小（字节），由水位线线程每次扫完更新。0 表示还没扫过。
static CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub enum Kind {
    Auth,
    Policy,
    Credential,
    Quota,
}

fn cell(kind: Kind) -> &'static AtomicU64 {
    match kind {
        Kind::Auth => &AUTH,
        Kind::Policy => &POLICY,
        Kind::Credential => &CREDENTIAL,
        Kind::Quota => &QUOTA,
    }
}

pub fn deny(kind: Kind) {
    cell(kind).fetch_add(1, Ordering::Relaxed);
}

pub fn count(kind: Kind) -> u64 {
    cell(kind).load(Ordering::Relaxed)
}

/// 任务失败后按 `error_code` 归类；引擎与上传共用，避免两处各写一遍。
pub fn note_error_code(code: &str) {
    if code.starts_with("policy.credential") {
        deny(Kind::Credential);
    } else if code.starts_with("policy.") {
        deny(Kind::Policy);
    } else if code == "quota.exceeded" {
        deny(Kind::Quota);
    }
}

pub fn webhook_failed() {
    WEBHOOK_FAILED.fetch_add(1, Ordering::Relaxed);
}

/// 水位线清扫删掉了几块 / 多少字节，以及清完之后缓存多大。
pub fn cache_evicted(blobs: u64, bytes: u64) {
    CACHE_EVICTED_BLOBS.fetch_add(blobs.max(1), Ordering::Relaxed);
    CACHE_EVICTED_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub fn cache_after_gc(total: u64) {
    CACHE_BYTES.store(total, Ordering::Relaxed);
}

pub fn render(out: &mut String) {
    out.push_str(
        "# HELP skyget_denied_total 被拒绝的操作（进程内累计）\n\
         # TYPE skyget_denied_total counter\n",
    );
    for (name, kind) in [
        ("auth", Kind::Auth),
        ("policy", Kind::Policy),
        ("credential", Kind::Credential),
        ("quota", Kind::Quota),
    ] {
        out.push_str(&format!(
            "skyget_denied_total{{kind=\"{name}\"}} {}\n",
            count(kind)
        ));
    }
    out.push_str(
        "# HELP skyget_webhook_failed_total webhook 投递最终失败次数\n\
         # TYPE skyget_webhook_failed_total counter\n",
    );
    out.push_str(&format!(
        "skyget_webhook_failed_total {}\n",
        WEBHOOK_FAILED.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP skyget_cache_evicted_blobs_total 水位线清扫删掉的内容块数\n\
         # TYPE skyget_cache_evicted_blobs_total counter\n",
    );
    out.push_str(&format!(
        "skyget_cache_evicted_blobs_total {}\n",
        CACHE_EVICTED_BLOBS.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP skyget_cache_evicted_bytes_total 水位线清扫释放的字节\n\
         # TYPE skyget_cache_evicted_bytes_total counter\n",
    );
    out.push_str(&format!(
        "skyget_cache_evicted_bytes_total {}\n",
        CACHE_EVICTED_BYTES.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP skyget_cache_bytes 内容缓存当前占用（0 = 未启用自动清扫）\n\
         # TYPE skyget_cache_bytes gauge\n",
    );
    out.push_str(&format!(
        "skyget_cache_bytes {}\n",
        CACHE_BYTES.load(Ordering::Relaxed)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_only_go_up_and_render_all_kinds() {
        let before = count(Kind::Auth);
        deny(Kind::Auth);
        assert!(count(Kind::Auth) > before);

        note_error_code("policy.credential_denied");
        note_error_code("policy.egress_denied");
        note_error_code("quota.exceeded");
        note_error_code("http.404");

        let mut out = String::new();
        render(&mut out);
        for name in ["auth", "policy", "credential", "quota"] {
            assert!(
                out.contains(&format!("skyget_denied_total{{kind=\"{name}\"}}")),
                "缺 {name}: {out}"
            );
        }
        assert!(out.contains("skyget_webhook_failed_total"));
    }
}
