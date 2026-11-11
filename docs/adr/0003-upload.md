# ADR 0003 · 上传：和下载共用一套数据面，但先解决三个安全问题

- 状态：已实现 MVP（0.20：PUT + tus；S3 multipart / 远端 digest 待做）
- 相关：`src/upload.rs`、`src/model.rs`、`src/api.rs`、`src/scheduler.rs`、`src/security.rs`

## 为什么不能直接加一个 `PUT`

下载是「把远程内容带到本地」，上传是「把本地内容送到远程」。后者一旦交给 agent，
风险方向反过来：

1. **文件外传**：agent 可以让 daemon 把 `~/.ssh/id_rsa` 上传到任意地址。
   下载侧只限制「写到哪」，上传侧必须限制「能读哪」——`SKYGET_UPLOAD_ROOTS`，
   默认空（= 禁止上传），由部署方显式配置。
2. **凭证泄漏**：S3 预签名 URL 本身就是凭证；写进台账、日志、错误消息都会泄漏。
   台账必须存**脱敏后的 URL**，真正的 URL 只在任务运行期从 secret store 取。
3. **目的地不可验证**：下载有 sha256 做硬门槛；上传的成功不能只看 HTTP 200。
   必须用远端返回的 digest（S3 `x-amz-checksum-sha256`、tus checksum extension）
   或回读校验，拿不到就如实标 `trust: none`，不假装「上传成功且校验通过」。

## 形态

复用 `Task`，新增：

- `direction`: `download` | `upload`（默认 download，老库迁移为 download）
- 上传时 `dest` = 本地源文件路径；`url` = 目标地址；`sha256` = 本地文件指纹
- `sink`: `http-put` | `tus` | `s3-multipart`（先做前两个）
- 凭证：`credential_ref`（`env:NAME` / `file:/path`），**不落库**，只存引用

API：

```http
POST /uploads
{
  "path": "~/outputs/checkpoint.safetensors",
  "url": "https://upload.example.org/files/checkpoint",
  "protocol": "tus",
  "sha256": "…",              // 本地文件指纹；不符不传
  "credential_ref": "env:UPLOAD_TOKEN",
  "idempotency_key": "run-7f3/upload"
}
→ 202 {"id": "…", "events": "/jobs/<id>/events"}
```

## 传输实现

- **HTTP PUT**：单次流式；`Content-Length` + 读取器计数进度；2xx 后校验远端 digest（若有）。
  不可续传：失败就是失败，`retryable` 按网络/限流分类，重跑从头来。
- **tus 1.0**：`POST` 创建（`Upload-Length`、`Upload-Metadata`）→ `PATCH` 分块
  （`Upload-Offset`）→ `HEAD` 查偏移。`Location` 和 offset 落 sidecar（与 `.part`
  同级），进程被杀可续。`Upload-Checksum` 支持时逐块校验。
- **S3 multipart**：需要 SigV4 或预签名分片 URL；放在 tus 之后。
  每个 part 的 checksum 必须验证，`CompleteMultipartUpload` 后核对 ETag/checksum。

## 安全与审计

- `SKYGET_UPLOAD_ROOTS`：源文件必须在允许根目录内；canonicalize + 符号链接检查，
  与 `SKYGET_DEST_ROOTS` 同一套实现。
- 目标 URL 走与下载相同的出网策略（私网/元数据默认拒绝），凭证只从 `credential_ref` 取。
- 台账/事件/日志里的 URL 脱敏：query 里的签名参数替换成 `[redacted]`；
  需要重试时从任务内存/secret store 取原值。
- 事件类型复用：`queued` / `progress` / `verifying` / `done` / `failed`；
  `bytes_done` 表示已发送字节。
- 审计：谁（source）、读了哪个本地文件（hash+path）、传到哪（脱敏 URL）、
  远端返回什么校验结论。

## 验收（实现时必须先写）

1. 允许根目录外的源文件 → `policy.upload_source_denied`，不发起任何网络请求。
2. 源文件 sha256 与调用方给的不符 → 失败，不发起上传。
3. tus 上传到一半 `kill -9` → 重跑从 offset 接着传，最终远端内容 sha 一致。
4. 远端返回 200 但 digest 不符 → 任务失败，不报「上传成功」。
5. 预签名 URL 不出现在**API 响应 / 事件 / CLI 输出**里（0.20 部分完成：
   TaskView.url/sources/error 与事件 data 已替换 `[redacted]`；SQLite 台账内仍存
   真实 URL 供重试，见“实现边界”）。
6. 目标地址是 `169.254.169.254` → `policy.egress_denied`。
7. tus 服务器返回跨源 `Location` → `policy.egress_denied`，且不发 HEAD/PATCH。
8. tus 远端对象 404/410 → 清 sidecar、重建上传一次，不永久卡死。

## 0.20 的实现边界

- 已做：PUT、tus 1.0（创建/HEAD/PATCH/重启续传/404 重建）、`SKYGET_UPLOAD_ROOTS`、
  env 白名单 + secrets 目录、目标出网策略、tus Location 同源校验、事件/台账/调度/幂等复用、
  API/事件/CLI 的 URL 脱敏、上传取消不复活、单请求最低速度超时。
- 未做：S3 multipart（SigV4/预签名分片）、远端 digest 回执、按带宽/agent 的上传配额。
- 磁盘隔离：SQLite 台账里仍有真实预签名 URL（重试要用）。同一 uid 的进程本来就能
  读 daemon 的数据库/内存，所以这不是跨用户边界；把 daemon 放进独立容器/账号时，
  数据库文件才随之隔离。更强的方案（真实 URL 放 0600 sidecar、台账只存引用）
  仍在路线图上。
- 远端内容校验：PUT/tus 没有统一回执，成功只代表“远端收完了字节”，所以 `trust=local`，
  不写成 `pinned` 或“校验通过”。S3 multipart 的 checksum 是下一步要补的真校验。
- 取消：阻塞式 HTTP 请求发出去后不能被 signal 立即打断；cancel 会等在途请求
  完成或超时。上传线程不会因为取消而复活任务行（save_if_exists + 分块哈希）。
