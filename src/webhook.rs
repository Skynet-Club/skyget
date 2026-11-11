//! 任务终态 webhook。
//!
//! 配置：`SKYGET_COMPLETION_WEBHOOK=https://...`。只在 daemon 里发；payload 不含
//! 凭证，URL 会去掉 query（签名 URL 的 token 不该被转发到第三方）。
//! 投递失败只记日志，不影响任务终态。

use std::time::Duration;

use serde_json::json;

use crate::store::Store;

fn terminal(state: &str) -> bool {
    matches!(state, "done" | "failed" | "interrupted" | "cancelled")
}

/// 任务是终态就异步 POST 一次。调用方不应该等它。
pub fn notify(store: &Store, id: &str) {
    let Ok(Some(t)) = store.get(id) else {
        return;
    };
    notify_task(&t);
}

/// 投递内容。`error` 可能带完整 URL（reqwest 的传输错误会带 query），
/// 和 `url` 一样要脱敏；否则签名 token 会从错误文本里漏给第三方。
fn payload(t: &crate::model::Task) -> serde_json::Value {
    // 先脱敏 userinfo，再去掉 query：签名 URL 的 query 可能有 token。
    // 台账里可能留着旧记录/手工写入的带账号密码 URL，不能只靠提交时的检查。
    let redacted = crate::security::redact_url(&t.url);
    let url = redacted.split('?').next().unwrap_or(&redacted);
    json!({
        "id": t.id,
        "state": t.state,
        "source": t.source,
        "direction": t.direction,
        "url": url,
        "path": t.dest,
        "sha256": t.sha256,
        "bytes_total": t.bytes_total,
        "error": crate::security::redact_text(&t.error),
        "error_code": t.error_code,
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `sha256=<hex>`；接收方用同一个 secret 对原始 body 重算即可。
fn signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    format!("sha256={}", hex(ring::hmac::sign(&key, body).as_ref()))
}

/// 已知任务（例如刚取消、记录马上要删）直接投递。
pub fn notify_task(t: &crate::model::Task) {
    let Ok(url) = std::env::var("SKYGET_COMPLETION_WEBHOOK") else {
        return;
    };
    let url = url.trim().to_string();
    if url.is_empty() {
        return;
    }
    if !terminal(&t.state) {
        return;
    }
    // webhook 也是出网：默认只允许公网，内网 webhook 要显式 SKYGET_ALLOW_PRIVATE=1。
    // webhook 是部署方配置，不受下载用的 SKYGET_ALLOWED_HOSTS 限制；
    // 仍然沿用 allow_private 的收紧（内网 webhook 需要 SKYGET_ALLOW_PRIVATE=1）。
    let mut policy = crate::security::Policy::strict();
    policy.allowed_hosts = None;
    if crate::security::check_url(&url, &policy).is_err() {
        // 不打印 URL：webhook 地址本身可能带 secret。
        eprintln!("webhook 被出网策略拒绝（内网需要 SKYGET_ALLOW_PRIVATE=1）");
        crate::metrics::webhook_failed();
        return;
    }
    let body = payload(t);
    let body = match serde_json::to_vec(&body) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("webhook payload 序列化失败：{e}");
            crate::metrics::webhook_failed();
            return;
        }
    };
    // 可选签名：设置 SKYGET_WEBHOOK_SECRET 后，接收方可以用同一 secret 验真。
    let secret = std::env::var("SKYGET_WEBHOOK_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let sig_header = secret.as_deref().map(|s| signature(s, &body));
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            // webhook 是部署方配置，不需要跟随跳转；禁止跳转也堵住
            // “初始 URL 合法、302 到内网/元数据”的 SSRF 路径。
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("webhook 客户端构造失败：{e}");
                crate::metrics::webhook_failed();
                return;
            }
        };
        for attempt in 1..=3 {
            let mut req = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body.clone());
            if let Some(sig) = &sig_header {
                req = req.header("X-Skynet-Signature", sig);
            }
            match req.send() {
                Ok(r) if r.status().is_success() => return,
                Ok(r) => eprintln!("webhook HTTP {}（第 {attempt} 次）", r.status()),
                // without_url：reqwest 的错误里默认带完整 URL，可能含 secret。
                Err(e) => eprintln!(
                    "webhook 投递失败（第 {attempt} 次）：{}",
                    e.without_url()
                ),
            }
            std::thread::sleep(Duration::from_secs(attempt));
        }
        crate::metrics::webhook_failed();
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2
        assert_eq!(
            super::signature("key", b"The quick brown fox jumps over the lazy dog"),
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn payload_redacts_query_and_error_text() {
        let mut t = crate::model::Task::new(
            "https://user:pass@example.test/model.bin?X-Amz-Signature=s3cr3t",
            "/tmp/model.bin",
        );
        t.state = crate::model::STATE_FAILED.into();
        t.error =
            "error sending request for url (https://example.test/model.bin?X-Amz-Signature=s3cr3t)"
                .into();
        let body = super::payload(&t);
        let text = body.to_string();
        assert!(!text.contains("s3cr3t"), "webhook 泄露了 query 凭证：{text}");
        assert!(!text.contains("pass"), "webhook 泄露了 URL userinfo：{text}");
        assert!(text.contains("example.test/model.bin"), "URL 主体应保留：{text}");
        assert!(text.contains("error sending request"), "错误信息不该被整段删掉：{text}");
    }
}
