//! 复现「界面点重试」这条路：从台账取一条记录、标上 rerun、再跑一次。
//!
//! 用途：确认重试落回同一个文件、不重下、也不会把文件清空。
//! 跑法：`cargo run --release --example rerun_probe -- <id>`

use skyfetch::{paths, run_cancellable, Cancel, Engine, Event, Store};

fn main() -> anyhow::Result<()> {
    let id = std::env::args().nth(1).expect("给一个台账 id");
    let store = Store::open()?;
    let mut task = store.get(&id)?.expect("台账里没有这条");
    let dest = std::path::PathBuf::from(&task.dest);
    let before = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    println!("重试前：{} 存在={} 大小={}", dest.display(), dest.is_file(), before);

    task.state = skyfetch::model::STATE_PENDING.to_string();
    task.error.clear();
    task.pid = 0;
    task.rerun = true; // 界面 requeue() 就是这么设的

    let engine = Engine::new(5);
    let cb = |e: Event| match e {
        Event::Progress { done, total } => eprintln!("progress {done}/{total}"),
        Event::Finished { cached } => eprintln!("finished cached={cached}"),
        Event::Note(n) => eprintln!("note {n}"),
        _ => {}
    };
    let started = std::time::Instant::now();
    let r = run_cancellable(&store, &engine, &mut task, &cb, &Cancel::new());
    println!("结果：{r:?} 用时 {:?}", started.elapsed());

    let after = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    println!("重试后：存在={} 大小={}", dest.is_file(), after);
    if after != before {
        println!("❌ 大小变了！{} → {}", before, after);
    } else {
        println!("✓ 大小没变");
    }
    let blob = paths::blobs_dir().join(&task.sha256[..2]).join(&task.sha256);
    println!(
        "缓存块：存在={} 大小={}",
        blob.is_file(),
        std::fs::metadata(&blob).map(|m| m.len()).unwrap_or(0)
    );
    Ok(())
}
