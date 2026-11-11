//! 这就是「天网机库内置天网盒子」的形态：只依赖 `skyfetch::sdk`，不碰 reqwest、
//! 不碰分段细节、不碰 SQLite —— 一次调用，拿到落地路径、sha256 和信任级别。
//!
//! 跑一下：
//!
//!     cargo run --example embed --no-default-features --features http -- \
//!         https://registry.npmmirror.com/typescript/-/typescript-5.4.5.tgz /tmp/embed

use std::sync::atomic::{AtomicU64, Ordering};

use skyfetch::sdk::{run, Engine, Event, Store, Task};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .expect("用法：embed <url> [目标目录] [期望 sha256]");
    let dest = args.next().unwrap_or_else(|| "/tmp/embed".to_string());
    let sha = args.next().unwrap_or_default();

    println!("能力：{:?}", skyfetch::sdk::capabilities());

    let store = Store::open()?;
    let engine = Engine::new(4);
    let mut task = Task::new(&url, &dest).from_agent("hangar");
    if !sha.is_empty() {
        task = task.expecting(&sha);
    }

    // 进度回调：这里可以换成「往机库窗口里画一条进度条」
    let last_mb = AtomicU64::new(0);
    let cb = |e: Event| {
        if let Event::Progress { done, total } = e {
            let mb = done >> 20;
            if mb > last_mb.load(Ordering::Relaxed) {
                last_mb.store(mb, Ordering::Relaxed);
                eprintln!("  {mb} MB / {} MB", total >> 20);
            }
        }
    };

    run(&store, &engine, &mut task, &cb)?;

    // 给上层（机库界面 / agent）看的最终结论
    println!("落地  {}", task.dest);
    println!("sha256 {}", task.sha256);
    println!("校验  {}", task.trust);
    Ok(())
}
