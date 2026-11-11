# Skynet Dataplane

An agent-native data plane for a single machine: downloads, uploads, verification,
a content-addressed cache, scheduling, and per-agent policy. One daemon, one ledger,
many agents.

## Platform support

**0.24.15 officially supports Ubuntu Linux x86_64 only.**

- The published artifact is `skynet-dataplane_0.24.15_amd64.deb`.
- It was built and validated on Ubuntu 26.04.1 LTS x86_64 and requires glibc 2.39+.
- Ubuntu 22.04 and older cannot run this binary.
- Ubuntu 24.04 meets the ABI floor, but was not separately release-certified for 0.24.15.
- Native Windows is not supported. macOS currently has exploratory source compatibility,
  not an official package or support commitment.
- Ubuntu under WSL2 follows the Linux path; it is not native Windows support.

See [platform support and roadmap](docs/platform-support.md) for the current matrix,
known gaps, and planned macOS/Windows work.

## Why another downloader?

- **Shared by agents**: one daemon, one ledger, one cache. Once content is in the
  cache, another agent gets it without downloading it again. A single concurrent
  follower (same sha) can also wait for the in-flight download and then reuse it;
  further duplicates or long waits still download in parallel.
- **Jobs outlive sessions**: `--detach` submits and returns; the daemon owns the job.
  Kill the client, the download keeps going.
- **Verification is a hard gate**: `--sha256` mismatch fails and discards the file.
- **Progress is visible**: a CLI bar, a status line, and MCP `notifications/progress`.
- **Explicit boundaries**: HTTP(S), torrent, and HLS/DASH over HTTP. No netdisk
  scraping, no DRM, no `thunder://`/`ed2k://` bypass.

## Quick start

Build the CLI, daemon, and MCP server:

```bash
cargo build --release --features http,bt
install -Dm755 target/release/skyget ~/.local/bin/skyget
install -Dm755 target/release/skynet-dataplaned ~/.local/bin/skynet-dataplaned
install -Dm755 target/release/skynet-mcp ~/.local/bin/skynet-mcp
```

Start the daemon:

```bash
skynet-dataplaned --port 7788
# or, in the foreground: skyget serve --port 7788
```

Download with a pinned hash:

```bash
skyget get https://example.org/model.bin -o ~/models \
  --sha256 <64-hex> --as claude --daemon
skyget watch <job_id>
```

Fire-and-forget:

```bash
skyget get https://example.org/model.bin -o ~/models \
  --sha256 <64-hex> --as claude --detach
# the JSON result contains the job id; the task keeps running in the daemon
```

## MCP (Claude Code / Codex / opencode)

```json
{
  "mcpServers": {
    "skynet-dataplane": {
      "command": "/home/you/.local/bin/skynet-mcp",
      "env": {
        "SKYGET_API": "http://127.0.0.1:7788",
        "SKYGET_API_TOKEN": "this agent's own token from SKYGET_AGENTS (omit if unset)"
      }
    }
  }
}
```

On a multi-agent host, give each agent its own token; the master token is for the
GUI/operator. Sharing it collapses task isolation, per-agent policy and quotas.

`skyget_watch` emits MCP progress notifications when the request carries
`params._meta.progressToken`. `skyget_active` returns the current active job (or
`state=idle`) without needing a job id; optionally filter by `source`.

## Status line and TUI

```bash
skyget statusline                        # one ANSI line
skyget statusline --format plain         # no color
skyget statusline --format json --watch  # NDJSON for a TUI plugin
skyget statusline --source agent:claude  # only Claude's jobs
skyget statusline --osc                  # OSC 9;4 terminal progress
```

Claude Code `settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/home/you/.local/bin/skyget statusline",
    "refreshInterval": 1
  }
}
```

HTTP plugins can poll `GET /statusline?source=agent:pi` (Bearer token) for one
active job instead of pulling the full `/jobs` list and re-implementing the
selection rule. For HLS/DASH, the status line uses seconds from
`GET /jobs/<id>/seconds` when total bytes are unknown.

## Private sources

Credentials are references, never stored in the ledger:

```bash
export HF_TOKEN=hf_...
export SKYGET_DOWNLOAD_CREDENTIALS=HF_TOKEN
export SKYGET_ALLOWED_HOSTS=huggingface.co,cdn-lfs.huggingface.co
skyget get https://huggingface.co/.../resolve/main/model.safetensors \
  --credential-ref env:HF_TOKEN -o ~/models --sha256 <64-hex> --as claude --daemon
```

The credential is sent as `Authorization: Bearer` to the primary source only, not to
mirrors. A host allowlist is mandatory when credentials are enabled, otherwise the
job is rejected with `policy.credential_needs_hosts`. `file:` references are restricted to `$SKYGET_HOME/secrets/`. Credentials
apply to direct HTTP(S) downloads; HLS/DASH and torrents reject `--credential-ref`
explicitly instead of silently ignoring it.

HLS/DASH is fetched by ffmpeg, so child playlists/segments do not pass the daemon's
per-URL egress checks (its input protocols are limited to `http,https,tls,tcp`, so
`file://` segments are refused). When egress is restricted (strict mode or
`SKYGET_ALLOWED_HOSTS`), HLS/DASH is refused unless `SKYGET_ALLOW_STREAM_EGRESS=1`
explicitly marks the playlist as trusted.

## Per-agent daily quota

```bash
export SKYGET_AGENT_QUOTAS="claude=10GB,pi=500MB"
```

The engine checks the quota after the size probe and before downloading. Exceeding it
fails with `quota.exceeded` and leaves no file. Cached hits are not counted.

## Completion webhook

```bash
export SKYGET_COMPLETION_WEBHOOK="https://hooks.example.com/skynet"
```

The daemon POSTs a JSON payload on `done` / `failed` / `interrupted` / `cancelled`
(no credentials, URL query stripped, 3 retries, fire-and-forget). Set
`SKYGET_WEBHOOK_SECRET` to add `X-Skynet-Signature: sha256=<hex>` (HMAC-SHA256 of
the raw body). Redirects are not followed. `scripts/webhook-e2e.sh` covers success,
failure, and cancellation.

## Security model

- The daemon binds `127.0.0.1` only; `/health` is the only unauthenticated route.
  With `SKYGET_AGENTS`, each token is an agent identity and jobs/events/pins are
  scoped to it. The master token sees everything; without either token, local mode
  is unauthenticated (any process running as the same user can use it).
- Egress is HTTP(S) only, redirects are re-checked at every hop, private/link-local/
  metadata addresses are denied by default, and `SKYGET_ALLOWED_HOSTS` is enforced.
  Browser-originated requests (an `Origin` header) are rejected.
- `SKYGET_DEST_ROOTS` / per-agent roots bound writes; uploads are disabled until
  `SKYGET_UPLOAD_ROOTS` is configured; symlink escapes are rejected.
- Credentials are `env:` / `file:` references, never stored in the ledger; only the
  primary source receives the `Authorization` header, and using a credential requires
  an explicit host allowlist. HLS/torrent reject credentials.
- Quotas are per UTC day and fail closed; cache hits do not consume quota.
- Webhook payloads contain no credentials, strip URL queries, and redact error text.
  Setting `SKYGET_WEBHOOK_SECRET` adds an HMAC-SHA256 signature; redirects are not
  followed.
- Known residual risks: DNS-rebinding TOCTOU, same-uid processes can read the
  ledger/cache, `/metrics` and `/cas` are machine-wide (any valid token), manifest
  signatures are not implemented, and BT peers are not host-allowlisted.

Full threat model and deployment checklist: [docs/security.md](docs/security.md)
(Chinese).

## Verification

```bash
skyget selftest
scripts/e2e.sh
scripts/detach-e2e.sh
scripts/agent-integration-e2e.sh
scripts/download-credential-e2e.sh
scripts/quota-e2e.sh
```

CI runs the same suite on GitHub Actions.

## Docs

- [README.md](README.md) (Chinese)
- [CLI reference](docs/cli.md)
- [Deployment and policy](docs/deploy.md)
- [Security model](docs/security.md)
- [HTTP contract](docs/api-v1.md)
- [ADRs](docs/adr/)
- [Conformance evidence](docs/conformance.md)
