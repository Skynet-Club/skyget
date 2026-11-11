//! 每 agent 的每日下载配额。
//!
//! 配置：`SKYGET_AGENT_QUOTAS="claude=10GB,pi=500MB"`；键可以写 `claude`，
//! 也可以写 `agent:claude`。只统计 `done` 任务的 `bytes_total`，按 UTC 自然日。
//! 缓存命中不联网，不计入配额。

use crate::security::Denied;
use crate::store::Store;

/// 解析 `10GB` / `500MB` / `1.5g` / `1024`。十进制单位，和 `human()` 一致。
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || s == "unlimited" || s == "none" {
        return None;
    }
    let (num, mult) = if let Some(v) = s.strip_suffix("tb") {
        (v, 1_000_000_000_000u64)
    } else if let Some(v) = s.strip_suffix("gb") {
        (v, 1_000_000_000)
    } else if let Some(v) = s.strip_suffix("mb") {
        (v, 1_000_000)
    } else if let Some(v) = s.strip_suffix("kb") {
        (v, 1_000)
    } else if let Some(v) = s.strip_suffix('b') {
        (v, 1)
    } else if let Some(v) = s.strip_suffix('g') {
        (v, 1_000_000_000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 1_000_000)
    } else if let Some(v) = s.strip_suffix('k') {
        (v, 1_000)
    } else {
        (s.as_str(), 1)
    };
    let n: f64 = num.trim().parse().ok()?;
    if n < 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

fn parse_quotas(raw: &str) -> (Vec<(String, Option<u64>)>, bool) {
    let mut out = Vec::new();
    let mut invalid = false;
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue; // 允许尾逗号
        }
        let Some((k, v)) = item.split_once('=') else {
            invalid = true;
            continue;
        };
        let name = k.trim().trim_start_matches("agent:");
        if name.is_empty() {
            invalid = true;
            continue;
        }
        let v = v.trim();
        let limit = if v.eq_ignore_ascii_case("unlimited") || v.eq_ignore_ascii_case("none") {
            None
        } else {
            // 配置写错时 fail closed：宁可全拒，也别把 `10G B` 当成不限。
            Some(parse_size(v).unwrap_or(0))
        };
        out.push((name.to_string(), limit));
    }
    (out, invalid)
}

/// 配置里出现的 agent 和上限（None = 不限）。给 `/metrics` 用。
pub fn configured() -> Vec<(String, Option<u64>)> {
    std::env::var("SKYGET_AGENT_QUOTAS")
        .map(|raw| parse_quotas(&raw).0)
        .unwrap_or_default()
}

/// 这个来源的每日上限；没配置就是 None（不限制）。
///
/// 语法错误（缺 `=`、空 agent 名）不能当成“没配置”而放行：那会让一个笔误
/// 把整台机器的配额关掉。这里按 0 处理（全拒），并在 stderr 警告一次。
pub fn limit_for(source: &str) -> Option<u64> {
    let raw = match std::env::var("SKYGET_AGENT_QUOTAS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return None,
    };
    limit_from(&raw, source)
}

fn limit_from(raw: &str, source: &str) -> Option<u64> {
    let (list, invalid) = parse_quotas(raw);
    if invalid {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "SKYGET_AGENT_QUOTAS 有非法条目（缺 '=' 或空 agent 名）；按 fail-closed 处理：所有 agent 配额为 0"
            );
        });
        return Some(0);
    }
    let name = source.trim_start_matches("agent:");
    list.into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, limit)| limit)
}

fn day_start_ms(now: i64) -> i64 {
    now - now.rem_euclid(86_400_000)
}

/// 今天已经完成的字节数（给 `skyget agents` / `/agents` 展示）。
pub fn used_today(store: &Store, source: &str) -> anyhow::Result<u64> {
    store.usage_since(source, day_start_ms(crate::model::now_ms()))
}

/// 在真正开跑前检查：已用 + 本次总量不能超过当天上限。
pub fn check(store: &Store, source: &str, add: u64) -> anyhow::Result<()> {
    let Some(limit) = limit_for(source) else {
        return Ok(());
    };
    let used = store.usage_since(source, day_start_ms(crate::model::now_ms()))?;
    if used.saturating_add(add) > limit {
        return Err(Denied {
            code: "quota.exceeded",
            why: format!(
                "今天的配额不够：已用 {} / 上限 {}，这次还要 {}。明天再试，或让管理员调 SKYGET_AGENT_QUOTAS。",
                crate::engine::human(used),
                crate::engine::human(limit),
                crate::engine::human(add)
            ),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_decimal_and_case_insensitive() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1_000));
        assert_eq!(parse_size("1.5MB"), Some(1_500_000));
        assert_eq!(parse_size("2gb"), Some(2_000_000_000));
        assert_eq!(parse_size("unlimited"), None);
        assert_eq!(parse_size("bogus"), None);
    }

    #[test]
    fn day_start_is_utc_midnight() {
        let now = 86_400_000 * 3 + 12_345;
        assert_eq!(day_start_ms(now), 86_400_000 * 3);
    }

    #[test]
    fn quota_list_parses_names_and_unlimited() {
        let (got, invalid) = parse_quotas("claude=1MB, agent:pi=unlimited,broken");
        assert!(invalid, "缺 '=' 的条目必须被标记");
        assert_eq!(
            got,
            vec![
                ("claude".to_string(), Some(1_000_000)),
                ("pi".to_string(), None),
            ]
        );
        let (got, invalid) = parse_quotas("claude=bogus");
        assert!(!invalid);
        assert_eq!(got, vec![("claude".into(), Some(0))]);
        // 尾逗号是常见写法，不算错。
        let (got, invalid) = parse_quotas("claude=1MB,");
        assert!(!invalid);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn malformed_config_fails_closed_for_everyone() {
        assert_eq!(limit_from("claude:10GB", "agent:claude"), Some(0));
        assert_eq!(limit_from("claude:10GB", "agent:pi"), Some(0));
        assert_eq!(limit_from("claude=1MB", "agent:claude"), Some(1_000_000));
        assert_eq!(limit_from("claude=1MB", "agent:pi"), None);
    }
}
