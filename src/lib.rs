//! 天网盒子 · 单机多 agent 的下载、上传、校验与调度。
//!
//! 人类看得见，agent 用得动：一个内核，多个面（CLI / HTTP 接口 / GUI / 天网机库），
//! 共享同一个任务台账与同一份内容寻址缓存。
//!
//! # 作为 SDK 用
//!
//! 对外**承诺稳定**的只有 [`sdk`] 这一个面里的几样东西。其余 `pub` 模块是实现细节，
//! 会随版本变化 —— 下游（比如天网机库）请只依赖 `skyfetch::sdk`。
//!
//! ```no_run
//! use skyfetch::sdk::{run, Engine, Event, Store, Task};
//!
//! let store = Store::open()?;
//! let engine = Engine::new(5);
//! let mut task = Task::new("https://example.org/x.iso", "~/.local/share/app");
//! let cb = |e: Event| { /* 画进度 */ };
//! run(&store, &engine, &mut task, &cb)?;
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod agents;
pub mod api;
#[cfg(feature = "bt")]
pub mod bt;
pub mod cas;
pub mod credential;
pub mod daemon;
pub mod engine;
pub mod hls;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod tray;
pub mod lock;
pub mod manifest;
pub mod metrics;
pub mod model;
pub mod paths;
pub mod prune;
pub mod quota;
pub mod scheduler;
pub mod security;
pub mod settings;
pub mod selftest;
pub mod stats;
pub mod store;
pub mod sys;
pub mod uds;
pub mod webhook;
pub mod upload;
pub mod verify;

pub use engine::{discard_parts, run, run_cancellable, Cancel, Engine, Event, Stopped};
pub use model::{Task, TaskView};
pub use store::Store;

/// SDK 面：机库这类下游只需要这几样。
///
/// 五个符号：`Task`（要下什么）、`Event`（过程）、`run`（干活）、
/// `Store`（台账）、`verify`（自检）。内部的 reqwest、分段、BT、SQLite 一条都不外露。
pub mod sdk {
    pub use crate::engine::{run, run_cancellable, Cancel, Engine, Event, Stopped};
    pub use crate::model::{Task, TaskView};
    pub use crate::store::Store;
    pub use crate::prune::{apply, parse_age, plan, Plan};
    pub use crate::verify::{run as verify, Report};

    /// 这个构建支持哪些能力（机库据此决定按钮怎么画）。
    pub fn capabilities() -> Vec<&'static str> {
        let mut v = vec!["http", "hls", "manifest"];
        if cfg!(feature = "bt") {
            v.push("bt");
        }
        if cfg!(feature = "gui") {
            v.push("gui");
        }
        v
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
