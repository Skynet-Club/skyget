# 一致性测试清单（0.24.15）

“基础设施”的标志不是功能列表，而是**别人能照着契约实现客户端，并拿一套测试验它**。
这份清单把 `docs/api-v1.md` 的每一条承诺映射到现有测试；新增契约必须先加一行。

## 跑法

```bash
cargo test --all-features --lib --bins         # 单元：锁/分类/幂等/调度/事件/MCP 协议
cargo run -- selftest                          # 内核 12 条行为断言（不联网）
scripts/agent-api-e2e.sh                       # 真 daemon + 真 HTTP 源 + token + MCP + CLI
scripts/agent-integration-e2e.sh               # 真 MCP stdio：want/status/watch/dedupe + CLI
scripts/detach-e2e.sh                          # --detach / 客户端 SIGKILL 后任务继续
scripts/download-credential-e2e.sh             # Bearer 凭证 / 白名单 / 不落值
scripts/quota-e2e.sh                           # per-agent 每日字节配额边界
scripts/token-scope-e2e.sh                     # 两个 token：任务与 pin 隔离
scripts/webhook-e2e.sh                         # done/failed 终态 webhook + HMAC
scripts/restart-e2e.sh                         # kill -9 → 对账 → 续传 → 墓碑跨重启
scripts/daemon-lifecycle-e2e.sh                # 残留状态 / status / stop / 空闲退出
scripts/upload-resume-e2e.sh                   # tus 断点续传
scripts/upload-recreate-e2e.sh                 # tus 远端 404 后重建
scripts/upload-cancel-e2e.sh                   # 上传取消不复活
scripts/statusline-e2e.sh                      # CLI/HTTP 状态栏 + 来源过滤 + 取消后空闲
scripts/http-limits-e2e.sh                     # 超长请求行/头 → 431；正常请求仍 200
scripts/hls-e2e.sh                             # HLS 编排 / 协议白名单 / 受限出网拒绝
scripts/inflight-e2e.sh                        # 同 sha 并发：后来者等待并秒传
scripts/waiter-stuck-e2e.sh                    # 等待者不静音其他任务的卡住提示、提示不带对端 id
scripts/lib.sh                                  # e2e 公共样板（端口/临时目录/ok-no/trap/等 health）
scripts/url-forms-e2e.sh                      # IPv6 / fragment / 查询串 / file:data: / 302→元数据
scripts/curl-shim-e2e.sh                     # skyget curl：接管、透传、凭证映射、退出码
scripts/cli-surface-e2e.sh                    # events / 用法错误 / statusline / export-import 字段
scripts/cli-precheck-e2e.sh                   # 预检与真提交同一套规则、补全、show --url
scripts/cli-control-e2e.sh                    # CLI pause / resume / cancel + 短 id + --json 失败
scripts/uds-e2e.sh                             # Unix socket 身份：uid→agent、免 token、TCP 行为不变
scripts/manifest-sign-e2e.sh                    # 清单签名：keygen→sign→install；改内容必失败
scripts/cache-gc-e2e.sh                        # 缓存水位线：清旧块、留落地链接、留刚下完的
scripts/scheduling-e2e.sh                      # 优先级真的改排队顺序 + 阶段耗时进事件流
scripts/soak-e2e.sh                            # 40 并发 / 4 worker / 10 取消
scripts/e2e.sh                                 # 旧 CLI 回归 29 项
python3 scripts/hostile.py                     # 坏服务器矩阵 5 项
python3 scripts/crash.py --rounds 6 --size 16000000 --mode slow3
```

## 契约 → 证据

| 契约 | 证据 |
|---|---|
| `/health` 不鉴权；其它接口无 token 401 | `agent-api-e2e.sh` §1 |
| token→agent 身份覆盖请求体 `source` | `agents::tests::parses_pairs_and_rejects_bad_names`、`agent-api-e2e.sh` §2（自称 agent:e2e，台账是 agent:claude） |
| per-agent 落地根/白名单/上传根生效 | `agents::tests::per_agent_policy_overrides_only_the_named_agent`、`agent-api-e2e.sh` §16 |
| per-agent 并发上限 | `scheduler::tests::per_source_limit_overrides_the_global_one` |
| `priority` 只影响排队顺序、不破坏公平 | `scheduler::tests::a_higher_priority_source_goes_first`、`scheduler::tests::priority_does_not_break_fairness_between_sources`、`scheduling-e2e.sh` §2 |
| 坏 `priority` 回 `input.bad_priority` | `api.rs` 校验 + `scheduling-e2e.sh` §1 |
| 自然幂等键超过 TTL 后不再 dedupe，响应带 `previous_job` | `api::tests::a_finished_natural_key_only_dedupes_within_the_ttl` |
| `phase` 事件覆盖 probe/transfer/verify/land | `scheduling-e2e.sh` §3 |
| 清单签名验签、fail closed（有签名无公钥 / 有公钥无签名都拒绝） | `manifest::tests::signature_round_trips_and_detects_tampering`、`manifest::tests::keygen_sign_verify_is_a_closed_loop`、`manifest-sign-e2e.sh` |
| 规范化摘要与文件顺序无关 | `manifest::tests::canonical_digest_ignores_file_order_and_whitespace` |
| Unix socket 用内核 uid 定身份；登记映射生效；TCP 仍要 token | `uds::tests::peer_cred_works_on_a_real_socket_pair`、`uds::tests::maps_uid_and_gid_but_ignores_junk`、`uds-e2e.sh` |
| 302 打向云元数据地址在**那一跳**被拒（且不发起连接） | `url-forms-e2e.sh` §8、`security::tests::allowing_private_does_not_also_open_cloud_metadata` |
| IPv6 字面量能正常下载；元数据 IPv6 端点也被拒 | `url-forms-e2e.sh` §4、`security::tests::ipv6_literals_are_parsed_not_mistaken_for_dns_failures` |
| `file://` / `data:` 拒绝且不读本机文件 | `url-forms-e2e.sh` §5 §6 §10 |
| `skyget curl -o <文件>` 与 curl 字节一致、`-o -`/POST 透传、Bearer 映射且不落值 | `curl-shim-e2e.sh` §1–§11 |
| MCP `skyget_check` 把 4xx 当答案返回（带 checks / next_action） | `mcp::tests::initialize_and_tools_list_are_well_formed` |
| 通用 skill 与各宿主副本逐字节一致（防漂移） | `integrations/skill_sync.py --check`（CI） |
| 按需拉起：客户端在服务停止后能把 daemon 拉起来（systemd 优先） | `daemon-lifecycle-e2e.sh` §1 |
| `daemon status` 的 pid / socket 可用性与现场一致 | `daemon-lifecycle-e2e.sh` §2 |
| `daemon stop` 能停直接拉起的进程并清理 pid/socket | `daemon-lifecycle-e2e.sh` §3 |
| 重复 stop 可清理死 pid / 失效 socket | `daemon-lifecycle-e2e.sh` §4 |
| 空闲退出：`SKYGET_IDLE_EXIT` 到点后 health/pid/socket 全部消失 | `daemon-lifecycle-e2e.sh` §5 |
| 默认不开机自启（`is-enabled` = disabled），`--with-service` 才 enable | `install-desktop.sh` 分支 + 手工验收 |
| pid 文件带 `.pid`；SIGTERM 后 pid/socket 都被清理 | `sys::tests::pid_file_keeps_the_pid_suffix`、`daemon-lifecycle-e2e.sh` §3 |
| `watch` / `events` 在 daemon 重启后自动重连 | 手工验收（kill -9 → 重连 3 次 → 重新接上 → done） |
| 崩溃循环只在"连续短命启动"时触发 | 手工验收（单次重启自动续传；连续秒杀才判循环） |
| 429/503 按 `Retry-After` 退避（无该头退回默认节奏） | `engine::tests::retry_after_header_becomes_our_retry_hint`、`engine::tests::retry_after_parses_seconds_and_clamps_absurd_values` |
| `/health` 自述 `capabilities`（agent 不读 skill 也能探测） | `api::tests::health_reports_version_and_capabilities`、`agent-api-e2e.sh` §1 |
| `POST /check` 与 `POST /jobs` 共用 `preflight`（预演不撒谎） | `api::tests::check_reports_which_policy_item_denied`、`api::tests::check_refuses_unsupported_schemes_without_touching_the_network`、`api::tests::check_requires_sha_for_mirrors`、`api::tests::check_applies_upload_roots`、`cli-precheck-e2e.sh` §6 |
| `check` 的结论随策略变化（严格 vs 放开私网） | `cli-precheck-e2e.sh` §2 |
| `completions` 生成三种 shell；`show --url` 按地址查 | `cli-precheck-e2e.sh` §7 §8 |
| `--timeout` 到点退出（1 / `cli.timeout`），不取消任务 | `cli-surface-e2e.sh`（超时用例）+ 手工矩阵 |
| `events` 只输出 NDJSON，`--follow` 到终态退出 | `cli-surface-e2e.sh` §1 §2 |
| 用法错误：退出码 2，`--json` 时 stdout 一行 `cli.usage` | `cli-surface-e2e.sh` §3 |
| `statusline --json` 与 `--format json` 同一份输出 | `cli-surface-e2e.sh` §4 |
| `export/import --json` 的字段是 `blobs/bytes/skipped` | `cli-surface-e2e.sh` §5 |
| CLI `pause/resume/cancel` 走 daemon、短 id、`--json` 一行 | `cli-control-e2e.sh` |
| `--json` 失败时 stdout 仍是一行 JSON（`cli.failed`） | `cli-control-e2e.sh` §5 + 手工矩阵 |
| `list` 截断时明说还有多少，筛选取在 SQL 里 | `store::list_page` + `cli-control-e2e.sh` 之外的 `skyget list` 手工验收 |
| `ensure` 幂等：已经正确的文件不联网、不写盘；清单外文件不删 | `manifest::tests::ensure_skips_what_is_already_correct`、`manifest::tests::ensure_does_not_delete_files_it_did_not_install` |
| 缓存水位线：超上限清最老；pin/落地链接/最小年龄不动 | `prune::tests::budget_*`、`prune::tests::pinned_blob_is_kept_even_if_old_and_unlinked`、`cache-gc-e2e.sh` |
| `GET /agents` 只回身份/策略/并发，不回 token；agent token 只看自己；顶层带 daemon 全局策略 | `api::tests::agents_view_is_scoped_to_the_caller_and_never_exposes_tokens`、`agent-api-e2e.sh` §19/§23 |
| 新 GUI/CLI 能解码旧 daemon（0.18）的 TaskView | `model::tests::old_daemon_taskview_without_new_fields_still_decodes` |
| 托盘「退出」真的结束 GUI 并释放单实例 pid | 0.21.1 真实 DBusMenu Event 测试（`dbus_rc=0`、进程退出、pid 删除）；用户手动确认 |
| 注册表模式主 token 仍可用，且看全部 agent、无 agent 身份 | `agent-api-e2e.sh` §19（master 看 claude+codex；codex token 只看自己） |
| 同一幂等键返回同一条任务 `deduped:true` | `agent-api-e2e.sh` §2；`store::tests::idempotency_claim_is_atomic_under_concurrency` |
| 删除任务释放幂等键 | `store::tests::removing_a_task_releases_its_idempotency_keys` |
| 事件 `seq` 连续、`since` 排他、`next` 可续读 | `agent-api-e2e.sh` §3；`store::tests::events_are_append_only_and_replayable_from_a_seq` |
| 命中缓存时 `cached:true`、不重复走网络 | `agent-api-e2e.sh` §4；`selftest`「秒传不重下」 |
| 缓存命中落进一个还不存在的目标目录 | `agent-api-e2e.sh` §4（新目录）/§18（out8） |
| `path` 是最终文件而不是目录 | `agent-api-e2e.sh` §4/§6；`engine::tests::early_dest_treats_an_existing_directory_as_a_directory` |
| MCP `initialize` / `tools/list` / `want` / `watch` / `retry` / `cancel` / `agents` | `mcp.rs` 单测；`agent-api-e2e.sh` §5 |
| Agent 经 MCP 提交/状态/事件/dedupe，CLI 走同一台账并命中共享缓存 | `scripts/agent-integration-e2e.sh` **5/5** |
| agent 大文件路由规则与可复用 skill | `CLAUDE.md`、`.claude/skills/skyget-transfer/SKILL.md`（规则本身无法自动测，调用路径由上一条覆盖） |
| CLI `--as` → daemon、stdout 一行 JSON | `agent-api-e2e.sh` §6 |
| 跨进程分片锁（第二个写者得到 `PartBusy`） | `lock::tests::second_acquire_fails_until_first_drops` |
| sha256 必须是 64 位十六进制；畸形 sha 不 panic、不读本地文件 | `security::tests::sha256_must_be_exactly_64_hex`、`engine::tests::blob_of_rejects_path_traversal_and_non_ascii`、`agent-api-e2e.sh` §12 |
| dest 展开 `~`/相对路径且保留目录语义 | `engine::tests::task_dest_expansion_preserves_directory_semantics`、`api::tests::submit_expands_tilde_before_checking_or_landing` |
| 悬空符号链接不被跟随 | `security::tests::dest_roots_confine_landing_and_block_dotdot_and_symlink_escape`、`agent-api-e2e.sh` §12 |
| 被篡改/符号链接的缓存块不被信任 | `agent-api-e2e.sh` §13/§14（daemon 重算 sha 并替换） |
| 多源回退：说谎镜像 → 下一个源，最终 sha 一致 | `agent-api-e2e.sh` §18 |
| 多源分片按源隔离，不跨源复用未校验字节 | `engine::tests::multi_source_parts_are_per_source_not_shared`、`agent-api-e2e.sh` §18 |
| 多源缺 sha 被拒 | `agent-api-e2e.sh` §18 |
| `SKYGET_AGENTS` 写错时拒绝启动（fail closed） | `agents::tests::empty_env_means_legacy_mode_but_bad_config_fails_closed` |
| 无身份不能自称 `agent:<name>` 套策略 | `api::tests::unauthenticated_callers_cannot_claim_an_agent_source`、`agent-api-e2e.sh` §19 |
| CAS export 目标被目录占位时明确失败、不写 manifest | `cas::tests::export_refuses_a_directory_squatting_on_the_blob_name` |
| 上传 PUT / tus 端到端、远端 sha 一致 | `agent-api-e2e.sh` §15 |
| tus Location 必须同源且过出网策略；目标端口一个请求都收不到 | `upload::tests::tus_location_must_be_same_origin_and_pass_egress_policy`、`agent-api-e2e.sh` §20（带 Bearer 凭证 + 请求记录器） |
| tus 远端 404/410 后重建并完成 | `scripts/upload-recreate-e2e.sh`（重启上传服务器清空状态） |
| 上传取消后记录不复活、sidecar 不重生 | `scripts/upload-cancel-e2e.sh`（慢速 tus 传 16MB 后 cancel，观察 6 秒） |
| 预签名 URL 在 API 视图/事件里脱敏；逗号/分号/大写 scheme 也不漏 | `security::tests::presigned_urls_are_redacted_but_plain_urls_are_left_alone`（4 组边界）、`agent-api-e2e.sh` §21 |
| 单镜像（sources 不含主 url）也走多源回退 | `engine::tests::a_single_mirror_still_counts_as_multi_source` |
| `allowed_hosts` 的 null/[] 在 CLI 显示不同 | `skyget::tests::host_policy_display_distinguishes_null_from_empty` |
| 未注册来源在 `/metrics` 收敛为 `other` | `api::tests::metrics_collapse_unregistered_sources_to_one_series`、`agent-api-e2e.sh` §10 |
| 上传源目录默认禁止、根外拒绝 | `security::tests::upload_sources_are_confined_and_must_be_regular_files`、`agent-api-e2e.sh` §15 |
| 上传凭证不能读任意 env/文件 | `upload::tests::credentials_are_not_an_arbitrary_secret_reader` |
| 缓存 export/import 往返 + 坏块拒绝 | `cas::tests::export_then_import_round_trips_and_detects_tamper`、`agent-api-e2e.sh` §17 |
| tus 在 daemon 重启后续传并完成 | `scripts/upload-resume-e2e.sh`（16MB 处 kill -9） |
| 8 个并发同幂等键只落 1 条任务 | `api::tests::concurrent_same_idempotency_key_creates_exactly_one_task` |
| 并发 resume 只起 1 个引擎 | `api::tests::concurrent_resume_of_the_same_id_only_starts_one_engine` |
| job panic 不死 worker、不留脏 running | `scheduler::tests::a_panicking_job_does_not_kill_the_worker`、`api::tests::job_guard_turns_a_panic_into_internal_panic_and_clears_the_handle` |
| 取消/删除后的终态不复活 | `store::tests::save_if_exists_never_resurrects_a_deleted_task`、`api::tests::cancel_unknown_id_is_404_and_external_process_is_409` |
| 重启后 pending/running 对账 + 事件 | `api::tests::reconcile_marks_queued_orphans_interrupted`、`scripts/restart-e2e.sh` |
| `/jobs/changes` 游标不跳过变更 | `api::tests::changes_cursor_advances_to_the_last_returned_row_not_wall_clock` |
| 删除墓碑跨 daemon 重启仍对增量客户端可见 | `store::tests::tombstones_survive_a_daemon_restart`、`restart-e2e.sh` §5 |
| 保留策略先清进度事件、不误删状态事件 | `store::tests::retention_prunes_progress_before_status_events` |
| 失败分类可机读 | `engine::tests::error_codes_are_machine_actionable` |
| 缓存 pin 的创建/列表/解除 | `store::tests::pins_round_trip_and_protect_shas`、`api::tests::pins_can_be_created_listed_and_removed`、`agent-api-e2e.sh` §8 |
| `/cas` 整取、Range 206、未知 404 | `api::tests::cas_range_parser_handles_open_suffix_and_invalid`、`agent-api-e2e.sh` §9 |
| `/metrics` 可抓取且需 token | `agent-api-e2e.sh` §10 |
| `skyget top` 读 daemon 台账 | `agent-api-e2e.sh` §11 |
| `skyget agents --json` 与人类可读机群视图 | `agent-api-e2e.sh` §24 |
| `skyget doctor` 报目录/策略/daemon/身份；不可达/200-坏 JSON 都非 0，`--json` 一行 | `agent-api-e2e.sh` §23（含 `scripts/bad-daemon.py`） |
| GUI 机群视图读 `/agents` 并展示策略/并发 | 手工像素验收 `docs/screenshots/agent-fleet.png`（`SKYGET_GUI_VIEW=fleet` + 自截图）；无自动化 UI 测试 |
| prune 不清被 pin 的内容 | `prune::tests::pinned_blob_is_kept_even_if_old_and_unlinked` |
| 私网/元数据地址拒绝 | `security::tests::private_and_metadata_addresses_are_rejected`、`strict_policy_blocks_loopback_and_metadata_but_allows_public_ip` |
| dest roots 挡 `..` 与符号链接逃逸 | `security::tests::dest_roots_confine_landing_and_block_dotdot_and_symlink_escape` |
| URL 带账号密码拒绝 | `security::tests::credentials_in_url_are_rejected` |
| daemon 并发上限/公平/有界队列 | `scheduler.rs` 3 条单测；`scripts/scheduler-loadtest.py`（2 worker / 6 慢任务 / 2 agent，实测峰值 2、单 agent 峰值 1、出现排队） |
| 40 并发 / 4 worker / 10 取消的台账不串、无幽灵 | `scripts/soak-e2e.sh`（30 个完成落地 sha 一致；取消记录 404；无 running/pending） |
| 队列满 429 + `retry_after_ms`、释放幂等键 | `api::tests::queue_full_returns_429_and_releases_the_idempotency_key` |
| 排队任务 pause/resume/cancel 不复活 | `api::tests::queued_pause_then_resume_reuses_the_same_run`、`cancelled_run_cannot_be_resumed_even_if_still_queued`、`forget_refuses_a_job_that_is_still_queued_or_running` |
| 连接数上限 | 代码路径；待补：并发 129 条连接的压力测试 |
| 浏览器 Origin / 非 JSON POST 被拒 | 代码路径（`api::handle`）；待补：带 Origin 的 socket 级测试 |
| 缓存命中也要过 dest 边界 | `agent-api-e2e.sh` §7（先正常落地进缓存，再从允许目录外重放同一 sha） |
| 崩溃后进度不回退、最终 sha 一致 | `scripts/crash.py` |
| 坏服务器不静默产出坏文件 | `scripts/hostile.py` |
| `/statusline` 选 running>pending>interrupted、source 过滤、token 隔离 | `api::tests::statusline_endpoint_*`、`token-scope-e2e.sh` §5、`statusline-e2e.sh` |
| `/jobs?active=1` 排除已完成任务 | `api::tests::jobs_active_filter_excludes_finished_tasks` |
| 请求行/头行 16KiB、头 100 个，超限 431 | `api::tests::http_line_reader_*`、`scripts/http-limits-e2e.sh` |
| 幂等键按来源隔离，同 agent 重试仍命中 | `api::tests::idempotency_key_is_scoped_per_agent`、`token-scope-e2e.sh` §6 |
| 取消/删除后事件仍可审计（主 token/本机） | `api::tests::deleted_jobs_keep_their_events_for_audit`、`upload-cancel-e2e.sh` |
| 缓存命中不计每日配额 | `store::tests::quota_usage_ignores_cache_hits` |
| 配额语法错误全局 fail closed | `quota::tests::malformed_config_fails_closed_for_everyone` |
| 清单必须带 sha/dest、URL 过出网边界、mode 去 setuid | `manifest::tests::manifest_files_require_sha_and_dest`、`load_rejects_non_regular_manifest` |
| 清单 sha 大写/带空白先归一，缓存路径不漂移 | `manifest::tests::load_normalizes_manifest_sha` |
| per-agent 策略字段名写错拒绝启动（不静默继承） | `agents::tests::parses_pairs_and_rejects_bad_names` |
| file 凭证文件必须 0600 | `credential::tests::file_credentials_must_not_be_group_or_world_readable` |
| webhook URL/error 不泄露 query/userinfo；可选 HMAC | `webhook::tests::payload_redacts_query_and_error_text`、`hmac_matches_rfc4231_case_2`、`webhook-e2e.sh` §1/§2b |
| 拒绝/失败计数进 `/metrics` | `token-scope-e2e.sh` §7、`api::tests::denial_counter_classifies_response_codes` |
| HLS 秒进度 / 状态栏按秒画条 / token 隔离 | `api::tests::seconds_endpoint_reports_hls_progress_and_scopes_token`、`skyget::tests::hls_statusline_uses_seconds_when_bytes_are_unknown` |
| HLS/DASH 在受限出网下默认拒绝，opt-in 才放行；ffmpeg 输入协议白名单 | `security::tests::stream_egress_is_fail_closed_under_restrictions`、`scripts/hls-e2e.sh` |
| 同 sha 并发：一个后来者等待后秒传，`cached=true` | `scripts/inflight-e2e.sh` |
| 等待同内容时：只有等待者不报“线路卡住”，其他任务照常报；提示不带对端 id | `scripts/waiter-stuck-e2e.sh` |
| CLI `list/show --json` 如实带出 `cached` | `scripts/inflight-e2e.sh`（B 的 show） |
| 同 URL 已下过但 `--to` 换目录：必须落到新目录 | `scripts/e2e.sh` §2b |
| 人类输出不执行 agent 提供的终端转义序列 | `skyget::tests::display_safe_strips_terminal_escapes` |

## 还没被测试钉住的承诺（别当成已完成）

1. 连接数上限的真实压力测试（并发 129 条连接）。
2. DNS rebinding 的 TOCTOU（已知残余风险，不是测试能解决的，需要 resolver 改造）。
3. HLS 子分片由 ffmpeg 自己取：受限出网默认拒绝（有单测），但 opt-in 后仍不受
   逐条策略约束；BT peers 的出网边界未覆盖，`.torrent` URL 的 redirect 由
   librqbit 跟随、不经策略。
4. 上传的 S3 multipart / 远端 digest 回执（tus/PUT 已有）。
5. 多源回退的「同源网络失败后续传」目前只有代码路径，没有专门 e2e；
   说谎镜像（内容错）回退已有 §18，跨源不共享分片有单测。
6. 每 agent 带宽配额与缓存容量配额（并发上限、每日字节配额已有）。
7. 保留策略的**长期运行**验证（维护线程每小时跑，单测只验证一次 prune）。
8. Metalink 清单解析（`sources` 已可手工给出）。
9. 上传取消在**哈希中/阻塞请求中**的即时打断没有 e2e（传输中取消已有
   `upload-cancel-e2e.sh`；阻塞式 HTTP 请求只能等超时，不能让 cancel 立即打断）。
10. 队列满/进程退出（Closed）分支的幂等键释放没有端到端测试（只有代码路径）。
11. SQLite 台账仍保存真实预签名 URL（同 uid 可读）；API/事件已脱敏，磁盘隔离未做。
12. `discard_task_parts` 遇到 PartBusy 时继续清理其它源的逻辑没有专门测试
    （代码路径已改，但没有稳定制造 PartBusy 的 e2e）。
