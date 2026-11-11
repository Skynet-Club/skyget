import { Plugin, usePlugin } from "@opencode/plugin/tui"
import { Show, createSignal, onCleanup } from "solid-js"
import { mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import { dirname, join } from "node:path"

// 数据只有一个来源：天网盒子的 GET /statusline。
// 这里不做任何下载逻辑，只负责把一行 JSON 画成 OpenCode 的 footer 状态。
type View = {
	state: string
	pct: number
	done: number
	total: number
	seconds_done: number
	seconds_total: number
}

const env = (globalThis as any).process?.env ?? {}
const API = env.SKYGET_API ?? "http://127.0.0.1:7788"

// TUI 进程的环境未必有 token（它是从哪个 shell 起的，不由我们决定）。
// 插件和 daemon 跑在同一个用户下，直接读本机 token 文件更可靠；
// 文件名对齐 docs/deploy.md 的约定，读不到再退回环境变量。
function readToken(): string {
	if (env.SKYGET_API_TOKEN) return env.SKYGET_API_TOKEN
	try {
		const raw = readFileSync(join(homedir(), ".config/skynet-dataplane/tokens"), "utf8")
		for (const line of raw.split("\n")) {
			const [k, v] = line.split("=")
			if (k?.trim() === "main" && v?.trim()) return v.trim()
		}
	} catch {}
	return ""
}
const TOKEN = readToken()
const WIDTH = 14
// 分段配色，和 Claude 状态栏对齐：
//   标签 白 / 填充 亮绿 / 空槽 深灰 / 百分比 绿
// 想换配色不用改代码：SKYGET_STATUS_COLOR、SKYGET_STATUS_DIM
const GREEN = env.SKYGET_STATUS_COLOR ?? "#00FF00"
const DIM = env.SKYGET_STATUS_DIM ?? "#3f3f46"

// 存活标记：每秒更新一次。这样"插件到底加载了没有"不用靠肉眼看 footer ——
// 看这个文件的 mtime 就知道。排查 UI 问题时最缺的就是这种证据。
const ALIVE = join(homedir(), ".local/state/skyget/opencode-plugin.alive")
function alive(note: string) {
	try {
		mkdirSync(dirname(ALIVE), { recursive: true })
		writeFileSync(ALIVE, `${new Date().toISOString()} ${note}\n`)
	} catch {}
}

const [demo, setDemo] = createSignal(false)
const ACTIVE = new Set(["running", "pending"])

function split(pct: number): [string, string] {
	const filled = Math.max(0, Math.min(WIDTH, Math.round((pct / 100) * WIDTH)))
	return ["━".repeat(filled), "─".repeat(WIDTH - filled)]
}

function Status() {
	const context = usePlugin()
	const [view, setView] = createSignal<View | null>(null)
	const [err, setErr] = createSignal("")
	// 任务结束后保留一小段"完成"标记：几秒下完的任务如果一闪而过，
	// 等于用户永远看不到这个功能在工作。
	let lastActive = 0

	const tick = async () => {
		try {
			const res = await fetch(`${API}/statusline`, {
				headers: TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {},
				signal: AbortSignal.timeout(800),
			})
			if (!res.ok) {
				setErr(`skyget·${res.status}`)
				alive(`http ${res.status}`)
				setView(null)
				return
			}
			const v = (await res.json()) as View
			setErr("")
			alive(`state=${v.state} pct=${v.pct}`)
			// interrupted（上次被杀的残局）不该常驻状态栏，只认真正在跑的
			const active = ACTIVE.has(v.state)
			if (active) lastActive = Date.now()
			setView(active ? v : null)
		} catch (e) {
			setErr("skyget·down")
			alive(`err ${String(e).slice(0, 60)}`)
			setView(null)
		}
	}

	void tick()
	const timer = setInterval(tick, 1000)
	onCleanup(() => {
		clearInterval(timer)
		alive("disposed")
	})

	const decided = () => {
		const v = view()
		if (v) return { kind: "bar" as const, pct: v.pct }
		if (demo()) return { kind: "bar" as const, pct: 42 }
		if (err()) return { kind: "text" as const, text: err() }
		if (Date.now() - lastActive < 8000) return { kind: "text" as const, text: "skyget·done " }
		return { kind: "text" as const, text: "skyget·idle " }
	}

	return (
		<Show when={decided()} keyed>
			{(d) =>
				d.kind === "bar" ? (
					// 分段配色只能靠并排的多个 <text>：
					//   inline <span> 不认 fg（实测无颜色）
					//   content={t`…`} 会被字符串化成 [object Object]（实测）
					// <box flexDirection="row"> + 每段一个 <text fg> 是两者都不依赖的写法。
					<box flexDirection="row">
						<text fg={context.theme.text.base}>Skygeting: </text>
						<text fg={GREEN}>{split(d.pct)[0]}</text>
						<text fg={DIM}>{split(d.pct)[1]}</text>
						<text fg={GREEN}>{` ${d.pct}% `}</text>
					</box>
				) : (
					<text fg={context.theme.text.base}>{d.text}</text>
				)
			}
		</Show>
	)
}

export default Plugin.define({
	id: "skyget.status",
	setup(context) {
		alive("setup")
		// 加载成功给一次提示：加插件时最常见的困惑就是"到底装上了没"。
		context.ui.toast.show({
			message: "skyget 状态已加载（footer 右侧）",
			variant: "success",
			duration: 4000,
		})
		const unregister = context.ui.slot({
			append: "prompt.footer.status",
			render: () => <Status />,
		})
		// 一条命令就能验证渲染：不用真的下载，也不用等任务出现。
		context.keymap.layer(() => ({
			mode: "global" as const,
			priority: 10,
			commands: [
				{
					id: "skyget.status.preview",
					title: "skyget: 预览进度条",
					group: "skyget",
					palette: true,
					run: () => {
						setDemo(true)
						context.ui.toast.show({ message: "进度条预览 5 秒", duration: 3000 })
						setTimeout(() => setDemo(false), 5000)
					},
				},
			],
			bindings: ["skyget.status.preview"],
		}))
		return unregister
	},
})
