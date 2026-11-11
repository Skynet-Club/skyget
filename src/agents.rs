//! token → agent 身份注册表。
//!
//! 0.19 只有一个 `SKYGET_API_TOKEN`：所有 agent 共用一个 token，`source` 字段
//! 是调用方自称 —— 谁都能写 `agent:claude`，公平队列和台账归属都不可信。
//!
//! `SKYGET_AGENTS` 给每个 agent 一个 token：
//!
//! ```text
//! SKYGET_AGENTS="tok_claude=claude,tok_codex=codex"
//! ```
//!
//! daemon 从 Authorization 头解析出身份，覆盖请求体里的 `source`。
//! 没配 `SKYGET_AGENTS` 时退回旧的单 token 行为（兼容）。

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::security::Policy;

#[derive(Debug, Clone)]
pub struct Registry {
    tokens: HashMap<String, String>,
}

impl Registry {
    /// `Err` = 环境变量设置了但一个合法条目都没有（配置写错）。
    ///
    /// 以前这种情况静默返回 None，daemon 退回单 token/无鉴权模式 ——
    /// 一个笔误就能把 per-agent 身份和策略关掉，属于 fail-open。
    pub fn from_env() -> Result<Option<Self>> {
        let raw = match std::env::var("SKYGET_AGENTS") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Ok(None),
        };
        let mut tokens = HashMap::new();
        for pair in raw.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let Some((token, name)) = pair.split_once('=') else {
                eprintln!("SKYGET_AGENTS 里有一项不是 token=name，已跳过：{pair}");
                continue;
            };
            let (token, name) = (token.trim(), name.trim());
            if token.is_empty() || !valid_name(name) {
                eprintln!("SKYGET_AGENTS 里的 token/name 不合法，已跳过：{pair}");
                continue;
            }
            if tokens.insert(token.to_string(), name.to_string()).is_some() {
                eprintln!("SKYGET_AGENTS 里 token 重复，后一个覆盖前一个：{pair}");
            }
        }
        if tokens.is_empty() {
            anyhow::bail!("SKYGET_AGENTS 设置了，但没有任何合法的 token=name 条目");
        }
        Ok(Some(Self { tokens }))
    }

    pub fn name_for(&self, token: &str) -> Option<&str> {
        self.tokens.get(token).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// 去重、排序后的 agent 名字（不含 token；token 永远不出现在接口里）。
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tokens.values().cloned().collect();
        v.sort();
        v.dedup();
        v
    }
}

/// 单个 agent 的策略覆盖。未写的字段继承全局 strict 策略。
/// 字段名写错必须拒绝启动：静默忽略等于把限制悄悄放宽。
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Rule {
    #[serde(default)]
    dest_roots: Option<Vec<String>>,
    #[serde(default)]
    allowed_hosts: Option<Vec<String>>,
    #[serde(default)]
    upload_roots: Option<Vec<String>>,
    /// 同时最多跑几个任务；不填用全局 `SKYGET_MAX_PER_AGENT`。
    #[serde(default)]
    max_running: Option<usize>,
}

/// agent 名字 → 策略覆盖。配置文件由 `SKYGET_AGENT_POLICIES` 指定：
///
/// ```json
/// {"claude": {"dest_roots": ["~/models"], "allowed_hosts": ["huggingface.co"],
///             "upload_roots": ["~/outputs"]}}
/// ```
#[derive(Debug, Clone)]
pub struct AgentPolicies {
    rules: HashMap<String, Rule>,
}

impl AgentPolicies {
    pub fn from_env() -> Result<Option<Self>> {
        let path = match std::env::var("SKYGET_AGENT_POLICIES") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => return Ok(None),
        };
        let text = std::fs::read_to_string(path.trim())
            .with_context(|| format!("读不到 agent 策略文件：{path}"))?;
        let ap = Self::parse(&text)?;
        Ok(Some(ap))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let rules: HashMap<String, Rule> =
            serde_json::from_str(text).context("agent 策略文件不是 {name: {...}} 的 JSON")?;
        for (name, rule) in &rules {
            if !valid_name(name) {
                anyhow::bail!("agent 策略文件里有非法名字：{name}");
            }
            // `dest_roots` 空数组在 Policy 里表示“不限制”（兼容旧行为）。
            // 在 per-agent 策略里写 [] 很容易被误解成“禁止落地”，实际却相反，
            // 所以直接拒绝：要继承就不写，要禁止就给一个不可能命中的根。
            if let Some(v) = &rule.dest_roots {
                if v.is_empty() {
                    anyhow::bail!(
                        "agent {name} 的 dest_roots 不能是空数组；继承全局请省略该字段"
                    );
                }
            }
        }
        Ok(Self { rules })
    }

    /// 给某个 agent 套上覆盖；没写到的字段继承 base。
    pub fn for_agent(&self, name: &str, base: &Policy) -> Policy {
        let mut p = base.clone();
        if let Some(r) = self.rules.get(name) {
            if let Some(v) = &r.dest_roots {
                p.dest_roots = frames(v);
            }
            if let Some(v) = &r.allowed_hosts {
                p.allowed_hosts = Some(
                    v.iter()
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            if let Some(v) = &r.upload_roots {
                p.upload_roots = frames(v);
            }
        }
        p
    }

    /// 从台账来源（`agent:claude`）取策略；人/CLI 用 base。
    pub fn for_source(&self, source: &str, base: &Policy) -> Policy {
        match source.strip_prefix("agent:") {
            Some(name) => self.for_agent(name, base),
            None => base.clone(),
        }
    }

    /// 策略文件里出现过的 agent 名字（可能还没提交过任务）。
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.rules.keys().cloned().collect();
        v.sort();
        v
    }

    /// 某个 agent 显式配置的并发上限；没配 = None（用全局默认）。
    pub fn max_running(&self, name: &str) -> Option<usize> {
        self.rules.get(name).and_then(|r| r.max_running).filter(|n| *n > 0)
    }

    /// (source, max_running) 列表，交给调度器按来源设上限。
    pub fn concurrency_limits(&self) -> Vec<(String, usize)> {
        self.rules
            .iter()
            .filter_map(|(name, r)| {
                r.max_running
                    .filter(|n| *n > 0)
                    .map(|n| (format!("agent:{name}"), n))
            })
            .collect()
    }
}

fn frames(v: &[String]) -> Vec<PathBuf> {
    v.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(crate::settings::expand(s)))
        .collect()
}

/// agent 名字会进 `source: agent:<name>`、公平队列的键和日志：
/// 只允许字母/数字/`-_.`，最长 32。
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些测试改全局环境变量，必须串行；否则并行跑时互相把 SKYGET_AGENTS 改掉。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parses_pairs_and_rejects_bad_names() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(
            "SKYGET_AGENTS",
            "tok1=claude,tok2=codex,broken,tok3=bad name,tok4=ok_agent.1",
        );
        let r = Registry::from_env().expect("配置").expect("注册表");
        assert_eq!(r.len(), 3);
        assert_eq!(r.name_for("tok1"), Some("claude"));
        assert_eq!(r.name_for("tok2"), Some("codex"));
        assert_eq!(r.name_for("tok3"), None);
        assert_eq!(r.name_for("tok4"), Some("ok_agent.1"));
        std::env::remove_var("SKYGET_AGENTS");
    }

    #[test]
    fn per_agent_policy_overrides_only_the_named_agent() {
        let ap = AgentPolicies::parse(
            r#"{"claude":{"dest_roots":["/tmp/a"],"allowed_hosts":["a.com","*.b.com"],"upload_roots":["/tmp/u"],"max_running":1}}"#,
        )
        .unwrap();
        let base = Policy::default();
        let p = ap.for_agent("claude", &base);
        assert_eq!(p.dest_roots, vec![PathBuf::from("/tmp/a")]);
        assert_eq!(
            p.allowed_hosts,
            Some(vec!["a.com".to_string(), "*.b.com".to_string()])
        );
        assert_eq!(p.upload_roots, vec![PathBuf::from("/tmp/u")]);

        let other = ap.for_agent("codex", &base);
        assert_eq!(other.dest_roots, base.dest_roots);
        assert_eq!(other.allowed_hosts, base.allowed_hosts);
        assert!(other.upload_roots.is_empty());

        assert_eq!(
            ap.concurrency_limits(),
            vec![("agent:claude".to_string(), 1)]
        );

        assert!(AgentPolicies::parse(r#"{"bad name":{}}"#).is_err());
        assert!(
            AgentPolicies::parse(r#"{"claude":{"dest_roots":[]}}"#).is_err(),
            "空 dest_roots 不能被当成「不限制」"
        );
        assert!(
            AgentPolicies::parse(r#"{"claude":{"dest_root":["/tmp"]}}"#).is_err(),
            "字段名写错不能静默继承全局（可能是放宽权限）"
        );
    }

    #[test]
    fn empty_env_means_legacy_mode_but_bad_config_fails_closed() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SKYGET_AGENTS");
        assert!(Registry::from_env().unwrap().is_none());

        // 设置了变量却没有任何合法条目 = 配置错误，不能静默降级成无鉴权。
        std::env::set_var("SKYGET_AGENTS", "broken,also-broken");
        assert!(Registry::from_env().is_err());
        std::env::set_var("SKYGET_AGENTS", "=name,token=bad name");
        assert!(Registry::from_env().is_err());
        std::env::remove_var("SKYGET_AGENTS");
    }
}
