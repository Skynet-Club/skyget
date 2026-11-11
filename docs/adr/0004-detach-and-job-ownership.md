# ADR 0004 · 任务归属与 detach

- 状态：已采纳（2026-10-31，0.22.0）
- 相关：`src/bin/skyget.rs`、`src/api.rs`、`scripts/detach-e2e.sh`

## 背景

ADR 0001 说「agent 的 shell 死了，下载不死」。0.21.3 的 Pi 实测里，任务在 34%
被标成 `job.paused`，看起来像客户端断开把任务带走了。需要把两件事分开：

- daemon 会不会因为客户端断开而暂停任务；
- 客户端能不能提交后立即退出，不用一直 watch。

## 决策

1. `skyget get --detach`：提交给 daemon 后立即返回；`--json` 的 stdout 只留
   提交结果（含 `id`），不再 watch。
2. 客户端生命周期与任务生命周期解耦：`watch` 是只读视图，客户端死亡/断线
   不改变任务状态；暂停只能由显式接口（`/jobs/<id>/pause`）或取消触发。
3. `job.paused` 只表示有人调用了暂停接口，不是「客户端没了」的副作用。

## 验证

`scripts/detach-e2e.sh` 用真 daemon + 本地慢速源：

- `--detach` 14ms 返回；任务继续到 `done`，文件 sha 正确；
- 提交后 `SIGKILL` 整个客户端进程组；任务仍 `running`，最终 `done`，sha 正确。

因此 Pi 那次 `job.paused` 不是断开导致的；暂停只能来自显式 pause 调用。

## 后果

- agent 可以 fire-and-forget；之后用 `skyget watch <id>`、`skyget statusline`
  或 MCP 看进度。
- 遗留任务仍要有人收：`skyget top` 可见，取消/删除走 daemon 接口。
