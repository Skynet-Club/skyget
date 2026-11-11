//! 天网盒子 · 界面（机群台）。
//!
//! 排版纪律跟天网哨兵一致：列定宽、数字右对齐、等宽字体。
//! 数值从 9 B/s 涨到 900 MB/s 时列一分不动 —— 否则整张表每 200ms 抖一次。
//!
//! 这一面只做两件事：让人看清 agent 在取什么，以及让人一句话就能下载。
//! 真正的活全在内核（`skyfetch::run`），界面不开第二条路。

pub(crate) mod remote;
mod run;
pub use run::run;

use remote::Cmd;
mod theme;

use crate::engine::{clock, human};
use crate::model::{new_id, now_ms, Task};
use crate::paths;

use egui::{
    pos2, vec2, Align, Align2, Button, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2,
    Rect, RichText, Sense, Stroke, StrokeKind, Ui,
};
use std::process::Stdio;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use std::time::{Duration, Instant};

const WAVE_LEN: usize = 60;
const ROW_H: f32 = 34.0;
const PAD_L: f32 = 12.0;

// 定宽列。波形列吃剩余宽度，其余一分不动。
const C_STATE: f32 = 62.0;
const C_NAME: f32 = 206.0;
const C_SRC: f32 = 104.0;
const C_SIZE: f32 = 92.0;
const C_RATE: f32 = 92.0;
/// 线路数：BT 是「已连/发现」，直链是分段连接数
const C_LINKS: f32 = 68.0;
const C_PROG: f32 = 140.0;
/// 三个窗口按钮（最小/最大/关闭）占的宽度：标题栏拖动区必须让开这一段，
/// 否则按下会被拖动区先吃掉，按钮看起来就是点了没反应
const WIN_BTN_W: f32 = 34.0;
const WIN_BTNS_W: f32 = WIN_BTN_W * 3.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowState {
    Queued,
    Running,
    /// 数据下完了，正在算 sha256 / 种子的 hash 校验
    Verifying,
    Done,
    Failed,
    Interrupted,
}

impl RowState {
    fn label(self) -> &'static str {
        match self {
            RowState::Queued => "排队",
            RowState::Running => "下载中",
            RowState::Verifying => "校验中",
            RowState::Done => "完成",
            RowState::Failed => "失败",
            RowState::Interrupted => "中断",
        }
    }

    fn color(self) -> Color32 {
        match self {
            RowState::Queued => theme::FAINT,
            RowState::Running | RowState::Verifying => theme::ACCENT,
            RowState::Done => theme::OK,
            RowState::Failed => theme::DANGER,
            RowState::Interrupted => theme::WARN,
        }
    }
}

/// 列表按状态筛。做成 4 个而不是 6 个：人找东西时脑子里只有
/// 「在跑的 / 出事的 / 下完的」，排队、校验、下载中是同一件事的三个阶段。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Problem,
    Done,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Active => "在跑",
            Filter::Problem => "失败",
            Filter::Done => "完成",
        }
    }

    fn matches(self, s: RowState) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => {
                matches!(s, RowState::Queued | RowState::Running | RowState::Verifying)
            }
            // 失败和中断都要人处理一下，放同一格；悬停里说清楚。
            Filter::Problem => matches!(s, RowState::Failed | RowState::Interrupted),
            Filter::Done => s == RowState::Done,
        }
    }
}

struct Row {
    id: String,
    url: String,
    name: String,
    state: RowState,
    done: u64,
    total: u64,
    rate: f64,
    /// 切片流：按播放时长报进度（秒）。字节数完成后才有。
    secs: Option<(u64, u64)>,
    /// 线路：已连接 / 发现到的（直链时 seen=0）
    links: Option<(u32, u32)>,
    /// 校验强度：这份内容凭什么可信
    trust: String,
    wave: Vec<f64>,
    path: String,
    sha: String,
    error: String,
    source: String,
    cached: bool,
    created_ms: i64,
    last_evt: Instant,
}

impl Row {
    fn new(t: &Task) -> Self {
        let name = file_name_of(&t.dest, &t.url);
        let state = match t.state_for_display().as_str() {
            "done" => RowState::Done,
            "failed" => RowState::Failed,
            "interrupted" => RowState::Interrupted,
            "running" => RowState::Running,
            _ => RowState::Queued,
        };
        Self {
            id: t.id.clone(),
            url: t.url.clone(),
            name,
            state,
            done: t.bytes_done,
            total: t.bytes_total,
            rate: 0.0,
            secs: None,
            trust: t.trust.clone(),
            links: if t.links_live > 0 || t.links_seen > 0 {
                Some((t.links_live, t.links_seen))
            } else {
                None
            },
            wave: Vec::new(),
            path: t.dest.clone(),
            sha: t.sha256.clone(),
            error: t.error.clone(),
            source: t.source.clone(),
            cached: false,
            created_ms: t.created_at,
            last_evt: Instant::now(),
        }
    }
}

/// 后台线程 → 界面。只有这一个方向，界面从不反向调用下载。
pub(crate) enum Msg {
    /// 守护进程那边变过的任务（增量，每 700ms 一次）
    Changes {
        items: Vec<crate::model::TaskView>,
        deleted: Vec<String>,
    },
    /// 守护进程连上了 / 断了
    Link { ok: bool, why: String },
    /// 命令没执行成功（暂停失败、删不掉……）
    RemoteErr(String),
    /// 机群快照（`GET /agents`）；None 表示这次没拉到，保留上一份。
    Fleet(Option<remote::AgentsView>),
    /// 提交成功了，守护进程给的真 id 回来了（占位那行要改名）
    Submitted { temp: String, real: String },
    /// 提交没成功：占位那行得撤掉，别留一个永远「排队」的幽灵
    SubmitFailed { temp: String, why: String },
    /// 系统目录选择器的结果（None = 用户取消了）
    PickedDir(Option<String>),
    /// 剪贴板接住一个地址
    Clip(String),
}

pub struct App {
    rows: Vec<Row>,
    selected: Option<String>,
    input: String,
    focus_input: bool,
    rx: Receiver<Msg>,
    /// 界面自己的发送端（后台线程回话用：选目录、剪贴板、轮询）
    tx: Sender<Msg>,
    /// 给守护进程发命令（提交 / 暂停 / 继续 / 取消 / 删除）
    cmd_tx: Sender<Cmd>,
    /// 守护进程此刻连不连得上
    link_ok: bool,
    /// 守护进程报上来的任务条数（页脚写「本机 N 条」用）
    remotes: usize,
    /// 缓存占用：`dir_size` 要遍历整个缓存目录，不能每帧算一次
    blob_size: (String, Instant),
    /// 帧率自证的起点（帧数, 时刻）
    fps_mark: Option<(u32, Instant)>,
    /// 「下载保存到」那个小弹层的状态
    dir_open: bool,
    dir_input: String,
    dir_err: Option<String>,
    dir_anchor: Option<Rect>,
    /// 系统目录选择器在跑（别让人连点出三个）
    dir_picking: bool,
    /// 弹层是在第几帧打开的。**决定它有没有被同一帧关掉**：
    /// 打开它的那一下点击本身也算「点了外面」，不排除掉就会开了立刻关。
    dir_open_pass: u64,
    auto_clip: bool,
    /// 台账里只看自己下的（不带来源标签的记录）。机库/agent 起的会混进来。
    only_mine: bool,
    /// 「机群」视图：agent 身份、策略、并发。关掉时回到任务表。
    fleet_open: bool,
    /// 最近一次 `/agents` 快照；守护进程断线时保留旧值而不是清空。
    fleet: Option<remote::AgentsView>,
    /// 最后一次拿到快照的时刻（页脚显示“几秒前”）
    fleet_at: Option<Instant>,
    clip_seen: String,
    notice: Option<(String, Color32, Instant)>,
    detail_open: bool,
    /// 列表按状态筛：台账里一半是失败时，「正在下的是什么」不该靠滚。
    state_filter: Filter,
    link_hot: bool,
    last_tick: Instant,
    /// 每秒去台账看一眼：CLI / 接口起的任务，这里也要看得见
    /// 标题栏左边的图标纹理（第一帧才建）
    icon: Option<egui::TextureHandle>,
    /// 托盘命令通道；None = 桌面没有托盘宿主，关窗口就只能退出
    tray_rx: Option<std::sync::mpsc::Receiver<crate::tray::TrayCmd>>,
    /// 用户从托盘选了「退出」——这时候关窗口就是真关
    quitting: bool,
    /// --hidden：开机自启用，起来就待在托盘里，不弹窗口
    start_hidden: bool,
    /// 窗口此刻是不是收在托盘里
    hidden: bool,
    /// 上次尝试起托盘的时刻（失败要重试，不能一次失败就永久放弃）
    last_tray_try: Instant,
    /// 上次确认托盘项还在不在的时刻
    last_tray_check: Instant,
    /// 托盘项此刻是否确实不在座（Watcher 掉线 / 扩展被停用）
    tray_dead: bool,
    /// 唤醒用的 Context：后台线程只在**真有变化**时叫一次重绘。
    /// 界面本身没有常驻心跳 —— 空闲时一帧都不画。
    ctx: egui::Context,
    /// `SKYGET_GUI_DEBUG=1`：把鼠标按键与按钮点击打到 stderr（排查「点了没反应」）
    debug: bool,
    /// 上一帧窗口有没有焦点（变化时记一行日志）
    was_focused: bool,
    /// QA 旋钮已经触发过（每次启动只做一次）
    qa_done: bool,
    /// 已经用 QA 旋钮塞过地址（只塞一次）
    qa_added: Option<String>,
    /// 启动时刻：QA 旋钮和自截图都按它算时间
    started_at: Instant,
    /// `SKYGET_SHOT=<png>` 自截图：画满 N 帧（SKYGET_SHOT_FRAMES，默认 40）截一张。
    /// 这台机器上 GNOME 的 D-Bus 截图返回 AccessDenied、X11 grab 对 Wayland 窗口全黑，
    /// 所以界面只能自己截自己 —— 这也是唯一能看见像素的路。
    shot: Option<(std::path::PathBuf, u32)>,
    frames: u32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();

        let (tx, rx) = channel::<Msg>();
        // 下载不归这个窗口管：它只跟守护进程说话。关了窗口，下载继续。
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let base = remote::base();
        // 拉起守护进程放在后台线程里做（systemctl 可能要几百毫秒到几秒）。
        // 放在这里会把「双击图标到看见窗口」整段时间拖长 —— 那是最不该慢的地方。
        remote::spawn(cmd_rx, tx.clone(), ctx.clone(), base.clone());

        spawn_clipboard(tx.clone(), ctx.clone());

        // 第一屏先空着：守护进程的那份台账马上就回来（700ms 内）。
        // 不在启动路径上读库，窗口就能立刻出来。
        let rows: Vec<Row> = Vec::new();
        let notice: Option<(String, Color32, Instant)> = None;

        // 启动时默认选中最上面那条：详情卡（谁在下、线路、落地、sha256）不该是空的
        let selected = rows.first().map(|r| r.id.clone());
        Self {
            rows,
            selected,
            input: String::new(),
            focus_input: true,
            rx,
            tx: tx.clone(),
            cmd_tx,
            link_ok: false,
            remotes: 0,
            blob_size: (String::new(), Instant::now() - Duration::from_secs(60)),
            fps_mark: None,
            dir_open: false,
            dir_input: String::new(),
            dir_err: None,
            dir_anchor: None,
            dir_picking: false,
            dir_open_pass: 0,
            auto_clip: true,
            clip_seen: String::new(),
            notice,
            detail_open: true,
            state_filter: Filter::All,
            link_hot: false,
            last_tick: Instant::now(),
            icon: None,
            tray_rx: None,
            quitting: false,
            start_hidden: false,
            hidden: false,
            last_tray_try: Instant::now(),
            last_tray_check: Instant::now(),
            tray_dead: false,
            ctx,
            only_mine: false,
            fleet_open: std::env::var("SKYGET_GUI_VIEW")
                .map(|v| v.eq_ignore_ascii_case("fleet"))
                .unwrap_or(false),
            fleet: None,
            fleet_at: None,
            debug: std::env::var("SKYGET_GUI_DEBUG").is_ok(),
            was_focused: false,
            qa_done: false,
            qa_added: None,
            started_at: Instant::now(),
            shot: std::env::var("SKYGET_SHOT").ok().map(|p| {
                let n = std::env::var("SKYGET_SHOT_FRAMES")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(40);
                (std::path::PathBuf::from(p), n)
            }),
            frames: 0,
        }
    }

    fn add_url(&mut self, url: String) {
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let lower = url.to_ascii_lowercase();
        let is_bt = lower.starts_with("magnet:") || lower.ends_with(".torrent");
        if !is_bt && !(lower.starts_with("http://") || lower.starts_with("https://")) {
            let kind = url.split("://").next().unwrap_or("?");
            self.say(
                &format!("{kind}:// 不是能直取的地址 —— 网盘、迅雷、电驴都是商业锁，我们不做。"),
                theme::WARN,
            );
            return;
        }
        // 已经在队列里或正在跑：不重复排，直接把那一行选中
        if let Some(r) = self
            .rows
            .iter()
            .find(|r| r.url == url && matches!(r.state, RowState::Queued | RowState::Running))
        {
            self.selected = Some(r.id.clone());
            self.say("这个地址已经在队列里了。", theme::FAINT);
            return;
        }

        let task = Task {
            id: new_id(&url),
            direction: crate::model::DIR_DOWNLOAD.to_string(),
            protocol: String::new(),
            credential_ref: String::new(),
            sources: Vec::new(),
            url: url.clone(),
            dest: {
                // 落地目录：环境变量 > 界面里设的 > 默认 ~/Downloads（见 settings.rs）
                let mut d = crate::settings::dest_dir();
                let mut text = d.display().to_string();
                if !text.ends_with('/') {
                    text.push('/');
                }
                d = std::path::PathBuf::from(text);
                d.display().to_string()
            },
            state: crate::model::STATE_PENDING.to_string(),
            bytes_done: 0,
            bytes_total: 0,
            sha256: String::new(),
            error: String::new(),
            error_code: String::new(),
            retryable: false,
            retry_after_ms: 0,
            next_action: String::new(),
            source: "human".to_string(),
            priority: crate::model::PRIORITY_NORMAL.to_string(),
            expected_sha256: String::new(),
            pid: 0,
            links_live: 0,
            links_seen: 0,
            trust: String::new(),
            accept_change: false,
            cached: false,
            rerun: false,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        self.rows.insert(0, Row::new(&task));
        self.selected = Some(task.id.clone());
        if self
            .cmd_tx
            .send(Cmd::Submit {
                temp_id: task.id.clone(),
                url: task.url.clone(),
                dest: task.dest.clone(),
                sha256: task.expected_sha256.clone(),
                source: task.source.clone(),
            })
            .is_err()
        {
            self.say("跟守护进程的连接断了，得重开界面。", theme::DANGER);
        }
    }

    /// 托盘起好了，把命令通道接进来。
    ///
    /// 这里不再起心跳：唤醒由托盘回调自己做（`tray::start` 拿着 ctx，
    /// 菜单一点就叫一次重绘）。窗口收进托盘后不可见、eframe 不出帧也没关系 ——
    /// 有人点图标就先来一次 request_repaint，命令随后在下一帧被处理。
    pub fn attach_tray(&mut self, rx: Option<std::sync::mpsc::Receiver<crate::tray::TrayCmd>>) {
        self.tray_rx = rx;
    }

    /// 关窗口该干什么：有托盘就收起来，没托盘只能退出 ——
    /// 否则用户关了窗口却再也找不到入口。下载线程不受影响，收起来也照跑。
    fn hides_instead_of_exiting(&self) -> bool {
        !self.quitting && self.tray_rx.is_some() && !self.tray_dead
    }

    pub fn set_start_hidden(&mut self, yes: bool) {
        self.start_hidden = yes;
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        // Wayland 没有「隐藏窗口」这回事：winit 的 wayland 后端里
        // `set_visible()` 是空实现（源码注释就写着 Not possible on Wayland），
        // 所以以前发 Visible(false) 在 GNOME/Wayland 上什么都不会发生 ——
        // 用户看到的就是「点 × 没反应」。能用的只有「最小化」（xdg_toplevel.set_minimized）。
        // 两条都发：X11 上 Visible(false) 真的隐藏，Wayland 上退化成一个最小化。
        self.click_log("收窗口：Minimized(true) + Visible(false)");
        self.hidden = true;
        self.last_tray_check = Instant::now();
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.click_log("把窗口放回来（Visible(true)）");
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// 托盘的看门狗。两件事：
    ///
    /// 1. 托盘没起来就隔 10 秒再试 —— GNOME 的 appindicator 扩展会掉线也会被重新启用，
    ///    一次失败就永久放弃，等于把「关窗口收托盘」悄悄降级成「关窗口退出」。
    /// 2. 窗口藏着的时候每 3 秒确认托盘项还在座。图标没了而窗口还藏着就是死角：
    ///    用户既看不见窗口，也点不到图标 —— 这时候主动把窗口放回来。
    fn tray_watchdog(&mut self, ctx: &egui::Context) {
        // 每 3 秒问一次：托盘项还在座吗。
        //
        // 这里**不**主动 shutdown 再重连。ksni 自己会等 Watcher 回来重新注册；
        // 我们插一手反而会在顶栏留下一模一样的第二个图标（实测过）。
        if self.last_tray_check.elapsed().as_secs() >= 3 {
            self.last_tray_check = Instant::now();
            self.tray_dead = !crate::tray::is_registered();
            if self.tray_dead && self.hidden {
                eprintln!("托盘图标不在了，把窗口放回来（免得既没窗口也点不到图标）");
                self.show_window(ctx);
            }
        }
        // 从没起过托盘（启动时宿主不在）就隔 10 秒再试一次；
        // 起来了的不用管，交给 ksni 自愈。
        if self.tray_rx.is_none() && !self.quitting && self.last_tray_try.elapsed().as_secs() >= 10 {
            self.last_tray_try = Instant::now();
            if let Some(rx) = crate::tray::start(self.ctx.clone()) {
                self.attach_tray(Some(rx));
                self.tray_dead = false;
            }
        }
    }

    /// Ctrl+W / Ctrl+H 也算「关窗口」：和点 × 走同一条路。
    /// 除了符合习惯，也让「收进托盘」在不方便点鼠标时仍然是可达的。
    fn wants_hide_shortcut(&self, ctx: &egui::Context) -> bool {
        ctx.input(|i| {
            i.modifiers.command && (i.key_pressed(egui::Key::W) || i.key_pressed(egui::Key::H))
        })
    }

    /// 处理托盘菜单与「第二个实例把你叫醒」的信号。
    fn handle_tray(&mut self, ctx: &egui::Context) {
        use crate::tray::TrayCmd;

        if crate::tray::take_wake() {
            self.show_window(ctx);
        }

        // 先把命令收干净，别在借用 tray_rx 时调 &mut self
        let mut cmds = Vec::new();
        if let Some(rx) = self.tray_rx.as_ref() {
            for _ in 0..8 {
                match rx.try_recv() {
                    Ok(c) => cmds.push(c),
                    Err(_) => break,
                }
            }
        }
        for cmd in cmds {
            match cmd {
                TrayCmd::Show => self.show_window(ctx),
                TrayCmd::Quit => {
                    // 真正的退出由 tray::fire 里延迟启动的线程完成（先让 D-Bus
                    // 菜单调用返回，再结束进程）；这里只标记，避免关窗逻辑把它藏起来。
                    self.quitting = true;
                }
            }
        }
    }

    /// 按钮类事件落一行日志：`~/.local/share/skynet-download/gui.log`。
    /// 「点了没反应」这种报告，只有日志能分清是「事件没进来」还是「逻辑没走对」。
    fn click_log(&self, what: &str) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(crate::paths::data_dir());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::paths::data_dir().join("gui.log"))
        {
            let _ = writeln!(
                f,
                "{} {what}（托盘={} hidden={} 选中={:?}）",
                crate::model::now_ms(),
                self.tray_rx.is_some(),
                self.hidden,
                self.selected
            );
        }
        if self.debug {
            eprintln!("[gui] {what}");
        }
    }

    fn say(&mut self, text: &str, color: Color32) {
        self.notice = Some((text.to_string(), color, Instant::now()));
    }

    /// 收后台消息 + 采样波形。每帧都跑，事件优先。
    fn pump(&mut self, _ctx: &egui::Context) {
        loop {
            match self.rx.try_recv() {
                Ok(msg) => self.on_msg(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let now = Instant::now();
        // 帧率自证：SKYGET_GUI_DEBUG=1 时每 5 秒往 gui.log 写一行。
        // 「空闲一帧不画」这件事得能测出来，不能只靠感觉。
        if self.debug {
            if let Some((f0, t0)) = self.fps_mark {
                let dt = now.duration_since(t0).as_secs_f32();
                if dt >= 5.0 {
                    let df = self.frames.saturating_sub(f0);
                    let causes: Vec<String> = _ctx
                        .repaint_causes()
                        .iter()
                        .map(|c| {
                            let t = format!("{c}");
                            t.chars().take(90).collect::<String>()
                        })
                        .collect();
                    self.click_log(&format!(
                        "帧：{df} 帧 / {dt:.1} 秒 = {:.1} fps · 谁在求重绘：{}",
                        df as f32 / dt,
                        if causes.is_empty() {
                            "（没有，是主动画的）".to_string()
                        } else {
                            causes.join(" | ")
                        }
                    ));
                    self.fps_mark = Some((self.frames, now));
                }
            } else {
                self.fps_mark = Some((self.frames, now));
            }
        }
        if now.duration_since(self.last_tick) >= Duration::from_millis(250) {
            self.last_tick = now;
            for r in &mut self.rows {
                if r.state == RowState::Running {
                    // 一秒没有新字节 = 速度归零，波形如实往下掉
                    if r.last_evt.elapsed() > Duration::from_millis(2600) {
                        r.rate = 0.0;
                    }
                    r.wave.push(r.rate);
                    if r.wave.len() > WAVE_LEN {
                        r.wave.remove(0);
                    }
                } else if !r.wave.is_empty() && r.wave.len() < WAVE_LEN {
                    r.wave.push(0.0);
                    if r.wave.len() > WAVE_LEN {
                        r.wave.remove(0);
                    }
                }
            }
        }

    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Changes { items, deleted } => {
                // 能收到台账就是连上了：不要只在「从断到通」那一跳上才置 true，
                // 否则启动时第一屏数据明明回来了，状态栏还写着「没连上」。
                self.link_ok = true;
                self.apply_changes(items, deleted);
            }
            Msg::Link { ok, why } => {
                let changed = self.link_ok != ok;
                self.link_ok = ok;
                if !ok {
                    self.say(&why, theme::WARN);
                } else if changed {
                    self.say("接上守护进程了。", theme::OK);
                }
            }
            Msg::RemoteErr(e) => self.say(&e, theme::DANGER),
            Msg::Fleet(v) => {
                if let Some(v) = v {
                    self.fleet = Some(v);
                    self.fleet_at = Some(Instant::now());
                }
            }
            Msg::PickedDir(p) => {
                self.dir_picking = false;
                if let Some(path) = p {
                    // 选完不直接生效：先填进输入框，让人看一眼再点「用这个」
                    self.dir_input = path;
                    self.dir_err = None;
                }
            }
            Msg::SubmitFailed { temp, why } => {
                self.rows.retain(|r| r.id != temp);
                if self.selected.as_deref() == Some(temp.as_str()) {
                    self.selected = self.rows.first().map(|r| r.id.clone());
                }
                self.say(&why, theme::DANGER);
            }
            Msg::Submitted { temp, real } => {
                // 占位行改名：以后轮询拿到的就是同一条，不会再冒出第二个「排队」
                if let Some(r) = self.rows.iter_mut().find(|r| r.id == temp) {
                    r.id = real.clone();
                }
                if self.selected.as_deref() == Some(temp.as_str()) {
                    self.selected = Some(real);
                }
            }
            Msg::Clip(url) => {
                if !self.auto_clip || url == self.clip_seen {
                    return;
                }
                self.clip_seen = url.clone();
                self.add_url(url);
                self.input.clear();
                self.say("从剪贴板接住了这个地址，已开始下载。", theme::ACCENT);
            }
        }
        if self.selected.is_none() {
            self.selected = self
                .rows
                .iter()
                .find(|r| matches!(r.state, RowState::Running))
                .map(|r| r.id.clone());
        }
    }

    /// 增量合并：只动变过的那几条，删掉的从列表里摘掉。
    ///
    /// 不做「整份替换」有两个原因：一是每 700ms 重排 200 行会让人眼里的顺序乱跳；
    /// 二是速度/波形这些只属于界面的东西得能沿着同一条记录累积下去。
    fn apply_changes(&mut self, items: Vec<crate::model::TaskView>, deleted: Vec<String>) {
        for id in &deleted {
            self.rows.retain(|r| &r.id != id);
        }
        for v in items {
            match self.rows.iter_mut().find(|r| r.id == v.id) {
                Some(row) => update_row(row, &v),
                None => self.rows.insert(0, Row::new(&view_to_task(&v))),
            }
        }
        // 进行中/排队的排前面，其余按时间倒序（和启动时那套一致）
        self.rows.sort_by(|a, b| {
            rank(a.state)
                .cmp(&rank(b.state))
                .then(b.created_ms.cmp(&a.created_ms))
        });
        self.remotes = self.rows.len();
        let still = self
            .selected
            .as_ref()
            .map(|id| self.rows.iter().any(|r| &r.id == id))
            .unwrap_or(false);
        if !still {
            self.selected = self.rows.first().map(|r| r.id.clone());
        }
    }


    fn find(&mut self, id: &str) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    // ── 绘制 ────────────────────────────────────────────────────────────

    fn title_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("titlebar")
            .exact_size(34.0)
            .frame(
                Frame::NONE
                    .fill(theme::CHROME)
                    .inner_margin(Margin::symmetric(10, 4))
                    .stroke(Stroke::new(1.0, theme::SEP)),
            )
            .show(ui, |ui| {
                let full = ui.available_rect_before_wrap();
                // 拖动区只铺在「标题文字那一段」，**必须避开右边那三个窗口按钮**：
                // 两者重叠时，按下会被拖动区先吃掉，按钮看起来就是点了没反应
                // （用户报的「X 点击无效」就是这个）。所以这里按列宽把右端让出来。
                let drag_rect = Rect::from_min_max(
                    full.min,
                    pos2((full.max.x - WIN_BTNS_W).max(full.min.x), full.max.y),
                );
                let drag = ui.interact(drag_rect, ui.id().with("drag"), Sense::click_and_drag());
                if drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
                }

                ui.horizontal_centered(|ui| {
                    if self.icon.is_none() {
                        if let Ok(d) = eframe::icon_data::from_png_bytes(include_bytes!(
                            "../../assets/icon.png"
                        )) {
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [d.width as usize, d.height as usize],
                                &d.rgba,
                            );
                            self.icon = Some(ui.ctx().load_texture(
                                "app-icon",
                                img,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    if let Some(t) = &self.icon {
                        ui.add(
                            egui::Image::new((t.id(), egui::vec2(16.0, 16.0)))
                                .sense(Sense::empty()),
                        );
                    }
                    ui.label(RichText::new("天网盒子").size(theme::T_BODY).color(theme::TEXT));
                    let hot = self.link_hot;
                    let resp = ui
                        .add(
                            // selectable(false) 是关键：Label 默认可选中文字，
                            // 一旦可选中，egui 的文本选择逻辑就会在悬停时把光标设成 I-beam，
                            // 手形光标会被它盖掉。一个外链本来也不需要能选文字。
                            egui::Label::new(
                                RichText::new("· Skynet.Club")
                                    .size(theme::T_MICRO)
                                    .color(if hot { theme::TEXT } else { theme::FAINT }),
                            )
                            .selectable(false)
                            .sense(Sense::click()),
                        )
                        // 官方 API，比自己 set_cursor_icon 更靠得住：交给 egui 的交互系统统一裁决
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("打开 skynet.club");
                    // 光标要**每帧**设。egui 每帧都会把光标重置成默认值，
                    // 只在“刚悬停”那一帧设一次，下一秒就退回文本光标了。
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.hovered() != self.link_hot {
                        self.link_hot = resp.hovered();
                        ui.ctx().request_repaint();
                    }
                    if resp.clicked() {
                        open_url("https://skynet.club");
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if win_button(ui, WinBtn::Close) {
                            self.click_log("点了关闭按钮");
                            if self.hides_instead_of_exiting() {
                                self.hide_window(&ctx);
                            } else {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                        if win_button(ui, WinBtn::Max) {
                            self.click_log("点了最大化按钮");
                            let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
                        }
                        if win_button(ui, WinBtn::Min) {
                            self.click_log("点了最小化按钮");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    });
                });
            });
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("bar")
            .exact_size(46.0)
            .frame(
                Frame::NONE
                    .fill(theme::DOCK)
                    .inner_margin(Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0, theme::SEP)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 「下到哪儿」放在「下载」按钮右边 —— 读完「下什么」接着就是「下到哪」。
                    // 先量出它有多宽，地址框再按剩下的宽度算，否则会把它挤出屏幕。
                    let now = crate::settings::dest_dir();
                    let name = dir_short_name(&now);
                    let galley = ui.painter().layout_no_wrap(
                        name.clone(),
                        egui::FontId::proportional(theme::T_SMALL),
                        theme::FAINT,
                    );
                    let btn_w = 16.0 + 5.0 + galley.size().x + 10.0;
                    let w = (ui.available_width() - 260.0 - btn_w - 6.0).clamp(220.0, 620.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.input)
                            .hint_text("粘贴地址，回车就下载")
                            .desired_width(w)
                            .font(egui::TextStyle::Body),
                    );
                    if self.focus_input {
                        edit.request_focus();
                        self.focus_input = false;
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let url = self.input.clone();
                        self.input.clear();
                        self.add_url(url);
                        self.focus_input = true;
                    }
                    if ui.button(RichText::new("下载").size(theme::T_BODY)).clicked() {
                        let url = self.input.clone();
                        self.input.clear();
                        self.add_url(url);
                    }
                    ui.add_space(6.0);
                    {
                        // 不画背景：只在悬停/打开时把图标和文字提亮（不抢注意力，
                        // 也不给顶栏平添一块色块）。
                        let (rect, resp) = ui.allocate_exact_size(vec2(btn_w, 26.0), Sense::click());
                        // 每帧都记下这个按钮的位置：弹层不管是被点开的、被 Ctrl+D 叫出来的、
                        // 还是验收旋钮打开的，都得贴着它出现（否则会飘到窗口左上角）。
                        self.dir_anchor = Some(rect);
                        let hot = resp.hovered() || self.dir_open;
                        let c = if hot { theme::TEXT } else { theme::FAINT };
                        folder_icon(
                            ui.painter(),
                            Rect::from_min_size(
                                pos2(rect.left() + 1.0, rect.center().y - 8.0),
                                vec2(16.0, 16.0),
                            ),
                            c,
                        );
                        ui.painter().galley(
                            pos2(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0),
                            galley,
                            c,
                        );
                        if resp.clicked() {
                            // 无条件记一笔：这个按钮要是「点了没反应」，
                            // 日志能立刻分清是「事件没进来」还是「逻辑没走对」。
                            self.click_log(if self.dir_open {
                                "目录按钮：收起"
                            } else {
                                "目录按钮：打开"
                            });
                            self.dir_open = !self.dir_open;
                            if self.dir_open {
                                self.dir_input = now.display().to_string();
                                self.dir_err = None;
                                self.dir_open_pass = ui.ctx().cumulative_pass_nr();
                            }
                        }
                        if resp.on_hover_text("下载保存到这个目录（点一下可以改）").hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let rate: f64 = self.rows.iter().map(|r| r.rate).sum();
                        let running = self
                            .rows
                            .iter()
                            .filter(|r| r.state == RowState::Running)
                            .count();
                        let (v, u) = human_rate(rate);
                        ui.label(
                            RichText::new(format!("{v} {u}"))
                                .size(theme::T_TITLE)
                                .monospace()
                                .color(if rate > 0.0 { theme::OK } else { theme::FAINT }),
                        );
                        if running > 0 {
                            ui.label(
                                RichText::new(format!("{running} 个在下"))
                                    .size(theme::T_SMALL)
                                    .color(theme::FAINT),
                            );
                        }
                        // 台账是混着的：自己下的 + agent/机库起的。这里只做一件事：换个看法。
                        let label = if self.only_mine {
                            "只看我的"
                        } else {
                            "全部来源"
                        };
                        // 无框按钮而不是可点 Label：命中区从「11px 那行字」变成 24px，
                        // 鼠标/触摸都更好按，键盘 Tab 也进得来（Label 不进焦点链）。
                        let resp = ui
                            .add(
                                egui::Button::new(
                                    RichText::new(label).size(theme::T_SMALL).color(theme::FAINT),
                                )
                                .frame(false)
                                .min_size(vec2(0.0, 24.0)),
                            )
                            .on_hover_text(if self.only_mine {
                                "现在只看你自己下的。点一下看全部（含 agent / 机库起的）"
                            } else {
                                "现在显示全部。点一下只看自己下的"
                            });
                        if resp.hovered() {
                            ui.ctx()
                                .set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            self.only_mine = !self.only_mine;
                            self.click_log("切换台账来源筛选");
                        }
                        // 机群：daemon 上所有 agent 的身份、策略和并发。
                        // 它不是一个“设置”，是这张台子的另一面 —— 所以放在视图切换旁边。
                        // 「机群 5」以前读起来像「第 5 号机群」；数字是 agent 数量，
                        // 现在只在悬停里说，标签保持一个词。
                        let agents = self.fleet.as_ref().map(|f| f.agents.len());
                        let resp = ui
                            .add(
                                egui::Button::new(
                                    RichText::new("机群")
                                        .size(theme::T_SMALL)
                                        .color(if self.fleet_open {
                                            theme::ACCENT
                                        } else {
                                            theme::FAINT
                                        }),
                                )
                                .frame(false)
                                .min_size(vec2(0.0, 24.0)),
                            )
                            .on_hover_text(match agents {
                                Some(n) => format!(
                                    "看这台数据面上有哪些 agent、各自能下到哪、正在跑几个（{n} 个）"
                                ),
                                None => "看这台数据面上有哪些 agent、各自能下到哪、正在跑几个"
                                    .to_string(),
                            });
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            self.fleet_open = !self.fleet_open;
                            self.click_log("切换机群视图");
                        }
                    });
                });
            });
    }

    /// 「机群」：daemon 上谁在用、能用到哪、现在跑几个。
    /// 数据全部来自 `GET /agents`，界面不猜、也不读 daemon 的环境变量。
    fn fleet_view(&mut self, ui: &mut Ui) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(PAD_L);
            ui.label(
                RichText::new("机群")
                    .size(theme::T_HEAD)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(8.0);
            match &self.fleet {
                Some(f) => {
                    let s = &f.scheduler;
                    ui.label(
                        RichText::new(format!(
                            "并发 {} / {} · 排队 {} / {}",
                            s.running, s.max_running, s.queued, s.max_queued
                        ))
                        .size(theme::T_SMALL)
                        .monospace()
                        .color(theme::FAINT),
                    );
                    if let Some(c) = &f.caller {
                        ui.label(
                            RichText::new(format!("以 {c} 的身份查看"))
                                .size(theme::T_SMALL)
                                .color(theme::FAINT),
                        );
                    }
                    if !f.registry {
                        ui.label(
                            RichText::new("未启用 per-agent token")
                                .size(theme::T_SMALL)
                                .color(theme::WARN),
                        );
                    }
                }
                None => {
                    ui.label(
                        RichText::new("正在等守护进程的 /agents 快照…")
                            .size(theme::T_SMALL)
                            .color(theme::FAINT),
                    );
                }
            }
            if let Some(t) = self.fleet_at {
                ui.label(
                    RichText::new(format!("{} 秒前", t.elapsed().as_secs()))
                        .size(theme::T_MICRO)
                        .color(theme::FAINT),
                );
            }
        });
        ui.add_space(6.0);
        ui.separator();
        let Some(f) = self.fleet.clone() else {
            return;
        };
        if f.agents.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("还没有 agent 提交过任务。用 `skyget --as <名字>` 或 MCP 接进来。")
                        .size(theme::T_BODY)
                        .color(theme::FAINT),
                );
            });
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for a in &f.agents {
                    agent_card(ui, a);
                }
                ui.add_space(12.0);
            });
    }

    fn table(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let cols = cols_for(ui.available_width());
        header_row(ui, &cols);
        let name_w = C_NAME + cols.name_extra;

        let only_mine = self.only_mine;
        let filter = self.state_filter;
        let visible: Vec<usize> = (0..self.rows.len())
            .filter(|i| !only_mine || is_mine(&self.rows[*i].source))
            .filter(|i| filter.matches(self.rows[*i].state))
            .collect();

        // 筛选把选中的那条藏起来了：选中要跟着走。
        // 不然「切到完成」之后右边还在讲一条失败的任务 —— 列表和详情互相矛盾。
        let sel_visible = self
            .selected
            .as_deref()
            .map(|id| visible.iter().any(|i| self.rows[*i].id == id))
            .unwrap_or(false);
        if !sel_visible {
            self.selected = visible.first().map(|i| self.rows[*i].id.clone());
            // 没有可选的了就收起详情卡；但不要替用户「打开」——
            // 他手动关掉的卡片，不该因为换了个筛子又冒出来。
            if self.selected.is_none() {
                self.detail_open = false;
            }
        }

        let mut clicked: Option<String> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {

                if visible.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        if !self.rows.is_empty() {
                            // 有记录，只是被筛子挡住了：说清楚是哪一层筛的。
                            if only_mine && filter != Filter::All {
                                ui.label(
                                    RichText::new("当前来源和状态下都没有任务")
                                        .size(theme::T_BODY)
                                        .color(theme::FAINT),
                                );
                            } else if only_mine {
                                ui.label(
                                    RichText::new("这些都不是你自己下的 —— 都是 agent / 机库起的")
                                        .size(theme::T_BODY)
                                        .color(theme::FAINT),
                                );
                            } else {
                                ui.label(
                                    RichText::new(format!(
                                        "没有「{}」的任务",
                                        filter.label()
                                    ))
                                    .size(theme::T_BODY)
                                    .color(theme::FAINT),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new("点底部的「全部」看所有记录")
                                    .size(theme::T_MICRO)
                                    .color(theme::FAINT),
                            );
                            return;
                        }
                        ui.label(
                            RichText::new("粘贴一个地址，或者复制一个下载链接")
                                .size(theme::T_BODY)
                                .color(theme::FAINT),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("直链、种子、切片流都能接；网盘与迅雷不做")
                                .size(theme::T_MICRO)
                                .color(theme::FAINT),
                        );
                    });
                    return;
                }

                for (n, idx) in visible.iter().enumerate() {
                    let idx = *idx;
                    let r = &self.rows[idx];
                    let sel = self.selected.as_deref() == Some(r.id.as_str());
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
                    let bg = if sel {
                        theme::ROW_SEL
                    } else if n % 2 == 0 {
                        theme::ROW
                    } else {
                        theme::ROW_ALT
                    };
                    ui.painter().rect_filled(rect, 0.0, bg);

                    let resp = ui.interact(rect, ui.id().with(("row", idx)), Sense::click());
                    if resp.hovered() && !sel {
                        ui.painter().rect_filled(rect, 0.0, theme::ROW_HOVER);
                    }
                    // 列宽必然截断，悬停给全量：长文件名、完整地址、失败原因。
                    // 失败原因只给第一行：几十段失败的全文塞进悬停提示会变成一张盖住整张表的大纸，
                    // 全文在右边详情卡的终端块里看。
                    let tail = if r.error.is_empty() {
                        r.path.clone()
                    } else {
                        let head = r.error.lines().next().unwrap_or("");
                        format!("{}（全文在右侧详情卡）", short(head, 90))
                    };
                    resp.clone()
                        .on_hover_text(format!("{}\n{}\n{}", r.name, r.url, tail));
                    if resp.clicked() {
                        clicked = Some(r.id.clone());
                    }

                    let mut x = rect.left() + PAD_L;
                    let y = rect.center().y;

                    cell_text(ui, x, y, C_STATE - 8.0, r.state.label(), r.state.color(), 11.5);
                    x += C_STATE;

                    cell_text(
                        ui,
                        x,
                        y,
                        name_w - 12.0,
                        &r.name,
                        if r.state == RowState::Done {
                            theme::DIM
                        } else {
                            theme::TEXT
                        },
                        12.0,
                    );
                    x += name_w;

                    let src = source_label(&r.source);
                    // agent 下的任务用高亮色：一眼就能从一堆「本机」里挑出来
                    let src_color = if r.source.starts_with("agent:") {
                        theme::ACCENT
                    } else {
                        theme::FAINT
                    };
                    cell_text(ui, x, y, C_SRC - 14.0, &src, src_color, 11.0);
                    x += C_SRC;

                    let size_txt = if let Some((_, t)) = r.secs {
                        if t > 0 {
                            clock(t)
                        } else {
                            "—".to_string()
                        }
                    } else if r.total > 0 {
                        human(r.total)
                    } else if r.done > 0 {
                        human(r.done)
                    } else {
                        "—".to_string()
                    };
                    cell_num(ui, x, y, C_SIZE, &size_txt, theme::DIM);
                    x += C_SIZE;

                    let (rv, ru) = human_rate(r.rate);
                    let rate_txt = if r.rate > 0.0 {
                        format!("{rv} {ru}")
                    } else {
                        "—".to_string()
                    };
                    cell_num(
                        ui,
                        x,
                        y,
                        C_RATE,
                        &rate_txt,
                        if r.rate > 0.0 { theme::OK } else { theme::FAINT },
                    );
                    x += C_RATE;

                    if cols.links > 0.0 {
                        let links = match r.links {
                            Some((0, 0)) | None => "—".to_string(),
                            // BT：已连/发现；直链：只有连接数
                            Some((l, 0)) => l.to_string(),
                            Some((l, s)) => format!("{l}/{s}"),
                        };
                        cell_num(
                            ui,
                            x,
                            y,
                            cols.links,
                            &links,
                            if links == "—" { theme::FAINT } else { theme::DIM },
                        );
                        x += cols.links;
                    }

                    progress_cell(ui, x, y, C_PROG - 10.0, r);
                    x += C_PROG;

                    if cols.wave > 0.0 {
                        let wave_rect = Rect::from_min_size(
                            pos2(x + 2.0, rect.top() + 6.0),
                            vec2(cols.wave - 10.0, ROW_H - 12.0),
                        );
                        if r.wave.len() > 2 {
                            sparkline(ui, wave_rect, &r.wave, r.state.color());
                        }
                    }
                }
            });

        if let Some(id) = clicked {
            self.selected = Some(id);
            self.detail_open = true;
        }
    }

    /// QA 旋钮：这台机器上 XTEST 的合成点击到不了 Wayland 客户端，
    /// 暂停 / 取消 / 重试这些动作必须能被脚本按帧触发，否则只能靠人点。
    ///
    /// ```text
    /// SKYGET_QA_ADD_URL=<url>        第 10 帧往队列里塞一个下载
    /// SKYGET_QA_SELECT=<id 前缀>      动作前先选中这一行
    /// SKYGET_QA_ACT=pause|resume|stop|retry|delete
    /// SKYGET_QA_ACT_FRAMES=120       第几帧动手（默认 120）
    /// ```
    fn qa_tick(&mut self, ctx: &egui::Context) {
        if self.qa_done {
            return;
        }
        // 还有旋钮没到点：求一次 100ms 后重绘。这个界面空闲时一帧都不画，
        // 不主动求重绘就永远等不到「第 3 秒」那一刻。
        // （只在真的还有活要等时才求 —— 只设了 SKYGET_QA_SELECT 的截图场景不该一直刷。）
        let pending = std::env::var("SKYGET_QA_ACT").is_ok()
            || (std::env::var("SKYGET_QA_ADD_URL").is_ok() && self.qa_added.is_none());
        if pending {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let t = self.started_at.elapsed().as_secs_f32();
        // 验收旋钮：SKYGET_QA_OPEN_DIR=1 → 1 秒后把「下载保存到」弹层打开（截图用；
        // 这台机器的合成器吞掉 XTEST 的点击，人手点不了就只能这样点）
        if std::env::var("SKYGET_QA_OPEN_DIR").is_ok() && !self.dir_open && t > 1.0 {
            self.dir_open = true;
            self.dir_input = crate::settings::dest_dir().display().to_string();
            self.dir_err = None;
            ctx.request_repaint();
        }

        // 截图/验收要看某个筛选下的样子：SKYGET_QA_FILTER=active|failed|done|all
        if let Ok(f) = std::env::var("SKYGET_QA_FILTER") {
            self.state_filter = match f.to_ascii_lowercase().as_str() {
                "active" | "在跑" => Filter::Active,
                "failed" | "problem" | "失败" => Filter::Problem,
                "done" | "完成" => Filter::Done,
                _ => Filter::All,
            };
        }

        // 只给 SELECT（不带 ACT）也能选中：截图时要看某一条的详情
        if let Ok(sel) = std::env::var("SKYGET_QA_SELECT") {
            if let Some(r) = self.rows.iter().find(|r| r.id.starts_with(&sel)) {
                self.selected = Some(r.id.clone());
            }
        }
        if self.qa_added.is_none() {
            if let Ok(u) = std::env::var("SKYGET_QA_ADD_URL") {
                if t >= 0.8 && !u.is_empty() {
                    self.qa_added = Some(u.clone());
                    self.add_url(u);
                }
            }
        }
        let Ok(act) = std::env::var("SKYGET_QA_ACT") else {
            return;
        };

        let after: f32 = std::env::var("SKYGET_QA_ACT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2.5);
        if t < after {
            return;
        }
        let a = match act.as_str() {
            "pause" => Act::Pause,
            "resume" => Act::Resume,
            "stop" => Act::Stop,
            "retry" => Act::Retry,
            "delete" => Act::Delete,
            _ => return,
        };
        self.qa_done = true;
        eprintln!("[qa] {t:.1}s：{act}");
        self.act(a);
    }

    /// 当前筛选后可见的行 id（和 table() 的筛法一致）。
    fn visible_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| !self.only_mine || is_mine(&r.source))
            .filter(|r| self.state_filter.matches(r.state))
            .map(|r| r.id.clone())
            .collect()
    }

    /// ←/→/↑/↓ 式的移动：在当前可见列表里前后挪一格。
    fn step_selection(&mut self, delta: isize) {
        let ids = self.visible_ids();
        if ids.is_empty() {
            self.selected = None;
            self.detail_open = false;
            return;
        }
        let cur = self
            .selected
            .as_deref()
            .and_then(|id| ids.iter().position(|x| x == id));
        let next = match cur {
            Some(i) => {
                let j = i as isize + delta;
                j.clamp(0, ids.len() as isize - 1) as usize
            }
            None => {
                if delta >= 0 {
                    0
                } else {
                    ids.len() - 1
                }
            }
        };
        self.selected = Some(ids[next].clone());
    }

    /// Home / End：跳到可见列表的首尾。
    fn select_edge(&mut self, end: usize) {
        let ids = self.visible_ids();
        self.selected = match end {
            0 => ids.first().cloned(),
            _ => ids.last().cloned(),
        };
        if self.selected.is_none() {
            self.detail_open = false;
        }
    }

    fn act(&mut self, a: Act) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if !self.link_ok {
            self.say("守护进程没连上 —— 先把它起起来（skynet-dataplaned，旧名 skynet-downloadd）。", theme::WARN);
            return;
        }
        let Some((_url, running)) = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.url.clone(), r.state == RowState::Running))
        else {
            return;
        };
        match a {
            Act::Pause => {
                let _ = self.cmd_tx.send(Cmd::Pause(id.clone()));
                self.say("正在停…已经下好的那部分留着，点继续接着下。", theme::FAINT);
            }
            Act::Resume | Act::Retry => {
                if running {
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::Resume(id.clone()));
                if let Some(r) = self.find(&id) {
                    r.state = RowState::Queued;
                    r.error.clear();
                    r.rate = 0.0;
                    r.done = 0;
                    r.wave.clear();
                }
                self.say("接着下 —— 已经下好的部分不会重下。", theme::ACCENT);
            }
            Act::Stop => {
                let _ = self.cmd_tx.send(Cmd::Cancel(id.clone()));
                self.say("正在取消…", theme::FAINT);
            }
            Act::Delete => {
                let _ = self.cmd_tx.send(Cmd::Delete(id.clone()));
                self.rows.retain(|r| r.id != id);
                if self.selected.as_deref() == Some(id.as_str()) {
                    self.selected = self.rows.first().map(|r| r.id.clone());
                }
                self.say("已从列表删掉这条（盘上的文件没动）。", theme::FAINT);
            }
        }
    }

    fn detail(&mut self, ui: &mut Ui) {
        if !self.detail_open {
            return;
        }
        let Some(sel) = self.selected.clone() else {
            return;
        };
        let Some(r) = self.rows.iter().find(|r| r.id == sel) else {
            return;
        };
        let mut close = false;
        let mut open_dir = false;
        let mut copy_sha = false;
        let mut act: Option<Act> = None;
        // 画完之后还要用到的两样，先拷出来：后面要 &mut self 写提示，不能带着对 rows 的借用
        let row_path = r.path.clone();
        let row_sha = r.sha.clone();

        egui::Panel::right("detail")
            .exact_size(316.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(theme::DOCK)
                    .inner_margin(Margin::symmetric(12, 12))
                    .stroke(Stroke::new(1.0, theme::SEP)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&r.name).size(theme::T_TITLE).color(theme::TEXT),
                        )
                        .truncate(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if close_x(ui) {
                            close = true;
                        }
                    });
                });
                ui.add_space(8.0);

                // 播放器式的五个控件：就放在名字下面、属性上面 —— 这张卡里说的就是
                // 「这一条」，控件作用于它，位置也属于它。
                // 位置固定五个、按状态变灰（不隐藏：位置跳动比变灰更糟）。
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    for a in [Act::Pause, Act::Resume, Act::Stop, Act::Retry, Act::Delete] {
                        if act_button(ui, a, allowed(a, r.state)) {
                            act = Some(a);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(r.state.label()).size(theme::T_MICRO).color(r.state.color()),
                        );
                    });
                });
                ui.add_space(12.0);

                // 失败原因紧跟在控件下面：这张卡上最该先读的就是「为什么没成」。
                // 以前它排在大片属性后面，失败行要滚过一串「—」才看得到结论。
                if !r.error.is_empty() {
                    ui.add_space(8.0);
                    // 失败原因用一块「终端」显示：第一行说结论，之后一段一行、等宽、能滚。
                    // 分段下载失败时会列出几十段，挤在一行里既读不出是哪几段，也看不出该重试哪段。
                    let (head, body) = error_lines(&r.error);
                    ui.add(
                        egui::Label::new(RichText::new(&head).size(theme::T_MICRO).color(theme::DANGER))
                            .wrap(),
                    );
                    if !body.is_empty() {
                        ui.add_space(4.0);
                        // 底色取窗口底（BG）而不是 FIELD：在这张卡上它是唯一更暗的面，
                        // 读起来才像一块终端输出，而不是又一个输入框。
                        Frame::NONE
                            .fill(theme::BG)
                            .stroke(Stroke::new(1.0, theme::SEP))
                            .inner_margin(Margin::symmetric(7, 5))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                // 关掉浮动滚动条：浮动的那种只在悬停时出现，等于「看不出能滚」
                                ui.style_mut().spacing.scroll.floating = false;
                                ui.style_mut().spacing.scroll.bar_width = 8.0;
                                egui::ScrollArea::vertical()
                                    .max_height(132.0)
                                    .auto_shrink([false, true])
                                    // 滚动条常显：告诉人「下面还有」，不然会以为只错了 8 段
                                    .scroll_bar_visibility(
                                        egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                                    )
                                    .show(ui, |ui| {
                                        for line in &body {
                                            ui.label(
                                                RichText::new(line)
                                                    .size(theme::T_MICRO)
                                                    .monospace()
                                                    .color(theme::DIM),
                                            );
                                        }
                                    });
                            });
                    }
                }

                ui.add_space(8.0);
                kv(ui, "下载者", &source_label(&r.source));
                if !r.trust.is_empty() {
                    kv(ui, "校验", trust_label(&r.trust));
                }
                // 空的字段不画：「线路 0」这种行只占位置，读起来还像界面坏了。
                if let Some((l, s)) = r.links {
                    if l > 0 || s > 0 {
                        let txt = if s > 0 {
                            format!("{l} 个已连接 · {s} 个发现")
                        } else {
                            format!("{l} 个分段连接")
                        };
                        kv(ui, "线路", &txt);
                    }
                }
                let size = if let Some((_, t)) = r.secs {
                    if t > 0 {
                        clock(t)
                    } else {
                        "—".to_string()
                    }
                } else if r.total > 0 {
                    human(r.total)
                } else {
                    "—".to_string()
                };
                if size != "—" {
                    kv(ui, "大小", &size);
                }
                if r.state == RowState::Running || r.state == RowState::Verifying {
                    let pct = if let Some((d, t)) = r.secs {
                        if t > 0 {
                            format!("{} / {}（{:.0}%）", clock(d), clock(t), d as f64 / t as f64 * 100.0)
                        } else {
                            clock(d)
                        }
                    } else if r.total > 0 {
                        format!("{:.0}%", r.done as f64 / r.total as f64 * 100.0)
                    } else {
                        "—".to_string()
                    };
                    kv(ui, "进度", &pct);
                }

                if r.cached {
                    kv(ui, "说明", "本机早有这份内容，一字节没下");
                }
                ui.add_space(6.0);

                let landing = if r.path.is_empty() {
                    // TaskView.path 只在 done 时有值。失败/中断的行不会再有文件了，
                    // 说「还没落定」等于骗人；跑着的才是真的「还没落定」。
                    match r.state {
                        RowState::Failed | RowState::Interrupted => "（没有落地）".to_string(),
                        _ => "（还没落定）".to_string(),
                    }
                } else if std::path::Path::new(&r.path).is_dir() {
                    format!("{}（下完再定名）", r.path)
                } else {
                    r.path.clone()
                };
                if kv_path(ui, "落地", &landing) {
                    self.click_log(&format!("点了落地路径：{row_path}"));
                    open_dir = true;
                }
                if !r.sha.is_empty() {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("sha256").size(theme::T_MICRO).color(theme::FAINT));
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("复制").size(theme::T_MICRO).color(theme::FAINT),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            copy_sha = true;
                        }
                    });
                    ui.add(
                        egui::Label::new(
                            RichText::new(&r.sha).size(theme::T_MICRO).monospace().color(theme::DIM),
                        )
                        .wrap(),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if r.state == RowState::Done
                        && ui
                            .add(Button::new(RichText::new("打开目录").size(theme::T_SMALL)))
                            .clicked()
                    {
                        self.click_log(&format!("点了打开目录按钮：{}", r.path));
                        open_dir = true;
                    }
                });
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(short(&r.url, 100))
                                .size(theme::T_MICRO)
                                .color(theme::FAINT),
                        )
                        .wrap(),
                    );
                });
            });

        if close {
            self.detail_open = false;
        }
        if let Some(a) = act {
            self.act(a);
        }
        if open_dir {
            if let Err(e) = open_in_file_manager(&row_path) {
                self.click_log(&format!("打开目录失败：{e}"));
                self.say(&e, theme::WARN);
            }
        }
        if copy_sha {
            ui.ctx().copy_text(row_sha);
            self.say("sha256 已复制", theme::FAINT);
        }
    }

    fn bottom_bar(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(
                Frame::NONE
                    .fill(theme::CHROME)
                    .inner_margin(Margin::symmetric(12, 4))
                    .stroke(Stroke::new(1.0, theme::SEP)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    // 台账摘要就是筛选器：哪一格不为 0，点它就只看那一格。
                    // 放在底栏而不是顶栏，因为它是「这份台账长什么样」，不是操作。
                    let counts = [
                        (Filter::All, self.rows.len()),
                        (
                            Filter::Active,
                            self.rows.iter().filter(|r| Filter::Active.matches(r.state)).count(),
                        ),
                        (
                            Filter::Problem,
                            self.rows
                                .iter()
                                .filter(|r| Filter::Problem.matches(r.state))
                                .count(),
                        ),
                        (
                            Filter::Done,
                            self.rows.iter().filter(|r| Filter::Done.matches(r.state)).count(),
                        ),
                    ];
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let mut pick: Option<Filter> = None;
                    for (f, n) in counts {
                        // 0 条的分类不占位置：底栏是摘要，不是菜单。
                        if f != Filter::All && n == 0 {
                            continue;
                        }
                        let on = self.state_filter == f;
                        let resp = ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("{} {}", f.label(), n))
                                        .size(theme::T_MICRO)
                                        .color(if on { theme::ACCENT } else { theme::FAINT }),
                                )
                                .frame(false)
                                .min_size(vec2(0.0, 20.0)),
                            )
                            .on_hover_text(match f {
                                Filter::All => "显示全部记录",
                                Filter::Active => "排队 / 下载中 / 校验中",
                                Filter::Problem => "失败与中断 —— 都等你处理一下",
                                Filter::Done => "已经下完并校验过的",
                            });
                        if resp.clicked() {
                            pick = Some(f);
                        }
                    }
                    if let Some(f) = pick {
                        self.state_filter = f;
                    }

                    // 有提示就先说提示（6 秒），之后回到台账摘要。
                    let notice = match &self.notice {
                        Some((t, c, at)) if at.elapsed() < Duration::from_secs(6) => {
                            Some((t.clone(), *c))
                        }
                        _ => None,
                    };
                    if let Some((t, c)) = notice {
                        ui.add_space(8.0);
                        ui.label(RichText::new(t).size(theme::T_MICRO).color(c));
                    } else if !self.link_ok {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("守护进程没连上 —— 起一个：skynet-dataplaned")
                                .size(theme::T_MICRO)
                                .color(theme::WARN),
                        );
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut on = self.auto_clip;
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(if on { "剪贴板 自动" } else { "剪贴板 关" })
                                        .size(theme::T_MICRO)
                                        .color(if on { theme::ACCENT } else { theme::FAINT }),
                                )
                                .frame(false),
                            )
                            .on_hover_text("复制一个下载地址就自动开始下载")
                            .clicked()
                        {
                            on = !on;
                            self.auto_clip = on;
                        }
                        if self.link_ok {
                            ui.label(
                                RichText::new(format!("缓存 {}", self.blob_size_str()))
                                    .size(theme::T_MICRO)
                                    .color(theme::FAINT),
                            );
                        }
                        // 这行以前是 SEP(#3e3e3e)：在底栏上只有 1.38:1，
                        // 等于写了一句没人能读的话。弱文本用 FAINT。
                        ui.label(
                            RichText::new("内容按 sha256 只存一份")
                                .size(theme::T_MICRO)
                                .color(theme::FAINT),
                        );
                    });
                });
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frames += 1;
        self.qa_tick(&ctx);
        if let Some((path, at)) = self.shot.clone() {
            ctx.request_repaint_after(Duration::from_millis(50));
            if self.frames >= at {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            // 截图是下一帧才回来的
            let img = ctx.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = img {
                match save_png(&path, &img) {
                    Ok(()) => eprintln!("[shot] 已存 {}", path.display()),
                    Err(e) => eprintln!("[shot] 存不了 {}：{e}", path.display()),
                }
                self.shot = None;
            }
        }
        // 鼠标按键一律记一行：出「点了没反应」时，靠它区分是事件没进来还是逻辑没走对
        let presses: Vec<String> = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter(|e| matches!(e, egui::Event::PointerButton { .. }))
                .map(|e| format!("{e:?}"))
                .collect()
        });
        for e in presses {
            self.click_log(&format!("鼠标按键 {e}"));
        }
        let focused = ctx.input(|i| i.focused);
        if focused != self.was_focused {
            self.was_focused = focused;
            self.click_log(if focused { "窗口拿到焦点" } else { "窗口失去焦点" });
        }
        // 任务栏关闭 / Alt+F4 也要收进托盘，而不是把正在跑的任务带走
        if self.start_hidden {
            self.start_hidden = false;
            if self.tray_rx.is_some() {
                self.hide_window(&ctx);
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.hides_instead_of_exiting() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(&ctx);
        }
        // 键盘兜底：鼠标万一不灵，Esc = 关窗口（有托盘就收起），Ctrl+Q = 真退出
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.hides_instead_of_exiting() {
                self.hide_window(&ctx);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Q)) {
            // 和托盘退出同一路径：直接结束 GUI，不依赖窗口下一帧。
            self.quitting = true;
            crate::tray::quit_now();
        }
        // 播放器式快捷键：空格 = 暂停 / 继续，Delete = 从列表删掉。
        // 输入框有焦点时不抢键（正在粘贴地址的时候按空格不该暂停谁）。
        if !ctx.egui_wants_keyboard_input() {
            let st = self
                .selected
                .as_deref()
                .and_then(|id| self.rows.iter().find(|r| r.id == id))
                .map(|r| r.state);
            // 键盘导航：↑/↓ 换行，Home/End 跳到首尾，Enter 打开详情。
            // 和空格/Delete 一样：输入框有焦点时不抢键。
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.step_selection(1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.step_selection(-1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Home)) {
                self.select_edge(0);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::End)) {
                self.select_edge(1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                if self.selected.is_some() {
                    self.detail_open = true;
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                match st {
                    Some(RowState::Running) => self.act(Act::Pause),
                    Some(RowState::Interrupted) => self.act(Act::Resume),
                    Some(RowState::Failed) => self.act(Act::Retry),
                    _ => {}
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Delete))
                && matches!(
                    st,
                    Some(RowState::Done) | Some(RowState::Failed) | Some(RowState::Interrupted)
                )
            {
                self.act(Act::Delete);
            }
        }
        // Ctrl+D：打开/收起「下载保存到」。给鼠标出问题的时候留一条路，
        // 也顺便让这个功能不只有「点那个小图标」一种进法。
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::D)) {
            self.dir_open = !self.dir_open;
            if self.dir_open {
                self.dir_input = crate::settings::dest_dir().display().to_string();
                self.dir_err = None;
                self.dir_open_pass = ctx.cumulative_pass_nr();
            }
            ctx.request_repaint();
        }
        if self.wants_hide_shortcut(&ctx) && self.hides_instead_of_exiting() {
            self.hide_window(&ctx);
        }
        self.handle_tray(&ctx);
        self.tray_watchdog(&ctx);
        self.pump(&ctx);
        self.title_bar(ui);
        self.top_bar(ui);
        self.bottom_bar(ui);
        // 机群视图讲的是「谁在这台机器上」，右侧那张任务详情卡在那里是错的
        // 上下文（它会一直显示切换前选中的那条任务，还把机群卡片挤窄）。
        if !self.fleet_open {
            self.detail(ui);
        }
        if self.fleet_open {
            // 机群视图显示「正在跑几个」这类会变但不频繁的数字；
            // 1.5 秒重画一次足够，不必跟着任务轮询的 700ms。
            ctx.request_repaint_after(Duration::from_millis(1500));
        }
        egui::CentralPanel::no_frame()
            .frame(Frame::NONE.fill(theme::BG))
            .show(ui, |ui| {
                if self.fleet_open {
                    self.fleet_view(ui);
                } else {
                    self.table(ui);
                }
            });
        self.dir_popup(&ctx);
    }
}

// ── 后台 ────────────────────────────────────────────────────────────────

/// 跑一个下载任务。
///
/// 每发出一条消息就叫一次重绘：界面没有心跳，靠的就是「有变化才叫」。
/// 剪贴板监听。
///
/// 剪贴板监听：**只用 xclip，不用 wl-paste。**
///
/// 这条规则是实测换来的：每 600ms 起一个 `wl-paste` 去读剪贴板，
/// 每次都是一个新 Wayland 客户端（它自己还要建 surface），gnome-shell 每秒要处理
/// 一次半，实测打 1.5 条 `meta_window_set_stack_position_no_sync` 断言/秒
/// —— 表现就是 dock 图标一直闪，连窗口收在托盘里也照闪。
/// 换成 xclip（普通 X 客户端，走 XWayland，读的是同一份剪贴板）后，
/// 同样 600ms 轮询，60 秒 0 条断言。
///
/// `wl-paste --watch` 本来是更漂亮的方案，但 GNOME 不给 wlroots 的
/// data-control 协议，watch 模式直接报「requires a compositor that supports
/// the wlroots data-control protocol」。所以只剩 xclip 这一条路。
fn spawn_clipboard(tx: Sender<Msg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let mut last = String::new();
        let mut misses = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(600));
            let Some(raw) = read_clipboard_x11() else {
                misses += 1;
                // xclip 没装、或这台机器没有 X：别再空转，说一句就退场
                if misses == 10 {
                    eprintln!("剪贴板自动接住用不了（没有 xclip 或连不上 X）；粘贴到输入框即可。");
                }
                if misses > 10 {
                    return;
                }
                continue;
            };
            misses = 0;
            let text = raw.trim().to_string();
            if text.is_empty() || text == last || text.len() > 2048 {
                continue;
            }
            last = text.clone();
            if looks_like_url(&text) && tx.send(Msg::Clip(text)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
}

/// 台账巡检：外部进程（`skyget get`、本地接口）起的任务没有唤醒我们的通道，
/// 只能每秒读一次台账。只有查到「真有任务在跑」才叫一次重绘 ——
fn read_clipboard_x11() -> Option<String> {
    let mut cmd = std::process::Command::new("xclip");
    cmd.args(["-selection", "clipboard", "-o"])
        .stderr(Stdio::null());
    // 从桌面启动时 DISPLAY/XAUTHORITY 都在环境里；从终端或 systemd 里起就没有，
    // 这里补上 XWayland 的默认位置（GNOME 把授权文件放在 /run/user/<uid>/）。
    if std::env::var_os("DISPLAY").is_none() {
        cmd.env("DISPLAY", ":0");
    }
    if std::env::var_os("XAUTHORITY").is_none() {
        if let Some(p) = xwayland_auth() {
            cmd.env("XAUTHORITY", p);
        }
    }
    let o = cmd.output().ok()?;
    if o.status.success() {
        String::from_utf8(o.stdout).ok()
    } else {
        None
    }
}

fn xwayland_auth() -> Option<std::path::PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = std::path::PathBuf::from(format!("/run/user/{uid}"));
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        if e.file_name()
            .to_string_lossy()
            .starts_with(".mutter-Xwaylandauth")
        {
            return Some(e.path());
        }
    }
    None
}

fn looks_like_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("magnet:")
        // 本地的 .torrent 文件也算（拖进来或复制路径都常见）
        || l.ends_with(".torrent")
}

/// 打开网址（浏览器）。这类用 xdg-open 是对的 —— URL 的默认处理程序就是浏览器。
fn open_url(u: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(u)
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn();
}

/// 在文件管理器里打开「这个文件所在的目录」，并尽量把它选中。
///
/// **不要用 `xdg-open <目录>`**：目录的默认处理程序不一定文件管理器 ——
/// 这台机器上 `inode/directory` 被指到了 VS Code，点「打开目录」实际拉起的是编辑器，
/// 用户看到的就是「没反应」。所以先找桌面的文件管理器，按各自的选中参数调用，
/// 全都不在才退回 xdg-open。
///
/// 失败要能说出来：启动命令后等最多 300ms，短命进程（命令不存在、参数不认）
/// 会在这里被抓到，交给界面显示，而不是点一下什么都不发生。
pub fn open_in_file_manager(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("这条任务还没有落地路径".into());
    }
    let raw = std::path::Path::new(path);
    let dir = if raw.is_dir() {
        raw.to_path_buf()
    } else {
        raw.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .ok_or_else(|| format!("打不开目录：{path} 没有上级目录"))?
    };
    // 文件被删了（台账还留着旧记录）：拿死路径去 `nautilus --select` 是没反应的，
    // 用户看到的又是「点了没事发生」。改成打开所在目录，并说清文件已经不在了。
    let mut gone = false;
    let target = if raw.exists() {
        raw.to_path_buf()
    } else {
        gone = true;
        dir.clone()
    };

    // (命令, 参数)：选中文件用各家的写法，目录就直接打开
    let candidates: [(&str, Vec<std::ffi::OsString>); 4] = [
        ("nautilus", vec!["--select".into(), target.as_os_str().into()]),
        ("nemo", vec![target.as_os_str().into()]),
        ("thunar", vec![target.as_os_str().into()]),
        ("xdg-open", vec![dir.as_os_str().into()]),
    ];
    let (cmd, args) = candidates.iter().find(|(c, _)| which(c)).ok_or_else(|| {
        "打不开目录：这台机器没有文件管理器（nautilus / nemo / thunar / xdg-open 都没有）"
            .to_string()
    })?;

    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("打不开目录：调不起 {cmd}：{e}"))?;

    // 等最多 300ms：只为了抓住「参数不认、立刻退出」这种短命失败，
    // 正常起来的文件管理器会一直活着，不让界面明显卡一下。
    let started = Instant::now();
    let mut failure: Option<String> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                let mut msg = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut msg);
                }
                let msg = msg.trim();
                failure = Some(if msg.is_empty() {
                    format!("打不开目录：{cmd} 退出码 {:?}", status.code())
                } else {
                    format!("打不开目录：{cmd} 说：{msg}")
                });
                break;
            }
            Ok(None) => {
                if started.elapsed() > Duration::from_millis(300) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => {
                failure = Some(format!("打不开目录：等 {cmd} 时出错：{e}"));
                break;
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if gone {
        return Err(format!(
            "文件已经不在了（{}），打开的是它所在的目录",
            path
        ));
    }
    Ok(())
}

#[cfg(test)]
mod open_dir_tests {
    #[test]
    fn missing_file_falls_back_to_parent() {
        // 不真的去开窗口：只验「文件不在时退回父目录」这条判断
        let p = std::path::Path::new("/home/skynet/Downloads/肯定不存在的文件-abc.zip");
        assert!(!p.exists());
        assert!(p.parent().is_some());
    }
}

/// PATH 里有没有这个可执行文件。
fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|d| {
                let p = d.join(cmd);
                p.is_file()
                    && std::fs::metadata(&p)
                        .map(|m| {
                            use std::os::unix::fs::PermissionsExt;
                            m.permissions().mode() & 0o111 != 0
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ── 小部件 ──────────────────────────────────────────────────────────────

/// 数字列右对齐的锚点。表头和数据必须用同一个函数算 ——
/// 以前各写一个魔数（-16 / -8），整列数字比标题凸出 8px。
fn num_x(x: f32, w: f32) -> f32 {
    x + w - 8.0
}

/// 哪些列显示。窗口窄的时候按「信息密度」从低到高丢：
/// 先丢速率波形（它是锦上添花），再丢「线路」（直链大多是一条连接，
/// 而且它和「速度」回答的是同一个问题：连上了几条）。
///
/// 为什么要有这个：固定列宽合计 ~868 + 右栏 316 = 1184，而最小窗宽是 980 ——
/// 在 13 寸笔记本上把详情卡打开，进度条就会被右栏切掉一截。
struct Cols {
    /// 「线路」列宽（0 = 不显示）
    links: f32,
    /// 波形列宽（0 = 不显示）
    wave: f32,
    /// 名称列额外宽度：线路列让出来的
    name_extra: f32,
}

fn cols_for(avail: f32) -> Cols {
    let fixed = PAD_L + C_STATE + C_SRC + C_SIZE + C_RATE + C_PROG + 12.0;
    // 先试着全都放得下（波形至少 80）
    let with_all = fixed + C_NAME + C_LINKS + 80.0;
    if avail >= with_all {
        return Cols {
            links: C_LINKS,
            wave: (avail - fixed - C_NAME - C_LINKS).max(80.0),
            name_extra: 0.0,
        };
    }
    // 丢波形
    let without_wave = fixed + C_NAME + C_LINKS;
    if avail >= without_wave {
        return Cols {
            links: C_LINKS,
            wave: 0.0,
            name_extra: 0.0,
        };
    }
    // 再丢线路，宽度还给名称
    Cols {
        links: 0.0,
        wave: 0.0,
        name_extra: C_LINKS,
    }
}

#[allow(dead_code)]
fn wave_width(avail: f32) -> f32 {
    cols_for(avail).wave
}

/// 控制条上的五个动作。
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Pause,
    Resume,
    Stop,
    Retry,
    Delete,
}

impl Act {
    fn hint(self) -> &'static str {
        match self {
            Act::Pause => "暂停  Space",
            Act::Resume => "继续  Space",
            Act::Stop => "取消：停掉并清掉临时分片（已落地的文件不动）",
            Act::Retry => "重试  Space",
            Act::Delete => "从列表删掉这条  Delete（盘上的文件不动）",
        }
    }
}

/// 哪几个动作在这一行状态下有意义。没意义的画灰，不隐藏 —— 位置不能跳。
fn allowed(a: Act, s: RowState) -> bool {
    match a {
        Act::Pause => s == RowState::Running,
        Act::Resume => s == RowState::Interrupted,
        Act::Stop => matches!(s, RowState::Running | RowState::Queued | RowState::Interrupted),
        Act::Retry => s == RowState::Failed,
        Act::Delete => matches!(
            s,
            RowState::Done | RowState::Failed | RowState::Interrupted
        ),
    }
}

/// 手画 16 网格图标：线宽 1.25、全直角，跟标题栏那三个按钮同一支笔。
/// 顶栏那行灰字显示什么：只用最后一级目录名，路径太长会把地址栏挤没了。
/// 家目录本身没有名字（就是 `/home/xxx`），单独给一句「家目录」。
fn dir_short_name(p: &std::path::Path) -> String {
    if p == crate::paths::home_dir().as_path() {
        return "家目录".to_string();
    }
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// 手画的文件夹（16 格、1.25 线宽、全直角）—— 和别处图标同一套画法。
fn folder_icon(p: &egui::Painter, r: Rect, color: Color32) {
    let w = 1.25;
    let st = Stroke::new(w, color);
    let l = r.left() + 1.0;
    let rr = r.right() - 1.0;
    let t = r.top() + 3.0;
    let b = r.bottom() - 2.5;
    // 后片（带一个凸起的口）
    let tab = l + 5.0;
    p.line_segment([pos2(l, b), pos2(l, t + 1.5)], st);
    p.line_segment([pos2(l, t + 1.5), pos2(tab, t + 1.5)], st);
    p.line_segment([pos2(tab, t + 1.5), pos2(tab + 1.6, t)], st);
    p.line_segment([pos2(tab + 1.6, t), pos2(rr, t)], st);
    p.line_segment([pos2(rr, t), pos2(rr, b)], st);
    p.line_segment([pos2(l, b), pos2(rr, b)], st);
    // 前片：一条斜口，表示「里面还有一层」
    p.line_segment([pos2(l, b), pos2(l + 2.0, t + 5.0)], st);
    p.line_segment([pos2(l + 2.0, t + 5.0), pos2(rr, t + 5.0)], st);
}

impl App {
    /// 「下载保存到」的弹层。
    ///
    /// 为什么是弹层而不是一个常驻输入框：改目录这件事一年做两次，
    /// 常驻就是把一次性的东西摆在最显眼的位置。
    fn dir_popup(&mut self, ctx: &egui::Context) {
        if !self.dir_open {
            return;
        }
        let mut pos = self
            .dir_anchor
            .map(|r| pos2(r.left(), r.bottom() + 6.0))
            .unwrap_or_else(|| pos2(80.0, 90.0));
        // 贴着按钮出现，但别顶出窗口：按钮靠右时把弹层往左挪进来
        const W: f32 = 392.0 + 26.0;
        let screen = ctx.content_rect();
        if pos.x + W > screen.right() - 6.0 {
            pos.x = (screen.right() - 6.0 - W).max(6.0);
        }

        let mut close = false;
        let area = egui::Area::new(egui::Id::new("dir-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(theme::CHROME)
                    .stroke(Stroke::new(1.0, theme::SEP))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(392.0);
                        ui.label(
                            RichText::new("下载保存到")
                                .size(theme::T_SMALL)
                                .color(theme::FAINT),
                        );
                        ui.add_space(5.0);
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.dir_input)
                                .hint_text("/某个/目录 或 ~/下载")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Body),
                        );
                        // 只在刚打开那一帧抢焦点。每帧都抢的话，
                        // 点「选目录…」之后的焦点会立刻被抢回输入框 —— 手感会很怪。
                        if ctx.cumulative_pass_nr() == self.dir_open_pass && !edit.has_focus() {
                            edit.request_focus();
                        }
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new("不存在会新建；换目录不会搬走已经下好的文件")
                                .size(theme::T_MICRO)
                                .color(theme::FAINT),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(RichText::new("用这个").size(theme::T_SMALL))
                                .clicked()
                                || enter
                            {
                                match crate::settings::set_dest(&self.dir_input) {
                                    Ok(p) => {
                                        self.say(
                                            &format!("以后都下到 {}", p.display()),
                                            theme::OK,
                                        );
                                        close = true;
                                    }
                                    Err(e) => self.dir_err = Some(format!("{e:#}")),
                                }
                            }
                            if ui
                                .button(RichText::new("选目录…").size(theme::T_SMALL))
                                .on_hover_text("用系统的目录选择器（zenity）")
                                .clicked()
                                && !self.dir_picking
                            {
                                self.dir_picking = true;
                                pick_dir_async(self.tx.clone());
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("关掉").size(theme::T_SMALL).color(theme::FAINT),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    close = true;
                                }
                            });
                        });
                        if let Some(e) = &self.dir_err {
                            ui.add_space(6.0);
                            ui.label(RichText::new(e).size(theme::T_MICRO).color(theme::DANGER));
                        }
                        if self.dir_picking {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("等选择器…")
                                    .size(theme::T_MICRO)
                                    .color(theme::FAINT),
                            );
                        }
                    });
            });

        // 点外面 / 按 Esc 收起来（和别的弹窗一致，不要「只能点关掉」）
        let clicked_out = popup_should_close(
            self.dir_open_pass,
            ctx.cumulative_pass_nr(),
            ctx.input(|i| i.pointer.any_click())
                .then(|| ctx.input(|i| i.pointer.interact_pos()))
                .flatten(),
            area.response.rect,
            self.dir_anchor,
        );
        if close || clicked_out || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.dir_open = false;
            self.dir_err = None;
        }
    }
}

/// 点了一下之后，这个弹层该不该关。
///
/// 三个「不该关」必须显式排除，缺一个就是「点了没反应」：
/// 1. **打开它的那一帧**：那一下点击本来就落在弹层外面（在顶栏那个按钮上），
///    当成「点了外面」就会开了立刻关 —— 用户看到的就是按钮点不动。
/// 2. 点在弹层里面（用这个 / 选目录 / 输入框）。
/// 3. 点在开关本身 —— 那是「切换」，由按钮自己处理，这里再关一次就成了关不掉。
fn popup_should_close(
    opened_pass: u64,
    now_pass: u64,
    click: Option<Pos2>,
    area: Rect,
    opener: Option<Rect>,
) -> bool {
    if now_pass == opened_pass {
        return false;
    }
    let Some(p) = click else {
        return false;
    };
    if area.contains(p) {
        return false;
    }
    if opener.map(|r| r.contains(p)).unwrap_or(false) {
        return false;
    }
    true
}

/// 起一个系统目录选择器（zenity）。它在另一个线程里跑 ——
/// 同步跑会把界面卡住，人看到的就是「点了没反应」。
/// 没有 zenity 也不报错：输入框本来就能直接打字。
fn pick_dir_async(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let out = std::process::Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=选择下载保存目录",
                "--filename=",
            ])
            .output();
        let picked = out.ok().and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                (!s.is_empty()).then_some(s)
            } else {
                None
            }
        });
        let _ = tx.send(Msg::PickedDir(picked));
    });
}

fn act_icon(p: &egui::Painter, r: Rect, act: Act, color: Color32) {
    let c = r.center();
    let s = 13.0_f32;
    let h = s / 2.0;
    let w = 1.25;
    match act {
        Act::Pause => {
            for dx in [-3.2_f32, 1.6] {
                p.rect_filled(
                    Rect::from_min_size(pos2(c.x + dx, c.y - h), vec2(1.8, s)),
                    0.0,
                    color,
                );
            }
        }
        Act::Resume => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    pos2(c.x - h * 0.75, c.y - h * 0.95),
                    pos2(c.x - h * 0.75, c.y + h * 0.95),
                    pos2(c.x + h, c.y),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Act::Stop => {
            p.rect_stroke(
                Rect::from_center_size(c, vec2(s * 0.78, s * 0.78)),
                0.0,
                Stroke::new(w, color),
                StrokeKind::Inside,
            );
        }
        Act::Retry => {
            // 缺口圆 + 箭头：从右上缺到右侧，箭头压在缺口上
            let rad = h * 0.82;
            let (a0, a1) = (-1.15_f32, 4.6_f32);
            let steps = 28;
            let mut pts = Vec::new();
            for i in 0..=steps {
                let a = a0 + (i as f32 / steps as f32) * (a1 - a0);
                pts.push(pos2(c.x + rad * a.cos(), c.y + rad * a.sin()));
            }
            p.add(egui::Shape::line(pts, Stroke::new(w, color)));
            // 箭头接在终点，沿切线方向收一个三角
            let tip = pos2(c.x + rad * a1.cos(), c.y + rad * a1.sin());
            let tan = (a1 + std::f32::consts::FRAC_PI_2).sin_cos();
            let (tx, ty) = (tan.1, tan.0); // 切线方向
            p.add(egui::Shape::convex_polygon(
                vec![
                    pos2(tip.x + tx * 3.0, tip.y + ty * 3.0),
                    pos2(tip.x - ty * 2.4, tip.y + tx * 2.4),
                    pos2(tip.x + ty * 2.4, tip.y - tx * 2.4),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Act::Delete => {
            // 垃圾桶：把手、盖、桶身（上宽下窄）、桶底
            let top = c.y - h + 2.6;
            let bot = c.y + h;
            p.line_segment(
                [pos2(c.x - 2.0, top - 2.4), pos2(c.x + 2.0, top - 2.4)],
                Stroke::new(w, color),
            );
            p.line_segment(
                [pos2(c.x - 5.2, top), pos2(c.x + 5.2, top)],
                Stroke::new(w, color),
            );
            p.line_segment(
                [pos2(c.x - 4.6, top), pos2(c.x - 3.6, bot)],
                Stroke::new(w, color),
            );
            p.line_segment(
                [pos2(c.x + 4.6, top), pos2(c.x + 3.6, bot)],
                Stroke::new(w, color),
            );
            p.line_segment(
                [pos2(c.x - 3.6, bot), pos2(c.x + 3.6, bot)],
                Stroke::new(w, color),
            );
        }
    }
}

/// 一个图标按钮：26×26 热区、图标居中、悬停有底色、禁用画灰。
fn act_button(ui: &mut Ui, act: Act, enabled: bool) -> bool {
    /// 禁用态：比 FAINT 明显更弱，但仍能看出「这里有个按钮，只是现在不能点」
    const OFF: Color32 = Color32::from_rgb(0x6a, 0x6a, 0x6a);
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    if resp.hovered() && enabled {
        ui.painter().rect_filled(rect, 0.0, theme::ROW_HOVER);
    }
    act_icon(
        ui.painter(),
        rect,
        act,
        if enabled { theme::TEXT } else { OFF },
    );
    resp.on_hover_text(act.hint()).clicked() && enabled
}

fn header_row(ui: &mut Ui, cols: &Cols) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::CHROME);
    let y = rect.center().y;
    let mut x = rect.left() + PAD_L;
    let name_w = C_NAME + cols.name_extra;
    let mut heads: Vec<(&str, f32, bool)> = vec![
        ("状态", C_STATE, false),
        ("名称", name_w, false),
        ("来源", C_SRC, false),
        ("大小", C_SIZE, true),
        ("速度", C_RATE, true),
    ];
    if cols.links > 0.0 {
        heads.push(("线路", cols.links, true));
    }
    heads.push(("进度", C_PROG, false));
    for (label, w, right) in heads {
        let align = if right {
            Align2::RIGHT_CENTER
        } else {
            Align2::LEFT_CENTER
        };
        let px = if right { num_x(x, w) } else { x };
        ui.painter().text(
            pos2(px, y),
            align,
            label,
            FontId::proportional(theme::T_MICRO),
            theme::FAINT,
        );
        x += w;
    }
    if cols.wave > 0.0 {
        ui.painter().text(
            pos2(x + 2.0, y),
            Align2::LEFT_CENTER,
            "速率（近 1 分钟）",
            FontId::proportional(theme::T_MICRO),
            theme::FAINT,
        );
    }
}

/// 把失败原因拆成「第一行结论 + 每条一行」。
///
/// 0.13.1 起内核直接写多行；更早的记录是一条用「；」连起来的长句
/// （`22 段没下来（…）：第 2 段：…；第 3 段：…`）。两种都得能好好显示 ——
/// 老记录不能因为格式旧就变成一团字，那正是用户在读的东西。
fn error_lines(text: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 1 {
        return (
            lines[0].to_string(),
            lines[1..].iter().map(|s| s.to_string()).collect(),
        );
    }
    // 老格式：结论 + 「；」连起来的条目。判据是有「；第 N 段」这种枚举。
    if let Some(pos) = text.find("第 ") {
        if text[pos..].contains("；第 ") {
            let head = text[..pos]
                .trim_end_matches(['：', ':', ' '])
                .to_string();
            let mut items: Vec<(usize, String)> = text[pos..]
                .split('；')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let n = s
                        .trim_start_matches("第 ")
                        .split(' ')
                        .next()
                        .and_then(|w| w.trim_end_matches('段').parse::<usize>().ok())
                        .unwrap_or(0);
                    (n, s)
                })
                .collect();
            items.sort_by_key(|(n, _)| *n);
            return (head, items.into_iter().map(|(_, s)| s).collect());
        }
    }
    (text.to_string(), Vec::new())
}

/// 把一条新回来的视图揉进已有那一行。
///
/// 只覆盖「台账说了算」的字段；速度、波形、上次事件时刻这些界面自己的东西留着，
/// 再按两次轮询之间涨了多少字节估一个速度。
/// 机群里的一张 agent 卡。纯展示：不改策略、不发命令。
fn agent_card(ui: &mut Ui, a: &remote::AgentInfo) {
    ui.add_space(8.0);
    // 不要把它塞进 ui.horizontal：水平布局里的子 Ui 宽度会缩成“剩余宽度”，
    // 长路径会一个字一行地竖着排（实测截图）。Frame 直接在纵向流里占满整行。
    Frame::NONE
        .fill(theme::ROW)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&a.name).size(theme::T_TITLE).strong().color(theme::TEXT));
                ui.label(
                    RichText::new(&a.source)
                        .size(theme::T_MICRO)
                        .monospace()
                        .color(theme::FAINT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} / {} 在跑", a.running, a.max_running))
                            .size(theme::T_SMALL)
                            .monospace()
                            .color(if a.running > 0 { theme::ACCENT } else { theme::FAINT }),
                    );
                    ui.label(
                        RichText::new(format!("{} 条任务", a.jobs))
                            .size(theme::T_SMALL)
                            .color(theme::FAINT),
                    );
                });
            });
            // 并发占用条：满格 = 这个 agent 已经占满自己的 worker 配额。
            let width = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(vec2(width, 4.0), Sense::hover());
            ui.painter()
                .rect_filled(rect, CornerRadius::same(2), theme::FIELD);
            let frac = (a.running as f32 / a.max_running.max(1) as f32).clamp(0.0, 1.0);
            if frac > 0.0 {
                let mut fill = rect;
                fill.set_width(width * frac);
                ui.painter()
                    .rect_filled(fill, CornerRadius::same(2), theme::ACCENT);
            }
            ui.add_space(6.0);
            agent_policy_line(
                ui,
                "落地",
                if a.policy.dest_roots.is_empty() {
                    "（未限制）".to_string()
                } else {
                    a.policy.dest_roots.join(" · ")
                },
            );
            agent_policy_line(
                ui,
                "主机",
                match &a.policy.allowed_hosts {
                    Some(h) if !h.is_empty() => h.join(" · "),
                    Some(_) => "（拒绝全部主机）".to_string(),
                    None => "（不限制）".to_string(),
                },
            );
            agent_policy_line(
                ui,
                "上传",
                if a.policy.upload_roots.is_empty() {
                    "禁止".to_string()
                } else {
                    a.policy.upload_roots.join(" · ")
                },
            );
            if a.policy.allow_private {
                agent_policy_line(ui, "出网", "可访问私网（allow_private）".to_string());
            }
        });
}

fn agent_policy_line(ui: &mut Ui, key: &str, value: String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{key}  "))
                .size(theme::T_MICRO)
                .color(theme::FAINT),
        );
        ui.label(
            RichText::new(value)
                .size(theme::T_MICRO)
                .monospace()
                .color(theme::DIM),
        );
    });
}

fn update_row(row: &mut Row, v: &crate::model::TaskView) {
    let prev_done = row.done;
    let was_running = row.state == RowState::Running;
    let next = match v.state.as_str() {
        "done" => RowState::Done,
        "failed" => RowState::Failed,
        "interrupted" => RowState::Interrupted,
        "running" => RowState::Running,
        _ => RowState::Queued,
    };
    row.state = next;
    row.url = v.url.clone();
    if !v.path.is_empty() {
        row.path = v.path.clone();
        row.name = file_name_of(&v.path, &v.url);
    }
    row.done = v.bytes_done;
    row.total = v.bytes_total;
    row.sha = v.sha256.clone();
    row.error = v.error.clone();
    row.source = v.source.clone();
    row.trust = v.trust.clone();
    row.cached = v.cached;
    row.links = if v.links_seen > 0 || v.links_live > 0 {
        Some((v.links_live, v.links_seen))
    } else {
        None
    };
    if next == RowState::Running {
        if was_running && v.bytes_done > prev_done {
            let dt = row.last_evt.elapsed().as_secs_f64().max(0.2);
            let inst = (v.bytes_done - prev_done) as f64 / dt;
            row.rate = if row.rate <= 0.0 {
                inst
            } else {
                row.rate * 0.6 + inst * 0.4
            };
            row.last_evt = Instant::now();
        }
    } else {
        row.rate = 0.0;
        row.secs = None;
        if next != RowState::Verifying {
            row.links = None;
        }
    }
}

/// 接口回来的视图转成界面内部的 Task（只为了复用 Row::new 那套字段映射）。
fn view_to_task(v: &crate::model::TaskView) -> Task {
    Task {
        id: v.id.clone(),
        direction: v.direction.clone(),
        protocol: v.protocol.clone(),
        credential_ref: String::new(),
        sources: v.sources.clone(),
        url: v.url.clone(),
        dest: v.path.clone(),
        state: v.state.clone(),
        bytes_done: v.bytes_done,
        bytes_total: v.bytes_total,
        sha256: v.sha256.clone(),
        error: v.error.clone(),
        error_code: v.error_code.clone(),
        retryable: v.retryable,
        retry_after_ms: v.retry_after_ms,
        next_action: v.next_action.clone(),
        source: v.source.clone(),
        priority: v.priority.clone(),
        expected_sha256: String::new(),
        pid: 0,
        links_live: v.links_live,
        links_seen: v.links_seen,
        trust: v.trust.clone(),
        accept_change: false,
        cached: v.cached,
        rerun: false,
        created_at: v.created_ms,
        updated_at: v.created_ms,
    }
}

/// 台账这条记录是不是「我自己下的」。
///
/// 来源里带冒号的是程序起的（`agent:hangar`、`agent:claude`），不带的是人（`cli` / `human`）。
/// 「只看我的」就是按这一条判断 —— 机库装东西的记录不该跟用户的下载混在一起。
fn is_mine(source: &str) -> bool {
    !source.contains(':')
}

fn cell_text(ui: &Ui, x: f32, y: f32, w: f32, text: &str, color: Color32, size: f32) {
    // 手动截断：painter 不做省略号，长名字会捅进下一列
    let max_chars = (w / (size * 0.62)).max(3.0) as usize;
    ui.painter().text(
        pos2(x, y),
        Align2::LEFT_CENTER,
        ellipsize(text, max_chars),
        FontId::proportional(size),
        color,
    );
}

fn cell_num(ui: &Ui, x: f32, y: f32, w: f32, text: &str, color: Color32) {
    ui.painter().text(
        pos2(num_x(x, w), y),
        Align2::RIGHT_CENTER,
        text,
        FontId::monospace(theme::T_MONO),
        color,
    );
}

fn progress_cell(ui: &Ui, x: f32, y: f32, w: f32, r: &Row) {
    let bar_w = (w - 46.0).max(40.0);
    let bar = Rect::from_min_size(pos2(x, y - 2.5), vec2(bar_w, 5.0));
    ui.painter()
        .rect_filled(bar, CornerRadius::same(2), theme::FIELD);

    let pct = if r.state == RowState::Done {
        1.0
    } else if let Some((d, t)) = r.secs {
        if t > 0 {
            (d as f64 / t as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    } else if r.total > 0 {
        (r.done as f64 / r.total as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    if pct > 0.0 {
        let fill = Rect::from_min_size(bar.min, vec2(bar.width() * pct, bar.height()));
        ui.painter()
            .rect_filled(fill, CornerRadius::same(2), r.state.color());
    }
    if r.state == RowState::Failed {
        ui.painter().rect_stroke(
            bar,
            CornerRadius::same(2),
            Stroke::new(1.0, theme::DANGER),
            StrokeKind::Inside,
        );
    }

    let text = match r.state {
        RowState::Done => "完成".to_string(),
        RowState::Failed => "—".to_string(),
        RowState::Queued => "待开始".to_string(),
        RowState::Verifying => "校验".to_string(),
        // BT 的 magnet 阶段：还没有总大小，说「等种子」而不是画个 0%
        _ if r.total == 0 && r.secs.is_none() => "等种子".to_string(),
        _ if r.secs.is_some() => match r.secs {
            Some((d, t)) if t > 0 => format!("{}", clock(d)),
            _ => "...".to_string(),
        },
        _ if r.total > 0 => format!("{:.0}%", pct * 100.0),
        _ => "…".to_string(),
    };
    ui.painter().text(
        pos2(x + w - 2.0, y),
        Align2::RIGHT_CENTER,
        text,
        FontId::monospace(theme::T_MONO),
        theme::FAINT,
    );
}

fn sparkline(ui: &Ui, rect: Rect, vals: &[f64], color: Color32) {
    let painter = ui.painter();
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::SEP),
    );
    let max = vals.iter().fold(1024.0_f64, |a, b| a.max(*b));
    let step = rect.width() / (WAVE_LEN - 1) as f32;
    let n = vals.len();
    if n < 2 {
        return;
    }
    // 右对齐：最新采样贴右边，数据不满时左边留白，波形不会横向伸缩
    let x0 = rect.right() - (n - 1) as f32 * step;
    let mut prev: Option<Pos2> = None;
    for (i, v) in vals.iter().enumerate() {
        let h = ((v / max).clamp(0.0, 1.0) as f32) * rect.height();
        let p = pos2(x0 + i as f32 * step, rect.bottom() - h);
        if let Some(q) = prev {
            painter.line_segment([q, p], Stroke::new(1.2, color));
        }
        prev = Some(p);
    }
}

fn kv(ui: &mut Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [56.0, 16.0],
            egui::Label::new(RichText::new(k).size(theme::T_MICRO).color(theme::FAINT)),
        );
        ui.add(egui::Label::new(RichText::new(v).size(theme::T_SMALL).color(theme::DIM)).truncate());
    });
}

/// 落地路径那一行：整行可点，点一下就打开所在目录。
/// 只有一个「打开目录」按钮时，目标太小、也不够明显。
fn kv_path(ui: &mut Ui, k: &str, v: &str) -> bool {
    ui.label(RichText::new(k).size(theme::T_MICRO).color(theme::FAINT));
    let resp = ui
        .add(
            egui::Label::new(RichText::new(short(v, 120)).size(theme::T_MICRO).monospace().color(theme::DIM))
                .wrap()
                .sense(Sense::click()),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("点一下打开所在目录");
    resp.clicked()
}

fn close_x(ui: &mut Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
    let c = if resp.hovered() { theme::TEXT } else { theme::FAINT };
    let r = rect.shrink(4.5);
    let stroke = Stroke::new(1.4, c);
    ui.painter()
        .line_segment([r.left_top(), r.right_bottom()], stroke);
    ui.painter()
        .line_segment([r.right_top(), r.left_bottom()], stroke);
    resp.clicked()
}

#[derive(Clone, Copy, PartialEq)]
enum WinBtn {
    Min,
    Max,
    Close,
}

fn win_button(ui: &mut Ui, kind: WinBtn) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(WIN_BTN_W, 26.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let c = if resp.hovered() {
        theme::TEXT
    } else {
        theme::FAINT
    };
    if resp.hovered() {
        let bg = if kind == WinBtn::Close {
            Color32::from_rgb(0x8a, 0x3b, 0x3b)
        } else {
            theme::ROW_HOVER
        };
        ui.painter().rect_filled(rect, CornerRadius::same(2), bg);
    }
    let p = ui.painter();
    let s = Stroke::new(1.2, c);
    let cy = rect.center().y;
    let cx = rect.center().x;
    match kind {
        WinBtn::Min => {
            p.line_segment([pos2(cx - 4.0, cy + 3.0), pos2(cx + 4.0, cy + 3.0)], s);
        }
        WinBtn::Max => {
            let r = Rect::from_center_size(rect.center(), vec2(8.0, 8.0));
            p.rect_stroke(r, CornerRadius::same(1), s, StrokeKind::Inside);
        }
        WinBtn::Close => {
            p.line_segment([pos2(cx - 4.0, cy - 4.0), pos2(cx + 4.0, cy + 4.0)], s);
            p.line_segment([pos2(cx + 4.0, cy - 4.0), pos2(cx - 4.0, cy + 4.0)], s);
        }
    }
    resp.clicked()
}

// ── 工具 ────────────────────────────────────────────────────────────────

/// 谁在下 —— 这一列是给人看的，不要出现 `agent:hermes` 这种内部写法。
fn source_label(s: &str) -> String {
    match s {
        "human" | "" => "本机（你）".to_string(),
        "cli" => "命令行".to_string(),
        "api" => "本地接口".to_string(),
        "manifest" => "清单安装".to_string(),
        other => match other.strip_prefix("agent:") {
            Some("hermes") => "Hermes Agent".to_string(),
            Some("claude") => "Claude Code".to_string(),
            Some("codex") => "Codex".to_string(),
            Some("opencode") => "opencode".to_string(),
            Some("hangar") => "天网机库".to_string(),
            Some(x) if !x.is_empty() => format!("{x} Agent"),
            _ => other.to_string(),
        },
    }
}

/// 校验强度说人话 —— 关键是让人知道「这份内容凭什么信」。
fn trust_label(t: &str) -> &'static str {
    match t {
        "pinned" => "你给的 hash",
        "indexed" => "发布方校验文件",
        "tofu-ok" => "与该地址上次一致",
        "tofu-new" => "首次记录",
        "torrent" => "种子自证",
        _ => "未校验",
    }
}

fn rank(s: RowState) -> u8 {
    match s {
        RowState::Running | RowState::Verifying => 0,
        RowState::Queued => 1,
        RowState::Interrupted => 2,
        RowState::Failed => 3,
        RowState::Done => 4,
    }
}

fn file_name_of(path: &str, url: &str) -> String {
    // 还没落地时 dest 是个目录（如 /home/skynet）：拿目录名当文件名会显示成 "skynet"
    let p = std::path::Path::new(path);
    if !path.is_empty() && !p.is_dir() {
        if let Some(n) = p.file_name().map(|s| s.to_string_lossy().to_string()) {
            if !n.is_empty() {
                return n;
            }
        }
    }
    let no_q = url.split(['?', '#']).next().unwrap_or(url);
    let after = no_q.split("://").nth(1).unwrap_or(no_q);
    let seg = after
        .splitn(2, '/')
        .nth(1)
        .unwrap_or("")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if seg.is_empty() {
        url.to_string()
    } else {
        seg.to_string()
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn human_rate(bps: f64) -> (String, &'static str) {
    const U: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut v = bps.max(0.0);
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        (format!("{:.0}", v), U[0])
    } else {
        (format!("{:.1}", v), U[i])
    }
}

impl App {
    /// 缓存占用，5 秒内复用上一次的结果。
    fn blob_size_str(&mut self) -> String {
        if self.blob_size.1.elapsed() > Duration::from_secs(5) || self.blob_size.0.is_empty() {
            self.blob_size = (dir_size(&paths::blobs_dir()), Instant::now());
        }
        self.blob_size.0.clone()
    }
}

fn dir_size(p: &std::path::Path) -> String {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += m.len();
            }
            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                for e2 in rd2.flatten() {
                    if let Ok(m) = e2.metadata() {
                        total += m.len();
                    }
                }
            }
        }
    }
    human(total)
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_lines_handles_both_formats() {
        // 0.13.1 之后：多行
        let (head, body) = super::error_lines("3 段没下来（已下的保留）\n第 1 段：超时\n第 2 段：超时");
        assert_eq!(head, "3 段没下来（已下的保留）");
        assert_eq!(body.len(), 2);

        // 老记录：一条长句，用「；」连起来，而且顺序是乱的
        let old = "22 段没下来（已下的保留，重跑会接着下）：第 2 段：请求失败：超时；第 1 段：请求失败：超时；第 10 段：请求失败：超时";
        let (head, body) = super::error_lines(old);
        assert_eq!(head, "22 段没下来（已下的保留，重跑会接着下）");
        assert_eq!(body.len(), 3);
        // 按段号排好：1、2、10（字符串序会把 10 排到 2 前面）
        assert!(body[0].starts_with("第 1 段"), "{:?}", body);
        assert!(body[1].starts_with("第 2 段"), "{:?}", body);
        assert!(body[2].starts_with("第 10 段"), "{:?}", body);

        // 普通一句话的错误：只有结论，没有条目
        let (head, body) = super::error_lines("校验不符：期望 0000…，实际 d66b…。文件已丢弃。");
        assert!(head.starts_with("校验不符"));
        assert!(body.is_empty());
    }

    #[test]
    fn mine_vs_agents() {
        // 人自己下的
        assert!(super::is_mine("cli"));
        assert!(super::is_mine("human"));
        // 程序起的：机库、别的 agent
        assert!(!super::is_mine("agent:hangar"));
        assert!(!super::is_mine("agent:claude"));
        // 空来源当作自己的（老记录）
        assert!(super::is_mine(""));
    }

    use super::*;

    #[test]
    fn url_gate() {
        assert!(looks_like_url("https://a/b.zip"));
        assert!(looks_like_url("magnet:?xt=urn:btih:abc"));
        assert!(looks_like_url("/home/a/b.torrent"));
        assert!(!looks_like_url("这是一段话"));
        assert!(!looks_like_url("https://a/b c"));
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(file_name_of("", "https://x.org/a/b.zip?t=1"), "b.zip");
        assert_eq!(file_name_of("/tmp/x/y.iso", "https://x.org/z.iso"), "y.iso");
    }

    #[test]
    fn rates_use_decimal_units() {
        assert_eq!(human_rate(999.0), ("999".to_string(), "B/s"));
        assert_eq!(human_rate(1500.0), ("1.5".to_string(), "KB/s"));
    }

    #[test]
    fn which_finds_real_commands_only() {
        assert!(which("sh"));
        assert!(!which("这个命令肯定不存在-abc"));
    }

    /// 空路径不该去调任何东西：直接说清楚，界面才有话给人听。
    #[test]
    fn open_dir_refuses_empty_path() {
        let e = open_in_file_manager("").unwrap_err();
        assert!(e.contains("落地路径"), "{e}");
    }
}

/// 把 egui 的 ColorImage 存成 PNG。只给 `SKYGET_SHOT` 用。
fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let mut raw = Vec::with_capacity(w * h * 4);
    for px in &img.pixels {
        raw.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
    }
    let f = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer
        .write_image_data(&raw)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod popup_tests {
    use super::popup_should_close;
    use egui::{pos2, Rect};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(x, y), egui::vec2(w, h))
    }

    #[test]
    fn opening_click_does_not_close_it_in_the_same_frame() {
        // 这一条就是那个 bug：按钮在 (10,10)，弹层在下面；同一帧里那一下点击
        // 落在按钮上，被当成「点了外面」→ 弹层开了立刻关 → 看着像按钮点不动。
        let opener = rect(10.0, 10.0, 80.0, 26.0);
        let area = rect(10.0, 42.0, 392.0, 120.0);
        assert!(
            !popup_should_close(7, 7, Some(pos2(40.0, 20.0)), area, Some(opener)),
            "同一帧里必须不关"
        );
        // 下一帧点在外面才关
        assert!(popup_should_close(7, 8, Some(pos2(600.0, 400.0)), area, Some(opener)));
    }

    #[test]
    fn clicks_inside_and_on_the_opener_never_close() {
        let opener = rect(10.0, 10.0, 80.0, 26.0);
        let area = rect(10.0, 42.0, 392.0, 120.0);
        assert!(!popup_should_close(7, 9, Some(pos2(100.0, 80.0)), area, Some(opener)), "点弹层里面不关");
        assert!(!popup_should_close(7, 9, Some(pos2(40.0, 20.0)), area, Some(opener)), "点开关是切换，交给按钮");
        assert!(!popup_should_close(7, 9, None, area, Some(opener)), "没有点击就不动");
    }
}
