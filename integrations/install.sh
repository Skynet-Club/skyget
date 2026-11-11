#!/usr/bin/env bash
# 把 skyget 接进本机的各个 agent：幂等、可预演、可回滚。
#
#   bash integrations/install.sh --dry-run     # 只说要改什么
#   bash integrations/install.sh               # 真写（先自动备份）
#   bash integrations/install.sh --no-mcp      # 只写指令块，不动 MCP 注册
#   bash integrations/install.sh --repo DIR    # 额外给这个仓库写 .github/copilot-instructions.md
#
# 为什么要有这个脚本：这些配置散在 10+ 个文件里（各 agent 的 AGENTS.md/SOUL.md、
# cli.json、mcp.json、token 文件、systemd env）。手改过一次之后，换机器或某个
# agent 自己改了配置，就只能靠回忆。这里把"该长什么样"写死，重复跑结果一致。
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$HOME/.local/bin}
MCP_BIN="$BIN/skynet-mcp"
TOKENS="$HOME/.config/skynet-dataplane/tokens"
ENVFILE="$HOME/.config/skynet-dataplane.env"
BACKUP="$HOME/.local/state/skyget/install-backup/$(date +%Y%m%d-%H%M%S)"
DRY=0
MCP=1
REPO_INSTRUCTIONS=""

while [ $# -gt 0 ]; do
	case "$1" in
	--dry-run) DRY=1 ;;
	--no-mcp) MCP=0 ;;
	--repo)
		shift
		REPO_INSTRUCTIONS="${1:-}"
		;;
	-h | --help)
		sed -n '2,12p' "$0"
		exit 0
		;;
	*) echo "未知参数：$1" >&2; exit 2 ;;
	esac
	shift
done

say() { printf '  %s\n' "$*"; }
changed=0

# ── 标记块：幂等写入 ────────────────────────────────────────────────────
# 用 HTML 注释当边界：Markdown / TOML / JSON 里都合法，人一眼能看出这是"被管理的段"。
BLOCK_START='<!-- >>> skyget >>> -->'
BLOCK_END='<!-- <<< skyget <<< -->'

# 常驻块的正文来自 skill/SKILL.md 本身（去掉 front-matter 和 H1）——
# 整个仓库只有这一份 skill 内容；AGENTS.md 块、各宿主 SKILL.md 都是它的搬运。
block_body() {
	local src="$REPO/skill/SKILL.md"
	[ -f "$src" ] || {
		echo "缺 $src：skill 没有真源了" >&2
		return 1
	}
	python3 "$REPO/integrations/skill_block.py" "$src"
}

upsert_block() {
	local file="$1" body
	body=$(block_body)
	local new="$BLOCK_START"$'\n'"$body"$'\n'"$BLOCK_END"

	if [ ! -e "$file" ]; then
		if [ "$DRY" = 1 ]; then say "新建 $file（含 skyget 段）"; return; fi
		mkdir -p "$(dirname "$file")"
		printf '%s\n' "$new" >"$file"
		say "新建 $file"
		changed=1
		return
	fi

	# 认「以 <!-- >>> skyget 开头」的任意历史标记，避免早期版本写的段被当成不存在而重复追加
	if grep -q '<!-- >>> skyget' "$file"; then
		local tmp
		tmp=$(mktemp)
		python3 - "$file" "$tmp" "$BLOCK_START" "$BLOCK_END" "$body" <<'PY'
import re, sys
src, dst, start, end, body = sys.argv[1:6]
text = open(src, encoding="utf-8").read()
pat = re.compile(r"<!-- >>> skyget.*?-->.*?<!-- <<< skyget <<< -->", re.S)
new = start + "\n" + body.strip("\n") + "\n" + end
out, n = pat.subn(new, text, count=1)
if n == 0:
    open(dst, "w", encoding="utf-8").write(text)
    sys.exit(3)
open(dst, "w", encoding="utf-8").write(out)
PY
		if [ ! -s "$tmp" ]; then
			say "$file（找不到完整的 skyget 段，跳过，请手工检查）"
			rm -f "$tmp"
			return
		fi
		if cmp -s "$file" "$tmp"; then
			say "$file（已最新）"
			rm -f "$tmp"
			return
		fi
		if [ "$DRY" = 1 ]; then say "更新 $file 的 skyget 段"; rm -f "$tmp"; return; fi
		[ -n "${BACKUP_DONE:-}" ] || { mkdir -p "$BACKUP"; }
		cp "$file" "$BACKUP/$(basename "$file").$$" 2>/dev/null
		mv "$tmp" "$file"
		say "更新 $file"
		changed=1
		return
	fi

	# 没有段：追加
	if [ "$DRY" = 1 ]; then say "追加 skyget 段到 $file"; return; fi
	mkdir -p "$BACKUP"
	cp "$file" "$BACKUP/$(basename "$file").$$" 2>/dev/null
	printf '\n%s\n' "$new" >>"$file"
	say "追加到 $file"
	changed=1
}

echo "== 1. 指令块"
upsert_block "$HOME/.codex/AGENTS.md"
upsert_block "$HOME/.config/opencode/AGENTS.md"
upsert_block "$HOME/.hermes/SOUL.md"
upsert_block "$HOME/AGENTS.md"

echo "== 2. Claude skill"
link="$HOME/.claude/skills/skyget-transfer"
target="$REPO/.claude/skills/skyget-transfer"
if [ -L "$link" ] && [ "$(readlink -f "$link")" = "$target" ]; then
	say "已链接 $link"
elif [ "$DRY" = 1 ]; then
	say "建立软链 $link → $target"
else
	mkdir -p "$(dirname "$link")"
	ln -sfn "$target" "$link"
	say "链接 $link"
	changed=1
fi

echo "== 2b. 原生 skill（各家机制不同，能装的就装）"
SKILL_DIR="$REPO/.claude/skills/skyget-transfer"

# Hermes：目录约定和 Claude 完全一样（<dir>/<name>/SKILL.md）
hlink="$HOME/.hermes/skills/skyget-transfer"
if [ -d "$HOME/.hermes/skills" ]; then
	if [ -L "$hlink" ] && [ "$(readlink -f "$hlink")" = "$SKILL_DIR" ]; then
		say "Hermes 已链接"
	elif [ "$DRY" = 1 ]; then
		say "建立软链 $hlink"
	else
		mkdir -p "$BACKUP"
		ln -sfn "$SKILL_DIR" "$hlink"
		say "Hermes 链接 $hlink"
		changed=1
	fi
fi

# agent 启动钩子：让守护进程跟着 agent 起，而不是开机自启。
#
# 只有支持"会话开始事件"的宿主才需要这一步；其它宿主靠客户端按需拉起
# （`skyget` / `skynet-mcp` 连不上时会 `daemon ensure`）—— 那条对所有宿主都成立。
#
# Claude Code：~/.claude/settings.json 的 hooks.SessionStart
ensure_claude_hook() {
	local f="$HOME/.claude/settings.json"
	local cmd="skyget daemon ensure >/dev/null 2>&1 || true"
	if [ "$DRY" = 1 ]; then
		say "写入 $f 的 hooks.SessionStart（$cmd）"
		return 0
	fi
	mkdir -p "$(dirname "$f")"
	[ -f "$f" ] && cp "$f" "$f.bak-skyget" 2>/dev/null || true
	python3 - "$f" "$cmd" <<'PY'
import json, os, sys
path, cmd = sys.argv[1], sys.argv[2]
doc = {}
if os.path.exists(path) and os.path.getsize(path) > 0:
    with open(path, encoding="utf-8") as f:
        try:
            doc = json.load(f)
        except json.JSONDecodeError:
            print("   ! settings.json 不是合法 JSON，跳过（不猜）")
            raise SystemExit(0)
hooks = doc.setdefault("hooks", {})
starts = hooks.setdefault("SessionStart", [])
want = {"matcher": "", "hooks": [{"type": "command", "command": cmd}]}
if not any(
    any((h or {}).get("command") == cmd for h in (entry or {}).get("hooks", []))
    for entry in starts
):
    starts.append(want)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print("   已加入 hooks.SessionStart")
PY
}
ensure_claude_hook

# 通用 skill：一个文件、一份字节，铺到各主流宿主的位置。
# 谁读哪里见 skill/REFERENCE.md 的「各宿主怎么拿到这份 skill」。
sync_skill() {
	if [ "$DRY" = 1 ]; then
		if python3 "$REPO/integrations/skill_sync.py" --check >/dev/null 2>&1; then
			say "通用 skill 已是各宿主的最新副本"
		else
			say "把 skill/SKILL.md 铺到 ~/.claude、~/.hermes、~/.local/share/skyget/skill"
		fi
		return 0
	fi
	python3 "$REPO/integrations/skill_sync.py" || return 1
}
sync_skill

# Copilot（VS Code）：工作区级读 .github/copilot-instructions.md。
# CLI / coding agent 走 AGENTS.md，第 1 步已经覆盖；这里只补工作区那份。
if [ -n "$REPO_INSTRUCTIONS" ]; then
	dir="$REPO_INSTRUCTIONS/.github"
	if [ "$DRY" = 1 ]; then
		say "写 $dir/copilot-instructions.md（同一份正文）"
	else
		mkdir -p "$dir"
		{ echo "<!-- 由 integrations/install.sh 生成，勿手改；真源是 skyget 仓库的 skill/SKILL.md -->"; block_body; } \
			> "$dir/copilot-instructions.md"
		say "写 $dir/copilot-instructions.md"
		changed=1
	fi
fi

# OpenCode / pi：配置里有 skills 字段，往里加一条路径
add_skill_path() {
	local file="$1" key="$2" path="$3"
	[ -f "$file" ] || return 0
	if python3 "$REPO/integrations/_skill_check.py" "$file" "$key" "$path" 2>/dev/null; then
		say "$(basename "$file") 已有该 skill"
		return 0
	fi
	if [ "$DRY" = 1 ]; then
		say "往 $(basename "$file") 的 $key 加 $path"
		return 0
	fi
	mkdir -p "$BACKUP"
	cp "$file" "$BACKUP/" 2>/dev/null
	python3 "$REPO/integrations/_skill_add.py" "$file" "$key" "$path" &&
		say "$(basename "$file") 已加"
	changed=1
}

add_skill_path "$HOME/.config/opencode/opencode.json" skills "$SKILL_DIR"
add_skill_path "$HOME/.pi/agent/settings.json" skills "$SKILL_DIR"

echo "== 3. token"
if [ -s "$TOKENS" ] && grep -q '^main=' "$TOKENS" 2>/dev/null; then
	say "已有 $TOKENS（不动）"
else
	if [ "$DRY" = 1 ]; then
		say "生成 6 个 token（main + 5 agent）并写 $ENVFILE"
	else
		mkdir -p "$(dirname "$TOKENS")"
		chmod 700 "$(dirname "$TOKENS")"
		gen() { head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 32; }
		main=$(gen)
		{
			echo "# 每 agent 自己的 token。泄露了就换一行再重启 daemon。"
			echo "main=$main"
			for a in claude codex opencode pi hermes; do echo "$a=$(gen)"; done
		} >"$TOKENS"
		chmod 600 "$TOKENS"
		{
			echo "# 天网盒子运行环境（0600）。改完需要重启 daemon。"
			echo "SKYGET_API_TOKEN=$main"
			printf 'SKYGET_AGENTS='
			for a in claude codex opencode pi hermes; do
				printf '%s=%s,' "$(grep "^$a=" "$TOKENS" | cut -d= -f2)" "$a"
			done | sed 's/,$/\n/'
		} >"$ENVFILE"
		chmod 600 "$ENVFILE"
		say "生成 $TOKENS 与 $ENVFILE"
		changed=1
	fi
fi

if [ "$MCP" = 1 ]; then
	echo "== 4. MCP 注册"
	if [ "$DRY" = 1 ]; then
		say "codex / opencode / pi / hermes 各注册一个 skynet MCP（需现有 token）"
	elif [ -s "$TOKENS" ]; then
		tok() { grep "^$1=" "$TOKENS" | cut -d= -f2; }
		# 每个 CLI 都是"先删再加"：不同版本对重复注册的行为不一样，删掉重加最稳。
		codex mcp remove skynet >/dev/null 2>&1
		codex mcp add skynet --env SKYGET_AGENT=codex --env SKYGET_API_TOKEN="$(tok codex)" \
			-- "$MCP_BIN" >/dev/null 2>&1 && say "codex ✓" || say "codex ✗（跳过）"

		opencode mcp add skynet --global --env SKYGET_AGENT=opencode \
			--env SKYGET_API_TOKEN="$(tok opencode)" -- "$MCP_BIN" >/dev/null 2>&1 &&
			say "opencode ✓" || say "opencode ✗（跳过）"

		pi mcp add skynet --env SKYGET_AGENT=pi --env SKYGET_API_TOKEN="$(tok pi)" \
			-- "$MCP_BIN" >/dev/null 2>&1 && say "pi ✓" || say "pi ✗（跳过）"

		hermes mcp remove skynet >/dev/null 2>&1
		printf 'y\n' | hermes mcp add skynet --command "$MCP_BIN" \
			--env SKYGET_AGENT=hermes SKYGET_API_TOKEN="$(tok hermes)" >/dev/null 2>&1 &&
			say "hermes ✓" || say "hermes ✗（跳过）"
	fi
fi

echo
if [ "$DRY" = 1 ]; then
	echo "（预演结束，什么都没写。去掉 --dry-run 执行）"
elif [ "$changed" = 0 ]; then
	echo "全部已是最新，没有改动。"
else
	echo "完成。备份在 $BACKUP"
	echo "daemon 若在跑，重启一次以加载新 token：systemctl --user restart skynet-downloadd"
fi
