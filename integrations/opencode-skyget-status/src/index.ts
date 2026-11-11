import { Plugin } from "@opencode/plugin"
import { Plugin as TuiPlugin } from "@opencode/plugin/tui"

// TUI 插件必须有 ./tui 入口；这里只做占位，实际逻辑在 tui.tsx。
export default Plugin.define({
  id: "skyget.status",
  setup() {},
})

export const tui = TuiPlugin
