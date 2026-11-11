# ADR 0001 · 守护进程是唯一写者

- 状态：已采纳（2026-10-30，0.19.0）
- 相关：`src/api.rs`、`src/scheduler.rs`、`src/lock.rs`、`src/bin/skyget.rs`

## 背景

0.12 之前下载线程在界面里，关窗口等于掐下载；0.16 拆出守护进程，但 CLI 仍是
「自己跑引擎」的第二个写者。两者共用 `SKYGET_HOME` 下的 `.part`、位图和 SQLite 台账。

单人使用时这没出过事，因为同一 URL 很少被两个进程同时下。agent 场景不是这样：
一个 agent 重试、另一个 agent 下同一个模型、CLI 和 daemon 同时收到同一个地址，
都会让两个写者打开同一份 `.part` / 位图。0.16.2 的 `running` 表只挡住了
**单进程内**的重复派发，挡不住跨进程。

## 决策

1. **agent 流量统一走守护进程**：`skyget --as <agent>` 默认提交给 daemon；
   `--daemon` 强制、`--standalone` 强制独立。人类不带 `--as` 时保持独立下载。
2. **跨进程分片锁是最后一道防线，不是架构**：`src/lock.rs` 的 `flock` 保证
   即使仍有两个写者，也只有一个能写；另一个得到 `part.busy`（可重试）。
3. **daemon 是并发、队列、事件、台账的唯一权威**：调度器、幂等键、事件流都在它这一侧。
4. CLI 只保留「人类一次性下载」的独立路径；它的输出契约（stdout 一行 JSON、
   stderr NDJSON）不变，daemon 模式下由事件流翻译回同一形状。

## 后果

好处：

- agent 的 shell 死了，下载不死；重连用 `skyget watch <id>`。
- 并发有全局上限和按来源公平，单个 agent 不能饿死别人。
- 台账/缓存只有一份，幂等和秒传才真正成立。

代价：

- agent 路径依赖 daemon；连不上时按来源决定回退（agent 回退并说明，`--daemon` 直接报错）。
- daemon 默认 strict 出网策略；内网/本地源要显式 `SKYGET_ALLOW_PRIVATE=1`。
- 锁文件会留在 `parts/`（unlink 与 flock 的经典竞态，见 `lock.rs` 注释）。

## 没选的方案

- **只加 flock，不统一写者**：能防文件损坏，但得不到全局调度、排队、事件重放，
  也让「agent 会话死了任务还在」无法实现。
- **CLI 链库（`skyfetch`）而不是 HTTP**：会把 CLI 和内核版本绑死，跨语言客户端
  也无从接；HTTP 契约更稳。
- **每个 agent 一个下载器**：缓存、并发、台账全部分裂，正好是这次要解决的问题。

## 后续

- 把人类 CLI 也默认切到 daemon（等 `SKYGET_DEST_ROOTS` / 内网策略的配置体验补齐）。
- 上传复用同一个调度器和台账时，仍然遵守「daemon 唯一写者」。
