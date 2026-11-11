---
name: skyget-transfer
description: Use for large or reliability-sensitive transfers — models, datasets, images, ISOs, build artifacts, or any download/upload that is over ~100MB, of unknown size, needs resume, needs sha256 verification, should run in the background after your shell exits, is shared between several agents on one machine, or may already be cached locally (resolve/curl turn a repeat fetch into zero network). Routes the payload through the local 天网盒子 / Skynet Data Box daemon (`skynet-dataplaned`; compatibility names remain) instead of curl or wget; curl stays fine for metadata and small files. Also use when skyget itself is missing or outdated and has to be installed.
license: MIT
---
# skyget-transfer

Move data through the local **天网盒子 / Skynet Data Box** daemon
(`skynet-dataplaned`; compatibility names remain) instead of curl/wget.
It gives you three things curl does not: **sha256 as a hard gate** (a mismatch fails the
transfer and the file is discarded), a **content-addressed cache** (same content stored
once per machine, so a repeat fetch is zero network), and **resume** that survives
`kill -9`.

The daemon owns the ledger, the cache and the partials; your shell can die and the
transfer keeps going. This file is written to be usable by any agent: the three sections
below are **alternatives**, not steps — take the one your host can actually do.

## When to use

- The transfer is large (roughly >100 MB) or of unknown size.
- It must survive the end of your session, needs sha256 verification, or several agents
  may share the same content.
- You are uploading a model, checkpoint, dataset or build artifact.
- The user says "resume / 断点续传 / verify / run it in the background / 接着上次下".

Metadata (HEAD, checksum file, mirror speed) and small one-off files are still fine with
curl; do not use curl for the payload when any of the above applies.
## A. MCP client — most portable (no shell, no paths, no OS assumptions)

`skynet-mcp` exposes 11 tools:

- `skyget_check` — preflight `{ok, checks[], code, next_action}`; **a 403 is an answer, not a fault**.
- `skyget_want` — submit `{url, dest, sha256, sources, credential_ref, idempotency_key, agent}`.
- `skyget_watch` — follow `{job_id, since, wait_ms}`; continue with the returned `next`.
- `skyget_status` / `skyget_active` — one job / the current active job.
- `skyget_upload` — upload (`put|tus`; source must be under `SKYGET_UPLOAD_ROOTS`).
- `skyget_pause` / `skyget_resume` / `skyget_cancel` / `skyget_retry` — transfer control.
- `skyget_pin` / `skyget_unpin` / `skyget_agents` — cache pin / fleet view.

Stop when `skyget_watch` says `done=true`. Branch on `error_code`; do not invent a cause.

## B. Shell

```bash
skyget check <URL> -o <dir>                                    # exit 0 = would be accepted
skyget --json get <URL> -o <dir> --sha256 <hex> --as <agent> --detach
skyget watch <job_id> --since 0                                # or: events <id> --follow
skyget pause|resume|cancel <id>                                # short ids work
skyget ensure <manifest.json> --key <pubkey>                   # idempotent: adds/replaces only
```

`--as` belongs to the `get` subcommand: `skyget get <URL> --as x` ✅,
`skyget --as x get <URL>` ❌ (exit 2).
## C. HTTP only

```http
POST /check   {"url":"...","dest":"...","sha256":"..."}
POST /jobs    {"url":"...","dest":"...","sha256":"...","source":"agent:name"}
GET  /jobs/<id> | /jobs/<id>/events?since=<seq>&wait_ms=25000
GET  /health  {"ok":true,"version":"...","capabilities":["http","hls","manifest",...]}
GET  /cas/<sha256>            # cache read-through, Range supported
```

Identify with `Authorization: Bearer $SKYGET_API_TOKEN`, or with a mounted Unix socket
(`SKYGET_API=unix:/path/to/daemon.sock`) where the kernel supplies the uid. Never put a
token in arguments, logs or output.

## Three hard rules

1. **stdout is always one line of JSON** — success, task failure, `cli.failed`,
   `cli.usage` (exit 2), `cli.timeout` (exit 1). Parse it; do not scrape stderr.
2. **Never pipe skyget** (`| tail`, `| head`, `| grep`): the pipe buffers until exit and
   the user sees nothing for the whole transfer. For progress use `--progress stderr`,
   `--json`, or the event stream.
3. **Without a sha256, never claim "verified"**; uploads are `trust=local`, never
   "verified remotely".

## When it fails

`part.busy` / `network.*` / `http.throttled` → retryable; wait `retry_after_ms` or
`retry <id>`. `verify.mismatch` / 4xx → change the source, do not retry.
`policy.egress_denied` / `policy.host_denied` → private host or missing allowlist entry.
`policy.metadata_denied` → cloud-metadata address; `SKYGET_ALLOW_PRIVATE=1` does **not**
open it. Daemon unreachable → `skyget doctor`; do not fall back to unverified curl.
## If it is not installed

`skyget --version || <repo>/scripts/install.sh`, then `skyget doctor`. After swapping
binaries **restart the daemon** (watermark, priority, `phase` events and the Unix socket
are daemon-side). Platform details are in `REFERENCE.md`.

## Deeper — `REFERENCE.md` (same directory): full command table, env vars, error-code table, honest
limits, and how each host picks this file up (Claude Code, OpenCode, Codex, Hermes, pi,
Copilot, anything that reads `AGENTS.md`). No shell and no network? Use section A, or say
plainly that you need a transfer tool.
