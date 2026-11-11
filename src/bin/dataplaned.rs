//! `skynet-dataplaned` · 天网盒子守护进程。
//!
//! 用法：
//!     skynet-dataplaned                # 127.0.0.1:7788
//!     skynet-dataplaned --port 7799
//!     systemctl --user start skynet-dataplaned
//!
//! 旧名 `skynet-downloadd` 继续可用，两个入口行为完全一致。

fn main() -> anyhow::Result<()> {
    skyfetch::daemon::run()
}
