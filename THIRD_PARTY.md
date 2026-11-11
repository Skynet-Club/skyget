# 第三方代码与许可证

本仓库自身是 **MIT**（见 [LICENSE](LICENSE)）。下面是随源码一起分发、需要单独说明的部分。

## 内置修改过的上游代码（vendored）

| 目录 | 上游 | 版本 | 许可证 | 我们改了什么 |
|---|---|---|---|---|
| `vendor/egui-winit/` | [`emilk/egui` · egui-winit](https://github.com/emilk/egui/tree/main/crates/egui-winit) | 0.36.2 | MIT OR Apache-2.0 | 26 行：丢掉 GNOME/Wayland + fcitx5 反复发来的**同一条**空输入法预编辑（否则界面 150–200fps 空转、dock 图标常闪）。见 `vendor/egui-winit/src/lib.rs` 的 `last_preedit` 与 `Cargo.toml` 里 `[patch.crates-io]` 的注释。 |

上游 crates.io 包里只带 `license` 字段、不带 LICENSE 文本；按 MIT/Apache-2.0 的要求，
分发时需保留版权与许可声明：**版权归 Emil Ernerfeldt 及 egui 贡献者**，
许可证全文见 <https://github.com/emilk/egui/blob/main/LICENSE-MIT> 与
<https://github.com/emilk/egui/blob/main/LICENSE-APACHE>。

上游修好之后（0.36/0.37 都还没有）应当删掉 `vendor/` 这一项，回到普通依赖。

## 普通依赖

其余依赖都是常规 crates.io 依赖，版本锁定在 `Cargo.lock`；它们的许可证跟随各自 crate
（多数是 MIT / Apache-2.0 / Unicode-3.0）。要出二进制发行包时，用：

```bash
cargo install cargo-about   # 可选
cargo about generate about.hbs > THIRD_PARTY.html
```

或直接 `cargo tree --format '{l} {license}'` 生成清单附在发行说明里。

## 没有引入的东西

- 没有内嵌 emoji、图标字体或位图素材：界面图标是代码画的（`src/gui/mod.rs` 的 `act_icon` /
  `folder_icon` / `close_x`），窗口图标是 `assets/icon.png`（本项目自己的图）。
- 没有从别的产品复制界面代码。`src/gui/theme.rs` 头部提到的 PhotoCraft 指**命名与尺寸的
  参考**（语义 token 名、面板高度这类约定），不是代码来源。
