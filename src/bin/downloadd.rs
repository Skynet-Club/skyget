//! `skynet-downloadd` · 天网盒子守护进程的**旧名兼容入口**。
//!
//! 新名是 `skynet-dataplaned`；两个二进制共用 `skyfetch::daemon::run()`，
//! 同一份 pid 文件、同一个端口、同一本台账。

fn main() -> anyhow::Result<()> {
    skyfetch::daemon::run()
}
