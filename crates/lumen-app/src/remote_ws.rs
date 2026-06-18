//! M5.3 终端远程 · part2a：客户端 WebSocket 控制面引擎。
//!
//! 与 `remote.rs`（M5.2 HTTP 心跳/设备列表）同款**后台线程 + channel**范式，不引
//! tokio：一条后台线程用**同步** `tungstenite` 维持到 `lumen-server` 的 WS 长连接，
//! 收发 [`lumen_protocol::remote`] 控制面消息；UI 帧不阻塞，后台有事件时
//! `ctx.request_repaint()` 唤醒。
//!
//! # 线程模型（读超时单线程）
//! `tungstenite` 是阻塞同步 socket，读写都需 `&mut`。后台线程把底层 `TcpStream`
//! 设 [`READ_TIMEOUT`] 读超时，单循环内交替：①排空 UI 投来的出站命令队列并写出
//! ②周期 [`Ping`](lumen_protocol::remote::RemoteC2S::Ping) 保活 ③带超时读一条消息
//! （超时即「暂无消息」，非错误）。连接断开则退避 [`RECONNECT_DELAY`] 重连。出站
//! 命令最坏延迟一个读超时（控制面人工节奏足够；part3 数据面再调小）。
//!
//! # 生命周期（与 `remote.rs` 对称，挂同样的主循环钩子）
//! 登录后 [`RemoteWs::start`]（须已有 token），每帧 [`RemoteWs::poll`] 收取后台
//! 事件并推进 UI 态，登出 [`RemoteWs::stop`]。本模块（part2a）只做**引擎 + 状态
//! 机**；配对弹窗 / 被控横幅 / 设备「连接」入口等 UI 在 part2b 消费这里暴露的态。
//!
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lumen_protocol::remote::{
    DenyReason, EndReason, PairingFailReason, RemoteC2S, RemoteFrame, RemoteS2C, Role,
};
use lumen_term::{SelPoint, Selection, Terminal};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{ClientRequestBuilder, Message, WebSocket};
use winit::event_loop::EventLoopProxy;

use crate::cloud::server_url;
use crate::PtyWake;

/// 控制端镜像 Terminal 的 scrollback 上限（行）。被控端转发的实时输出滚出可见区后
/// 进镜像的历史，控制端可上滚回看。
const MIRROR_SCROLLBACK: usize = 5000;
/// 镜像 Terminal 创建时的占位尺寸（首个 `Resize` 帧到达前；被控端会立即发实际尺寸）。
const MIRROR_INIT_ROWS: usize = 24;
/// 同上：占位列数。
const MIRROR_INIT_COLS: usize = 80;
/// part3d 历史行缓存上限（绝对行 → VT 字节）。超限时淘汰离当前视口最远的行。
const HISTORY_CACHE_CAP: usize = 8000;
/// part3d 单次历史请求/应答的行数硬上限。**控制端请求与被控端应答必须共用此值**：
/// 否则被控端截断后，控制端按返回行数销 `hist_inflight`，超出部分的绝对行永久卡在
/// 在途集合、永不重拉，回看窗口出现永久空白（rows>~85 时单窗口预取量即可超 256）。
pub const HISTORY_CHUNK_MAX: u16 = 1024;

/// 读超时：无消息时 `read` 返回，循环转去处理出站/保活/停止（兼顾响应与不空转）。
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// 应用层 Ping 周期（保活 + 刷新服务端 `last_seen`）。
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// 断线后重连退避。
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// 控制端待配对态：已发起请求、正等用户输入被控端展示的配对码。
#[derive(Debug, Clone)]
pub struct PairingPrompt {
    /// 目标（被控端）设备 id（提交 [`RemoteC2S::SubmitPairing`] 时回填）。
    pub target_device_id: String,
    /// 目标设备显示名。
    pub target_name: String,
    /// 上次配对码校验失败原因（首次为 `None`）。
    pub last_error: Option<PairingFailReason>,
    /// 剩余尝试次数（仅在收到 [`RemoteS2C::PairingResult`] 后有意义）。
    pub attempts_left: Option<u32>,
}

/// 被控端来件态：有控制端请求控制本机，醒目展示配对码 + 可拒绝。
#[derive(Debug, Clone)]
pub struct IncomingControl {
    /// 控制端设备显示名。
    pub controller_name: String,
    /// 本机展示给对方转述的 9 位配对码。
    pub pairing_code: String,
}

/// 活跃会话态（控制中 / 被控中）。
#[derive(Debug, Clone)]
pub struct ActiveSession {
    /// 对端设备显示名。
    pub peer_name: String,
    /// 本端角色（[`Role::Controller`] = 控制中；[`Role::Controlled`] = 被控中）。
    pub role: Role,
}

/// 一次性通知（main 循环 [`RemoteWs::take_notices`] 取走 → 弹 toast）。
#[derive(Debug, Clone)]
pub enum Notice {
    /// 控制请求被拒（离线 / 已被控 / 被拒 / 跨用户 / 自控等）。
    ControlDenied(DenyReason),
    /// 被控端的未决配对被取消（控制端撤销 / 超时）。
    PairingCancelled(DenyReason),
    /// 配对码连错作废 / 过期。
    PairingFailed(PairingFailReason),
    /// 会话已建立（本端角色 + 对端设备名）。
    SessionStarted {
        /// 本端角色。
        role: Role,
        /// 对端设备显示名。
        peer: String,
    },
    /// 会话结束（对端离开 / 断线 / 被取代）。
    SessionEnded(EndReason),
}

/// 后台线程 → 主线程事件。
enum WsEvent {
    /// 连接已建立。
    Connected,
    /// 连接断开（含主动停止前的退场）。
    Disconnected,
    /// 收到一条服务端消息。
    Server(Box<RemoteS2C>),
}

/// 客户端远程控制 WS 引擎（主线程持有）。
#[derive(Default)]
pub struct RemoteWs {
    /// 控制端：待输入配对码态（part2b 渲染弹窗）。
    pub pairing: Option<PairingPrompt>,
    /// 被控端：来件配对态（part2b 渲染横幅 + 配对码）。
    pub incoming: Option<IncomingControl>,
    /// 活跃会话态（part2b 渲染「控制中 / 正在被远程控制」横幅）。
    pub session: Option<ActiveSession>,
    /// M5.3 part3a 控制端镜像：被控端整屏状态在本地用无 PTY 的 `Terminal` 复现
    /// （`advance` 喂入被控端转发的 PTY 字节）。仅 `role==Controller` 会话期间存在。
    mirror: Option<Terminal>,
    /// M5.3 part3d 控制端回看：历史视图锚定的**绝对首行**（`None` = 跟随实时底部，
    /// 渲染 live `mirror`；`Some(top)` = 回看，渲染 `hist_term` 的 `[top, top+rows)` 窗口）。
    hist_top: Option<u64>,
    /// 控制端：被控端最新历史边界 `(base, screen_top)` 绝对行号（夹紧回看范围 + 跟随
    /// 实时输出推进；随快照的 `HistoryBounds` 与每次 `HistoryRows` 应答刷新）。
    hist_bounds: Option<(u64, u64)>,
    /// 控制端：按绝对行号缓存的历史行 VT 字节（回看渲染源；上限 [`HISTORY_CACHE_CAP`]）。
    hist_cache: HashMap<u64, Vec<u8>>,
    /// 控制端：已请求待回的历史绝对行（去重，避免每帧重复请求同段）。
    hist_inflight: HashSet<u64>,
    /// 控制端：历史渲染用 scratch `Terminal`（按 `hist_top` 窗口逐行填充，复用渲染器）。
    hist_term: Option<Terminal>,
    /// 控制端：`hist_term` 已构建的 `(top, version)`，无变化则跳过重建。
    hist_built: Option<(u64, u64)>,
    /// 控制端：历史缓存版本号（每次写入历史行 +1，驱动 `hist_term` 按需重建）。
    hist_version: u64,
    /// 被控端：待应答的历史行请求 `(top, count)`（main 从焦点窗格 term 序列化后应答）。
    pending_history: Vec<(u64, u16)>,
    /// M5.3 part4b 控制端镜像选区（作用于**当前显示**的镜像终端：跟随=mirror，回看=
    /// hist_term；按绝对行号定位，与渲染/取文本同坐标系）。右键有选区→复制本地剪贴板。
    mirror_selection: Option<Selection>,
    /// 控制端：镜像选区拖动中（左键按下到松开）。
    mirror_selecting: bool,
    /// M5.3 part4 被控端待执行的远程输入字节（控制端转发来）：main 每帧取走、经
    /// 「本地输入优先」仲裁后写入焦点窗格 PTY。
    pending_input: Vec<Vec<u8>>,
    /// M5.3 被控端待应用的远程视口尺寸（控制端请求；SSH 式跟随）：main 取走后
    /// 把焦点窗格 resize 到此 (rows, cols)。仅保留最新值。
    pending_viewport: Option<(u16, u16)>,
    /// 待消费的一次性通知（main 取走弹 toast）。
    notices: Vec<Notice>,
    /// UI → 后台 出站命令发送端。
    cmd_tx: Option<Sender<RemoteC2S>>,
    /// 后台 → UI 事件接收端。
    evt_rx: Option<Receiver<WsEvent>>,
    /// 停止标志（登出 / Drop 时置位）。
    stop: Option<Arc<AtomicBool>>,
}

/// 控制端一帧镜像的渲染源（[`RemoteWs::mirror_render`] 产出）：跟随态借 live `mirror`，
/// 回看态借填好窗口的 `hist_term`，连同该帧应画的光标（回看态光标隐藏）。
pub struct MirrorFrame<'a> {
    /// 本帧要渲染的终端（live 镜像或历史窗口 scratch）。
    pub term: &'a Terminal,
    /// 光标 `(row, col, visible)`：跟随态取 live 光标，回看态 `(0, 0, false)` 隐藏。
    pub cursor: (usize, usize, bool),
    /// 控制端镜像选区（part4b）：与 `term` 同坐标系，渲染器据此画高亮。空/无则 `None`。
    pub selection: Option<&'a Selection>,
}

impl RemoteWs {
    /// 登录后启动后台 WS 线程（已在跑则先停旧的）。`token` 为账户 JWT。
    ///
    /// `proxy` + `wake_pending`：后台收到消息时除 `ctx.request_repaint()` 外，再发
    /// `PtyWake` user event 唤醒 winit 事件循环——**否则窗口失焦时 `request_repaint`
    /// 唤不醒空闲循环，远程消息（配对/输入/镜像）会卡到焦点回来才处理**（与 PTY
    /// 输出同款唤醒机制，共用 `wake_pending` 去重防事件风暴）。
    pub fn start(
        &mut self,
        token: String,
        ctx: egui::Context,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    ) {
        self.stop();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.cmd_tx = Some(cmd_tx);
        self.evt_rx = Some(evt_rx);
        self.stop = Some(stop.clone());
        if let Err(e) = thread::Builder::new()
            .name("lumen-remote-ws".into())
            .spawn(move || worker(&token, &cmd_rx, &evt_tx, &stop, &ctx, &proxy, &wake_pending))
        {
            log::error!("启动远程 WS 线程失败: {e}");
        }
    }

    /// 登出 / 停止：终止后台线程并清空所有远程态。
    pub fn stop(&mut self) {
        if let Some(s) = &self.stop {
            s.store(true, Ordering::SeqCst);
        }
        self.cmd_tx = None;
        self.evt_rx = None;
        self.stop = None;
        self.pairing = None;
        self.incoming = None;
        self.session = None;
        self.mirror = None;
        self.pending_input.clear();
        self.pending_viewport = None;
        self.pending_history.clear();
        self.reset_history();
        self.notices.clear();
    }

    /// 控制端：复位历史回看态（回跟随、清缓存/在途/边界/scratch）。会话起止、断线、
    /// 被控端 resize（绝对行号体系变更）时调用。`pending_history`（被控端侧）不在此清。
    fn reset_history(&mut self) {
        self.hist_top = None;
        self.hist_bounds = None;
        self.hist_cache.clear();
        self.hist_inflight.clear();
        self.hist_term = None;
        self.hist_built = None;
        // 显示坐标系换源/会话变更：选区作废。
        self.mirror_selection = None;
        self.mirror_selecting = false;
    }

    /// 是否已登录并在维持连接（后台线程在跑）。
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stop.is_some()
    }

    /// 每帧调用：收取后台事件、推进 UI 态。返回是否有更新（请求重绘用）。
    pub fn poll(&mut self) -> bool {
        let mut events = Vec::new();
        if let Some(rx) = &self.evt_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        let changed = !events.is_empty();
        for ev in events {
            self.apply(ev);
        }
        changed
    }

    /// 取走待消费通知（main 弹 toast 用）。
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// 控制端：发起控制 `target` 设备。
    pub fn request_control(&self, target: String) {
        self.send(RemoteC2S::RequestControl { target });
    }

    /// 控制端：提交当前待配对的配对码。
    pub fn submit_pairing(&self, code: String) {
        if let Some(p) = &self.pairing {
            self.send(RemoteC2S::SubmitPairing {
                target: p.target_device_id.clone(),
                code,
            });
        }
    }

    /// 控制端：取消当前待配对（仅清本地态；服务端 120s 后自动 GC）。
    pub fn cancel_pairing(&mut self) {
        self.pairing = None;
    }

    /// 被控端：拒绝来件控制请求。
    pub fn decline(&mut self) {
        self.send(RemoteC2S::DeclineControl);
        self.incoming = None;
    }

    /// 任一端：结束当前活跃会话。
    pub fn end_session(&mut self) {
        self.send(RemoteC2S::EndSession);
        self.session = None;
        self.mirror = None;
        self.pending_input.clear();
        self.pending_viewport = None;
        self.pending_history.clear();
        self.reset_history();
    }

    /// 被控端：转发焦点窗格 PTY 输出字节给控制端（含会话起始的整屏快照重放）。
    pub fn send_output(&self, bytes: &[u8]) {
        self.send_frame(&RemoteFrame::Output(bytes.to_vec()));
    }

    /// 被控端：告知控制端镜像终端尺寸（会话起始 + 窗格 resize 时发；须在对应
    /// 尺寸的 `Output` 之前）。
    pub fn send_resize(&self, rows: u16, cols: u16) {
        self.send_frame(&RemoteFrame::Resize { rows, cols });
    }

    /// 控制端：把用户输入的 VT 字节转发给被控端（part4）。
    ///
    /// 转发输入即把镜像 **snap 回跟随实时底部**（标准终端「输入即滚到底」）——否则
    /// 回看历史时打字会看不到自己输入的回显。是所有控制端→被控端输入（按键 / Ctrl+C /
    /// win32 / IME / 粘贴）的收口，故在此统一 snap。
    pub fn send_input(&mut self, bytes: &[u8]) {
        if self.hist_top.is_some() {
            self.hist_top = None;
            self.mirror_selection = None;
            self.mirror_selecting = false;
        }
        self.send_frame(&RemoteFrame::Input(bytes.to_vec()));
    }

    /// 控制端：请求被控端焦点窗格 resize 到控制端视图尺寸（SSH 式跟随）。
    pub fn send_viewport_resize(&self, rows: u16, cols: u16) {
        self.send_frame(&RemoteFrame::ViewportResize { rows, cols });
    }

    /// 被控端：取走待应用的远程视口尺寸（main 把焦点窗格 resize 到它）。
    pub fn take_viewport(&mut self) -> Option<(u16, u16)> {
        self.pending_viewport.take()
    }

    /// 被控端：取走待应答的历史行请求（main 从焦点窗格 term 序列化后回 `HistoryRows`）。
    pub fn take_history_reqs(&mut self) -> Vec<(u64, u16)> {
        std::mem::take(&mut self.pending_history)
    }

    /// 被控端：应答历史行请求（`lines[i]` 对应绝对行 `top+i`；回带当前历史边界）。
    pub fn send_history_rows(&self, top: u64, base: u64, screen_top: u64, lines: Vec<Vec<u8>>) {
        self.send_frame(&RemoteFrame::HistoryRows {
            top,
            base,
            screen_top,
            lines,
        });
    }

    /// 被控端：广播当前历史边界（会话起始随整屏快照发，控制端首次回看前即知可滚范围）。
    pub fn send_history_bounds(&self, base: u64, screen_top: u64) {
        self.send_frame(&RemoteFrame::HistoryBounds { base, screen_top });
    }

    /// 是否为控制端（控制中）：true 时本端键盘输入应转发而非本地执行。
    #[must_use]
    pub fn is_controlling(&self) -> bool {
        matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller))
    }

    /// 被控端：取走待执行的远程输入（main 仲裁后写焦点窗格 PTY）。
    pub fn take_input(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_input)
    }

    /// 控制端：滚轮回看镜像历史（part3d 按需拉取）。`lines > 0` 向上看更旧、`< 0` 向下；
    /// 按**绝对行**锚定窗口——被控端实时输出推进时回看内容不被推走（标准终端回滚行为）。
    /// 滚回底部即恢复「跟随实时」。返回是否改变了视图（驱动重绘）。
    pub fn scroll_mirror(&mut self, lines: isize) -> bool {
        if self.mirror.is_none() {
            return false;
        }
        let Some((base, screen_top)) = self.hist_bounds else {
            return false; // 边界未知（会话刚起、快照边界未到）：忽略本次滚动。
        };
        if screen_top <= base {
            return false; // 被控端无 scrollback 历史，无可回看。
        }
        // 当前窗口首行：跟随态视作可视区首行 screen_top。
        let cur_top = self.hist_top.unwrap_or(screen_top);
        // lines>0 向上 = 更旧 = 绝对行减小。
        let delta = i64::try_from(lines).unwrap_or(0);
        let new_top = i64::try_from(cur_top)
            .unwrap_or(i64::MAX)
            .saturating_sub(delta)
            .clamp(base as i64, screen_top as i64) as u64;
        // 抵达/越过可视区首行 = 回到跟随实时（hist_top = None）。
        let new_hist = (new_top < screen_top).then_some(new_top);
        if new_hist == self.hist_top {
            return false;
        }
        self.hist_top = new_hist;
        // 视图窗口变了（跟随↔回看 / 换窗口）：旧选区坐标作废。
        self.mirror_selection = None;
        self.mirror_selecting = false;
        true
    }

    /// 控制端：产出本帧镜像渲染源（[`MirrorFrame`]）。跟随态借 live `mirror`；回看态按
    /// `hist_top` 窗口**按需拉取缺失历史行**（发 `HistoryReq`）并填好 `hist_term` 再借出。
    /// 无镜像（非控制中）返回 `None`。须 `&mut`：回看时要拉取 + 构建 scratch。
    pub fn mirror_render(&mut self) -> Option<MirrorFrame<'_>> {
        // 视口行列取自 live 镜像（被控端 SSH 跟随后两端尺寸一致）。
        let (rows, cols) = {
            let g = self.mirror.as_ref()?.grid();
            (g.rows(), g.cols())
        };
        // 规整回看锚点：被控端边界推进/淘汰可能使 hist_top 越界——触底（≥可视区首行）
        // 回跟随实时，越下界（<最旧保留行，已被淘汰）夹到 base。锚点一变（显示窗口/坐标
        // 系换源）则选区作废，避免按旧窗口坐标错位高亮/取文本。
        if let (Some(top), Some((base, screen_top))) = (self.hist_top, self.hist_bounds) {
            let fixed = if top >= screen_top {
                None
            } else if top < base {
                Some(base)
            } else {
                Some(top)
            };
            if fixed != self.hist_top {
                self.hist_top = fixed;
                self.mirror_selection = None;
                self.mirror_selecting = false;
            }
        }
        let Some(top) = self.hist_top else {
            // 跟随实时：借 live 镜像 + 真实光标。
            let sel = self.mirror_selection.as_ref().filter(|s| !s.is_empty());
            let m = self.mirror.as_ref()?;
            let g = m.grid();
            return Some(MirrorFrame {
                term: m,
                cursor: (g.cursor.row, g.cursor.col, true),
                selection: sel,
            });
        };
        // 回看：拉取窗口缺失行 + 构建 scratch（光标隐藏）。
        self.fetch_history_window(top, rows);
        self.build_hist_term(top, rows, cols);
        let sel = self.mirror_selection.as_ref().filter(|s| !s.is_empty());
        let ht = self.hist_term.as_ref()?;
        Some(MirrorFrame {
            term: ht,
            cursor: (0, 0, false),
            selection: sel,
        })
    }

    /// 控制端：为回看窗口 `[top, top+rows)` 拉取缺失历史行（上下各约一屏预取，减少滚动
    /// 抖动时的请求次数）。已缓存 / 在途的行不重复请求。
    fn fetch_history_window(&mut self, top: u64, rows: usize) {
        let Some((base, screen_top)) = self.hist_bounds else {
            return;
        };
        let rows64 = rows as u64;
        let max_abs = screen_top.saturating_add(rows64); // 被控端保留区间上界（不含）
        let lo = top.saturating_sub(rows64).max(base);
        let hi = top
            .saturating_add(rows64.saturating_mul(2))
            .min(max_abs);
        if lo >= hi {
            return;
        }
        // 找窗口内首个「缺失且不在途」的行，从它单段请求到 hi（被控端按行返回，含已
        // 缓存行重复返回也无妨——单段比碎片化多请求更省往返）。
        let mut start = None;
        let mut abs = lo;
        while abs < hi {
            if !self.hist_cache.contains_key(&abs) && !self.hist_inflight.contains(&abs) {
                start = Some(abs);
                break;
            }
            abs += 1;
        }
        if let Some(start) = start {
            // 单段请求量夹在 HISTORY_CHUNK_MAX（与被控端应答上限同值，防 inflight 泄漏）。
            // 极端超大视口下剩余行下帧再补请求（缺失且不在途）→ 自愈，无永久空白。
            let count = (hi - start).min(u64::from(HISTORY_CHUNK_MAX)) as u16;
            self.request_history(start, count);
        }
    }

    /// 控制端：请求历史行 `[top, top+count)`，标记在途、发 `HistoryReq`。
    fn request_history(&mut self, top: u64, count: u16) {
        if count == 0 {
            return;
        }
        for a in top..top.saturating_add(u64::from(count)) {
            self.hist_inflight.insert(a);
        }
        self.send_frame(&RemoteFrame::HistoryReq { top, count });
    }

    /// 控制端：把回看窗口 `[top, top+rows)` 的缓存行填进 `hist_term`（缺失行留空白），
    /// 复用整套渲染器。`(top, version)` 未变则跳过重建。
    fn build_hist_term(&mut self, top: u64, rows: usize, cols: usize) {
        if self.hist_built == Some((top, self.hist_version)) {
            return;
        }
        let need_new = self
            .hist_term
            .as_ref()
            .is_none_or(|t| t.grid().rows() != rows || t.grid().cols() != cols);
        if need_new {
            self.hist_term = Some(Terminal::new(rows.max(1), cols.max(1), 0));
        }
        // 组装一段 VT：清屏 → 逐行定位 + 该行缓存字节（空则只定位、留空白）。
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\x1b[2J\x1b[H");
        for i in 0..rows {
            let abs = top + i as u64;
            let mut head = String::new();
            let _ = write!(head, "\x1b[{};1H", i + 1);
            buf.extend_from_slice(head.as_bytes());
            if let Some(bytes) = self.hist_cache.get(&abs) {
                buf.extend_from_slice(bytes);
            }
        }
        if let Some(t) = self.hist_term.as_mut() {
            t.advance(&buf);
            let _ = t.take_responses();
        }
        self.hist_built = Some((top, self.hist_version));
    }

    /// 控制端：历史缓存超上限时，淘汰离当前视口锚点最远的行。
    fn trim_history_cache(&mut self) {
        if self.hist_cache.len() <= HISTORY_CACHE_CAP {
            return;
        }
        let anchor = self
            .hist_top
            .or_else(|| self.hist_bounds.map(|(_, st)| st))
            .unwrap_or(0);
        let keep = HISTORY_CACHE_CAP / 2;
        let mut entries: Vec<u64> = self.hist_cache.keys().copied().collect();
        entries.sort_by_key(|&abs| abs.abs_diff(anchor));
        for abs in entries.into_iter().skip(keep) {
            self.hist_cache.remove(&abs);
        }
        // 缓存变动：作废已建窗口，下次 mirror_render 重建。
        self.hist_built = None;
    }

    // ── part4b 镜像选区 / 复制 / 粘贴 ──────────────────────────────────────

    /// 当前显示的镜像终端（跟随=live `mirror`，回看=`hist_term`；回看态 scratch 缺则回退
    /// live）。选区取文本 / 视图首行换算的坐标系基准。
    fn displayed_term(&self) -> Option<&Terminal> {
        if self.hist_top.is_none() {
            self.mirror.as_ref()
        } else {
            // 回看态严格只认 hist_term（view_top_abs_line()=0）：未建时返回 None（选区/
            // 取文本本帧 no-op，下帧 build_hist_term 后自愈），**不回退 live mirror**——
            // 否则坐标基准从 0 跳到 mirror 的大绝对行号，选区/取文本错位。
            self.hist_term.as_ref()
        }
    }

    /// 当前显示终端的视图首行绝对行号（鼠标 row → 选区绝对行号用）。
    fn displayed_view_top(&self) -> u64 {
        self.displayed_term()
            .map_or(0, |t| t.grid().view_top_abs_line())
    }

    /// 控制端：是否正在镜像区拖选（左键按下到松开）。
    #[must_use]
    pub fn mirror_selecting(&self) -> bool {
        self.mirror_selecting
    }

    /// 控制端：当前是否有非空镜像选区（Ctrl+C 第一级裁决用：keymap 据此决定复制 vs 中断）。
    #[must_use]
    pub fn has_mirror_selection(&self) -> bool {
        self.mirror_selection.is_some_and(|s| !s.is_empty())
    }

    /// 控制端（part4c）：被控端焦点窗格是否处于 win32-input 模式（镜像跟踪自 VT 流）。
    /// 转发按键时据此选 win32 编码 + 发 key-up，使被控端 win32 程序收到完整输入记录。
    #[must_use]
    pub fn mirror_win32_input(&self) -> bool {
        self.mirror.as_ref().is_some_and(Terminal::win32_input)
    }

    /// 控制端（part4c）：当前镜像光标 `(row, col)`（跟随态 Some；回看态 None）。IME
    /// 候选框定位到被控端光标处用。
    #[must_use]
    pub fn mirror_cursor(&self) -> Option<(usize, usize)> {
        if self.hist_top.is_some() {
            return None;
        }
        self.mirror.as_ref().map(|m| {
            let g = m.grid();
            // 加 display_offset 与渲染侧 cursor_view_row / 本地 IME 定位口径统一（镜像 grid
            // 当前恒 display_offset==0，显式加上以防将来非零时候选框纵向偏 display_offset 行）。
            (g.display_offset() + g.cursor.row, g.cursor.col)
        })
    }

    /// 控制端：在镜像区 `(row, col)` 起选（建空选区、进拖选态）。`row/col` 为显示终端
    /// 内的行列（调用方按镜像区像素换算并夹紧）。
    pub fn mirror_sel_start(&mut self, row: usize, col: usize) {
        let line = self.displayed_view_top() + row as u64;
        let p = SelPoint { line, col };
        self.mirror_selection = Some(Selection { anchor: p, head: p });
        self.mirror_selecting = true;
    }

    /// 控制端：拖动更新选区终点。返回是否真的移动了（驱动重绘）。
    pub fn mirror_sel_update(&mut self, row: usize, col: usize) -> bool {
        if !self.mirror_selecting {
            return false;
        }
        let head = SelPoint {
            line: self.displayed_view_top() + row as u64,
            col,
        };
        match self.mirror_selection.as_mut() {
            Some(sel) if sel.head != head => {
                sel.head = head;
                true
            }
            _ => false,
        }
    }

    /// 控制端：结束镜像拖选（仅点击未拖动 = 空选区则清掉）。
    pub fn mirror_sel_end(&mut self) {
        self.mirror_selecting = false;
        if self.mirror_selection.is_some_and(|s| s.is_empty()) {
            self.mirror_selection = None;
        }
    }

    /// 控制端：清空镜像选区（复制后 / 切换视图时）。
    pub fn clear_mirror_selection(&mut self) {
        self.mirror_selection = None;
        self.mirror_selecting = false;
    }

    /// 控制端：取当前显示镜像终端的选区文本（空选区 / 空文本返回 `None`，供复制到本地
    /// 剪贴板）。
    #[must_use]
    pub fn copy_mirror_selection(&self) -> Option<String> {
        let sel = self.mirror_selection.filter(|s| !s.is_empty())?;
        let text = self.displayed_term()?.selection_text(&sel);
        (!text.is_empty()).then_some(text)
    }

    /// 控制端：把文本作为「粘贴」转发给被控端 PTY——换行规整为 CR，按被控端 bracketed
    /// paste 模式（镜像跟踪自 VT 流）包裹，经 `RemoteFrame::Input` 发送。
    pub fn send_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.mirror.as_ref().is_some_and(Terminal::bracketed_paste);
        let payload = if bracketed {
            let mut p = Vec::with_capacity(normalized.len() + 12);
            p.extend_from_slice(b"\x1b[200~");
            p.extend_from_slice(normalized.as_bytes());
            p.extend_from_slice(b"\x1b[201~");
            p
        } else {
            normalized.into_bytes()
        };
        self.send_input(&payload);
    }

    /// 把数据面帧序列化为不透明 `Relay` 投递（序列化失败仅记日志、不断连）。
    fn send_frame(&self, frame: &RemoteFrame) {
        match frame.to_value() {
            Ok(v) => self.send(RemoteC2S::Relay(v)),
            Err(e) => log::error!("远程数据面帧序列化失败: {e}"),
        }
    }

    /// 投递一条出站命令（未连接则静默丢弃）。
    fn send(&self, msg: RemoteC2S) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(msg);
        }
    }

    /// 作用一帧数据面：控制端的 `Output`/`Resize` → 镜像 Terminal；被控端的
    /// `Input` → 待执行输入队列（main 仲裁后写 PTY）。按本端角色路由。
    fn apply_relay(&mut self, value: &serde_json::Value) {
        let Ok(frame) = RemoteFrame::from_value(value) else {
            log::debug!("数据面帧解析失败（可能是更高版本对端的未知变体），丢弃");
            return;
        };
        match frame {
            RemoteFrame::Resize { rows, cols } => {
                if let Some(mirror) = self.mirror.as_mut() {
                    mirror.resize(usize::from(rows).max(1), usize::from(cols).max(1));
                }
                // resize（列宽变）/ 被控端切窗格（绝对行号体系换源）→ 历史缓存按旧列宽
                // 序列化、绝对行号不再对应，必须复位回看与缓存，回到跟随实时。
                self.reset_history();
            }
            RemoteFrame::Output(bytes) => {
                if let Some(mirror) = self.mirror.as_mut() {
                    mirror.advance(&bytes);
                    // 镜像无 PTY，不回写应答（DSR/DA 等）；排空避免无界增长。
                    let _ = mirror.take_responses();
                }
            }
            RemoteFrame::Input(bytes) => {
                // 仅被控端会话期间接受（控制端不应收到 Input）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_input.push(bytes);
                }
            }
            RemoteFrame::ViewportResize { rows, cols } => {
                // 仅被控端接受：保留最新视口请求，main 把焦点窗格 resize 到它。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_viewport = Some((rows, cols));
                }
            }
            // part3d 历史按需分页：
            RemoteFrame::HistoryReq { top, count } => {
                // 仅被控端应答：入待处理队列，main 从焦点窗格 term 序列化对应行后回。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_history.push((top, count));
                }
            }
            RemoteFrame::HistoryRows {
                top,
                base,
                screen_top,
                lines,
            } => {
                // 仅控制端：刷新边界 + 把回带的行入缓存、销在途、提版本触发重建。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.hist_bounds = Some((base, screen_top));
                    for (i, bytes) in lines.into_iter().enumerate() {
                        let abs = top + i as u64;
                        self.hist_inflight.remove(&abs);
                        self.hist_cache.insert(abs, bytes);
                    }
                    self.hist_version = self.hist_version.wrapping_add(1);
                    self.trim_history_cache();
                }
            }
            RemoteFrame::HistoryBounds { base, screen_top } => {
                // 仅控制端：会话起始即知可滚范围（首次回看前）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.hist_bounds = Some((base, screen_top));
                }
            }
            RemoteFrame::Echo(_) => {}
        }
    }

    /// 处理一条后台事件。
    fn apply(&mut self, ev: WsEvent) {
        match ev {
            // 连接建立：presence 上线（后台线程已处理重连，主线程无需记状态）。
            WsEvent::Connected => {}
            WsEvent::Disconnected => {
                // 断线即丢弃进行中的配对/会话态（服务端侧亦已拆除）。
                self.pairing = None;
                self.incoming = None;
                self.mirror = None;
                self.pending_input.clear();
                self.pending_viewport = None;
                self.pending_history.clear();
                self.reset_history();
                if self.session.take().is_some() {
                    self.notices.push(Notice::SessionEnded(EndReason::PeerDisconnected));
                }
            }
            WsEvent::Server(msg) => self.apply_server(*msg),
        }
    }

    /// 处理一条服务端协议消息，推进配对/会话状态机。
    fn apply_server(&mut self, msg: RemoteS2C) {
        match msg {
            RemoteS2C::Welcome { .. } | RemoteS2C::Pong => {}
            RemoteS2C::ControlRequested {
                controller_name,
                pairing_code,
                ..
            } => {
                self.incoming = Some(IncomingControl {
                    controller_name,
                    pairing_code,
                });
            }
            RemoteS2C::PairingNeeded {
                target_device_id,
                target_name,
                ..
            } => {
                self.pairing = Some(PairingPrompt {
                    target_device_id,
                    target_name,
                    last_error: None,
                    attempts_left: None,
                });
            }
            RemoteS2C::PairingResult {
                reason,
                attempts_left,
            } => {
                if attempts_left == 0 {
                    self.pairing = None;
                    self.notices.push(Notice::PairingFailed(reason));
                } else if let Some(p) = &mut self.pairing {
                    p.last_error = Some(reason);
                    p.attempts_left = Some(attempts_left);
                }
            }
            RemoteS2C::ControlDenied { reason, .. } => {
                self.pairing = None;
                self.notices.push(Notice::ControlDenied(reason));
            }
            RemoteS2C::PairingCancelled { reason } => {
                self.incoming = None;
                self.notices.push(Notice::PairingCancelled(reason));
            }
            RemoteS2C::SessionStarted {
                peer_name, role, ..
            } => {
                self.pairing = None;
                self.incoming = None;
                let peer = peer_name.clone();
                // 控制端：起一个无 PTY 的镜像 Terminal（被控端会随即发 Resize+快照）。
                self.reset_history();
                self.mirror = (role == Role::Controller)
                    .then(|| Terminal::new(MIRROR_INIT_ROWS, MIRROR_INIT_COLS, MIRROR_SCROLLBACK));
                self.session = Some(ActiveSession { peer_name, role });
                self.notices.push(Notice::SessionStarted { role, peer });
            }
            RemoteS2C::SessionEnded { reason } => {
                self.session = None;
                self.mirror = None;
                self.pending_input.clear();
                self.pending_viewport = None;
                self.pending_history.clear();
                self.reset_history();
                self.notices.push(Notice::SessionEnded(reason));
            }
            // 数据面：part3a 镜像字节流 / part4 远程输入，按角色路由。
            RemoteS2C::Relay(value) => self.apply_relay(&value),
        }
    }
}

/// 唤醒主线程：标记 egui 重绘 + 发 `PtyWake` 唤醒 winit 循环（失焦也送达；共用
/// `wake_pending` 去重，避免高频输出时事件风暴）。
fn nudge(ctx: &egui::Context, proxy: &EventLoopProxy<PtyWake>, wake_pending: &Arc<AtomicBool>) {
    ctx.request_repaint();
    // SeqCst 与主线程清标志（main.rs user_event）配对，避免丢唤醒；swap RMW 恒读最新值。
    if !wake_pending.swap(true, Ordering::SeqCst) {
        let _ = proxy.send_event(PtyWake);
    }
}

/// 后台线程主体：连接 → 跑读写循环 → 断线退避重连，直到 `stop`。
fn worker(
    token: &str,
    cmd_rx: &Receiver<RemoteC2S>,
    evt_tx: &Sender<WsEvent>,
    stop: &Arc<AtomicBool>,
    ctx: &egui::Context,
    proxy: &EventLoopProxy<PtyWake>,
    wake_pending: &Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        match connect_ws(token) {
            Ok(mut socket) => {
                let _ = evt_tx.send(WsEvent::Connected);
                nudge(ctx, proxy, wake_pending);
                run_connection(&mut socket, cmd_rx, evt_tx, stop, ctx, proxy, wake_pending);
                let _ = evt_tx.send(WsEvent::Disconnected);
                nudge(ctx, proxy, wake_pending);
            }
            Err(e) => log::warn!("远程 WS 连接失败: {e}"),
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        sleep_with_stop(RECONNECT_DELAY, stop);
    }
}

/// 单条连接的读写循环：排空出站命令 + 周期 Ping + 带超时读消息。返回即断开。
fn run_connection(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    cmd_rx: &Receiver<RemoteC2S>,
    evt_tx: &Sender<WsEvent>,
    stop: &Arc<AtomicBool>,
    ctx: &egui::Context,
    proxy: &EventLoopProxy<PtyWake>,
    wake_pending: &Arc<AtomicBool>,
) {
    let mut last_ping = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            return;
        }
        // 1. 排空 UI 出站命令。
        loop {
            match cmd_rx.try_recv() {
                Ok(msg) => {
                    if !write_msg(socket, &msg) {
                        return; // 写失败=断开
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return, // 主线程已 stop
            }
        }
        // 2. 周期保活。
        if last_ping.elapsed() >= PING_INTERVAL {
            if !write_msg(socket, &RemoteC2S::Ping) {
                return;
            }
            last_ping = Instant::now();
        }
        // 3. 带超时读一条消息。
        match socket.read() {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<RemoteS2C>(text.as_str()) {
                    Ok(msg) => {
                        let _ = evt_tx.send(WsEvent::Server(Box::new(msg)));
                        nudge(ctx, proxy, wake_pending);
                    }
                    Err(e) => log::debug!("远程 WS 消息解析失败: {e}"),
                }
            }
            Ok(Message::Close(_)) => return,
            // 二进制 / Ping / Pong / 原始帧：part1 不使用，忽略。
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // 读超时：本轮无消息，继续循环（处理出站/保活/停止）。
            }
            Err(e) => {
                log::debug!("远程 WS 读断开: {e}");
                return;
            }
        }
    }
}

/// 序列化并发送一条出站消息；成功返回 `true`，写失败（断开）返回 `false`。
fn write_msg(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, msg: &RemoteC2S) -> bool {
    let Ok(text) = serde_json::to_string(msg) else {
        log::error!("远程 WS 出站消息序列化失败");
        return true; // 序列化错误不该断连接，丢弃该条
    };
    socket.send(Message::Text(text.into())).is_ok()
}

/// 建立到 `lumen-server` 的 WS 连接（带 `Authorization: Bearer` 头），并对底层
/// `TcpStream` 设读超时。
fn connect_ws(token: &str) -> anyhow::Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let url = ws_url(&server_url());
    let uri: tungstenite::http::Uri = url.parse()?;
    let req = ClientRequestBuilder::new(uri).with_header("Authorization", format!("Bearer {token}"));
    let (mut socket, _resp) = tungstenite::connect(req)?;
    set_read_timeout(socket.get_mut(), Some(READ_TIMEOUT));
    Ok(socket)
}

/// 把 HTTP(S) 基址转成 WS(S) URL 并拼上远程控制路径。
fn ws_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let scheme_swapped = if let Some(rest) = b.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = b.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{b}")
    };
    format!("{scheme_swapped}{}", lumen_protocol::routes::WS)
}

/// 给底层 `TcpStream` 设读超时（明文 / rustls 两种流均覆盖）。
fn set_read_timeout(stream: &mut MaybeTlsStream<TcpStream>, dur: Option<Duration>) {
    match stream {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(dur);
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(dur);
        }
        // MaybeTlsStream 是 #[non_exhaustive]：未启用的 TLS 后端等忽略。
        _ => {}
    }
}

/// 可被 `stop` 提前打断的睡眠（重连退避用，避免登出后还干等）。
fn sleep_with_stop(total: Duration, stop: &Arc<AtomicBool>) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_转换() {
        assert_eq!(ws_url("http://127.0.0.1:8787"), "ws://127.0.0.1:8787/api/v1/ws");
        assert_eq!(ws_url("https://lumen.example.com"), "wss://lumen.example.com/api/v1/ws");
        // 缺协议默认 ws://；去尾斜杠。
        assert_eq!(ws_url("192.168.1.85:8787/"), "ws://192.168.1.85:8787/api/v1/ws");
    }

    #[test]
    fn 默认态与停止() {
        let mut ws = RemoteWs::default();
        assert!(!ws.is_running());
        assert!(ws.pairing.is_none() && ws.incoming.is_none() && ws.session.is_none());
        // stop 在未启动时应安全（幂等）。
        ws.stop();
        assert!(!ws.is_running());
    }

    #[test]
    fn 配对结果推进状态机() {
        let mut ws = RemoteWs::default();
        // 模拟收到 PairingNeeded → 进入待配对态。
        ws.apply_server(RemoteS2C::PairingNeeded {
            target_device_id: "t".into(),
            target_name: "被控机".into(),
            expires_in_secs: 120,
        });
        assert!(ws.pairing.is_some());
        // 错码：剩余次数下降、记录错误，仍保留待配对。
        ws.apply_server(RemoteS2C::PairingResult {
            reason: PairingFailReason::InvalidCode,
            attempts_left: 4,
        });
        let p = ws.pairing.as_ref().expect("仍待配对");
        assert_eq!(p.attempts_left, Some(4));
        assert!(matches!(p.last_error, Some(PairingFailReason::InvalidCode)));
        // 归零：配对作废 + 通知。
        ws.apply_server(RemoteS2C::PairingResult {
            reason: PairingFailReason::TooManyAttempts,
            attempts_left: 0,
        });
        assert!(ws.pairing.is_none());
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::PairingFailed(PairingFailReason::TooManyAttempts)]
        ));
    }

    #[test]
    fn 会话建立与结束() {
        let mut ws = RemoteWs::default();
        ws.apply_server(RemoteS2C::SessionStarted {
            peer_device_id: "p".into(),
            peer_name: "对端".into(),
            role: Role::Controller,
        });
        let s = ws.session.as_ref().expect("会话已建立");
        assert_eq!(s.role, Role::Controller);
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::SessionStarted { role: Role::Controller, .. }]
        ));
        ws.apply_server(RemoteS2C::SessionEnded {
            reason: EndReason::PeerLeft,
        });
        assert!(ws.session.is_none());
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::SessionEnded(EndReason::PeerLeft)]
        ));
    }

    /// part3d：把 [`RemoteFrame`] 经 Relay 通路喂给状态机（模拟收到对端数据面帧）。
    fn relay(ws: &mut RemoteWs, frame: &RemoteFrame) {
        let v = frame.to_value().expect("帧转 value");
        ws.apply_relay(&v);
    }

    fn 起会话(ws: &mut RemoteWs, role: Role) {
        ws.apply_server(RemoteS2C::SessionStarted {
            peer_device_id: "p".into(),
            peer_name: "对端".into(),
            role,
        });
        let _ = ws.take_notices();
    }

    #[test]
    fn 回看锚定与跟随() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller); // 起镜像
        // 被控端会话起始发的历史边界：100 行可视区首行，base=0（100 行历史）。
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        // 跟随态上滚 3 行 → 进回看，绝对首行 = 97。
        assert!(ws.scroll_mirror(3));
        assert_eq!(ws.hist_top, Some(97));
        // 继续上滚 2 → 95。
        assert!(ws.scroll_mirror(2));
        assert_eq!(ws.hist_top, Some(95));
        // 下滚越底 → 回跟随实时。
        assert!(ws.scroll_mirror(-10));
        assert_eq!(ws.hist_top, None);
        // 上滚不能越过最旧行（base=0）：一次性滚很多仍夹在 0。
        assert!(ws.scroll_mirror(9999));
        assert_eq!(ws.hist_top, Some(0));
    }

    #[test]
    fn 无历史不可回看() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        // base == screen_top：被控端无 scrollback。
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 50, screen_top: 50 });
        assert!(!ws.scroll_mirror(3));
        assert_eq!(ws.hist_top, None);
        // 边界未知时滚动也忽略。
        let mut ws2 = RemoteWs::default();
        起会话(&mut ws2, Role::Controller);
        assert!(!ws2.scroll_mirror(3));
    }

    #[test]
    fn 历史行入缓存() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(
            &mut ws,
            &RemoteFrame::HistoryRows {
                top: 10,
                base: 0,
                screen_top: 100,
                lines: vec![b"a".to_vec(), Vec::new(), b"c".to_vec()],
            },
        );
        assert_eq!(ws.hist_bounds, Some((0, 100)));
        assert_eq!(ws.hist_cache.get(&10).map(Vec::as_slice), Some(b"a".as_slice()));
        assert_eq!(ws.hist_cache.get(&11).map(Vec::as_slice), Some(b"".as_slice()));
        assert_eq!(ws.hist_cache.get(&12).map(Vec::as_slice), Some(b"c".as_slice()));
        assert!(ws.hist_version >= 1, "写入历史行应提版本");
    }

    #[test]
    fn 回看锚点用最新边界() {
        // 回归 Bug B：实时输出推进后被控端重发的最新边界须被采纳，否则首次上滚跳到旧屏位。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        // 被控端实时输出推进后重发的最新边界。
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 600 });
        // 跟随态首次上滚 3 → 锚到最新屏(600)上方 597，而非陈旧值(100)算出的 97。
        assert!(ws.scroll_mirror(3));
        assert_eq!(ws.hist_top, Some(597));
    }

    #[test]
    fn 历史应答全量销在途() {
        // 回归 Bug A：应答覆盖的请求段须整段销 inflight（两端 count 上限对齐保证
        // lines.len()==请求 count），否则残留行永不重拉、回看永久空白。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        for a in 10..13 {
            ws.hist_inflight.insert(a); // 模拟已请求 [10,13) 在途
        }
        relay(
            &mut ws,
            &RemoteFrame::HistoryRows {
                top: 10,
                base: 0,
                screen_top: 100,
                lines: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            },
        );
        assert!(ws.hist_inflight.is_empty(), "应答覆盖的在途行应全部销账");
        assert_eq!(ws.hist_cache.len(), 3);
    }

    #[test]
    fn 镜像选区取文本() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::Resize { rows: 3, cols: 20 });
        relay(&mut ws, &RemoteFrame::Output(b"hello world\r\nsecond line".to_vec()));
        // 跟随态选区：第 1 行 col0..col4 = "hello"。
        ws.mirror_sel_start(0, 0);
        assert!(ws.mirror_selecting());
        ws.mirror_sel_update(0, 4);
        ws.mirror_sel_end();
        assert!(!ws.mirror_selecting());
        assert_eq!(ws.copy_mirror_selection().as_deref(), Some("hello"));
        // 清空后无文本。
        ws.clear_mirror_selection();
        assert!(ws.copy_mirror_selection().is_none());
    }

    #[test]
    fn 有镜像选区判定() {
        // 回归 #1：keymap 据 has_mirror_selection 决定 Ctrl+C 复制 vs 转发中断。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::Output(b"hello".to_vec()));
        assert!(!ws.has_mirror_selection(), "无选区");
        ws.mirror_sel_start(0, 0);
        ws.mirror_sel_update(0, 3);
        assert!(ws.has_mirror_selection(), "拖出非空选区");
        ws.clear_mirror_selection();
        assert!(!ws.has_mirror_selection(), "清空后无选区");
    }

    #[test]
    fn 转发输入回跟随底部() {
        // part4c：回看态转发输入（打字/中文/粘贴）即 snap 回跟随，使用户看到回显。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        ws.scroll_mirror(5);
        assert_eq!(ws.hist_top, Some(95), "已进回看态");
        ws.send_input(b"x");
        assert_eq!(ws.hist_top, None, "转发输入后回跟随实时底部");
    }

    #[test]
    fn 镜像光标跟随态有回看态无() {
        // part4c：IME 候选框只在跟随态定位到镜像光标，回看态不定位。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::Output(b"abc".to_vec()));
        assert!(ws.mirror_cursor().is_some(), "跟随态有镜像光标");
        assert!(!ws.mirror_win32_input(), "默认非 win32 输入模式");
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        ws.scroll_mirror(5);
        assert!(ws.mirror_cursor().is_none(), "回看态不返回光标");
    }

    #[test]
    fn 镜像点击不拖动不留选区() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::Output(b"abc".to_vec()));
        ws.mirror_sel_start(0, 2);
        ws.mirror_sel_end(); // 未拖动 = 空选区
        assert!(ws.copy_mirror_selection().is_none());
    }

    #[test]
    fn 被控端收历史请求入队() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controlled);
        relay(&mut ws, &RemoteFrame::HistoryReq { top: 5, count: 3 });
        assert_eq!(ws.take_history_reqs(), vec![(5, 3)]);
        // 取走后清空。
        assert!(ws.take_history_reqs().is_empty());
        // 控制端角色不应入队历史请求（HistoryReq 仅被控端处理）。
        let mut cc = RemoteWs::default();
        起会话(&mut cc, Role::Controller);
        relay(&mut cc, &RemoteFrame::HistoryReq { top: 1, count: 1 });
        assert!(cc.take_history_reqs().is_empty());
    }

    #[test]
    fn resize_复位回看() {
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        relay(
            &mut ws,
            &RemoteFrame::HistoryRows {
                top: 10,
                base: 0,
                screen_top: 100,
                lines: vec![b"x".to_vec()],
            },
        );
        assert!(ws.scroll_mirror(5));
        assert_eq!(ws.hist_top, Some(95));
        // 收到 Resize（列宽变 / 切窗格）：历史缓存按旧体系失效，复位回跟随。
        relay(&mut ws, &RemoteFrame::Resize { rows: 30, cols: 100 });
        assert_eq!(ws.hist_top, None);
        assert!(ws.hist_bounds.is_none());
        assert!(ws.hist_cache.is_empty());
    }

    #[test]
    fn 来件控制与断线清理() {
        let mut ws = RemoteWs::default();
        ws.apply_server(RemoteS2C::ControlRequested {
            controller_device_id: "c".into(),
            controller_name: "控制机".into(),
            pairing_code: "123456789".into(),
            expires_in_secs: 120,
        });
        assert_eq!(
            ws.incoming.as_ref().map(|i| i.pairing_code.clone()),
            Some("123456789".to_string())
        );
        // 断线：来件/会话态清掉。
        ws.apply(WsEvent::Disconnected);
        assert!(ws.incoming.is_none());
    }
}
