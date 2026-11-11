# 这个目录是改过的上游代码，不是我们自己的

`vendor/egui-winit/` 是 [egui-winit 0.36.2](https://github.com/emilk/egui/tree/main/crates/egui-winit)
的副本，按 **MIT OR Apache-2.0** 分发（版权归 Emil Ernerfeldt 及 egui 贡献者）。
`Cargo.toml` 里通过 `[patch.crates-io]` 指向它。

## 改了什么

一处，26 行，全部在 `src/lib.rs`：丢掉**与上一条完全相同**的输入法预编辑。

GNOME 48 + Wayland + fcitx5 在输入框有焦点时会反复发同一条空预编辑
（上游文档里记着这个现象：*duplicate until `TextEdit` blurred*）。
它对输入没有任何信息量，却会变成 egui 的一条「立刻重绘」请求 ——
实测 150~200 fps 空转，gnome-shell 每 0.6 秒打一次窗口栈断言，dock 图标一直闪。

实现是记住上一条预编辑（`last_preedit`），只有**完全相同**时才丢；
第一次照常放行，所以「取消输入法」这类需要清空预编辑的操作不受影响。

## 什么时候删掉

上游修好之后（0.36 / 0.37 都还没有）。删的时候：

1. 删掉 `vendor/` 整个目录；
2. 删掉 `Cargo.toml` 里的 `[patch.crates-io]` 段；
3. `cargo update -p egui-winit` 回到 crates.io 版本。

## 许可证文本

上游 crates.io 的包里只带 `license` 字段、不带 LICENSE 文件。按 MIT/Apache-2.0 的要求，
分发时必须保留版权与许可声明，全文见：

- <https://github.com/emilk/egui/blob/main/LICENSE-MIT>
- <https://github.com/emilk/egui/blob/main/LICENSE-APACHE>

仓库根目录的 `THIRD_PARTY.md` 与打包用的 `packaging/copyright`（DEP-5）都记了这一条。
