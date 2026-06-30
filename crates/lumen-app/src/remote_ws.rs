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
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use lumen_protocol::remote::{
    DenyReason, DirEntry, EndReason, FsErr, PairingFailReason, PaneOpKind,
    PaneSnapshot, PaneViewport, PutConflict, PutOverwrite, PutStatus, RecursiveDirEntry, RemoteC2S,
    RemoteFrame, RemoteOpErr, RemoteS2C, Role, SessionId, TabId, TabState, FETCH_MAX_LEN,
    FETCH_WINDOW, FILE_CHUNK, LIST_DIR_RECURSIVE_MAX_DEPTH, LIST_DIR_RECURSIVE_MAX_ENTRIES,
};
use lumen_term::{SelPoint, Selection, Terminal};

use crate::shell::layout::PaneLayout;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{ClientRequestBuilder, Message, WebSocket};
use winit::event_loop::EventLoopProxy;

use crate::cloud::server_url;
use crate::p2p::{P2pEngine, P2pEvent, SignalPayload};
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
/// WS 读超时。**也是出站延迟上限**：`run_connection` 单线程循环「排空出站命令 → 阻塞读最长本超时」，
/// 读阻塞期间无法发出站，故键入的输入帧最多延迟本超时才发出（两端各一次，往返双倍）。100ms 时打字
/// 严重卡顿（海风哥实测）；降到 5ms：发送延迟 ≤5ms（低于 8ms 合帧预算），空闲时仍睡在 read 系统调用、
/// CPU 可忽略。要彻底零延迟需读写分线程（tungstenite 单 socket 不便拆，5ms 轮询是务实解）。
const READ_TIMEOUT: Duration = Duration::from_millis(5);
/// 被控端读目录服务常驻 worker 线程数（review MED-1）：固定小池替代「每请求起一线程」，慢盘
/// 并发读不互相阻塞，又封顶线程数。Fetch/Put 大文件另有滑动窗口背压，不走此池。
const DIR_SERVICE_WORKERS: usize = 4;
/// 读目录任务有界队列容量：满即拒绝并回 `FsErr::Io`（控制端不空挂）。正常负载下控制端
/// `has_listing`/`is_pending` 闸使每目录至多一次在途，远不及此；上限仅防慢盘大量展开堆积。
const DIR_SERVICE_QUEUE_CAP: usize = 64;
/// 应用层 Ping 周期（保活 + 刷新服务端 `last_seen`）。
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// 断线后重连退避。
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// part3c-2 控制端在途 Fetch 的停滞超时：超过这么久没收到新块即判传输中断、清临时文件
/// （正常传输每收一块刷新计时；仅防对端静默卡死，非总时长上限）。
const FETCH_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// part3c-2 #7 下载并发文件数上限（递归下载时同时在途的文件 Fetch 数；防一次性起几百个）。
const DOWNLOAD_MAX_FILES: usize = 4;
/// part3c-2 #7 下载并发目录列举上限（同时在途的 ListDir 数；防深 / 宽树洪泛被控端线程）。
const DOWNLOAD_MAX_LISTDIR: usize = 6;
/// part3c-2 #7/上传 递归深度上限（防 junction / symlink 成环；超深的子树跳过）。
const TRANSFER_MAX_DEPTH: usize = 64;

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

/// part3d Phase 3 尺寸同步：订阅会话各窗格的目标尺寸列表 `(session_id, rows, cols)`。
type PaneSizes = Vec<(SessionId, u16, u16)>;
/// 一次尺寸同步请求：`(tab_id, 各窗格目标尺寸)`。
type SubViewportReq = (TabId, PaneSizes);
/// part3d Phase 3 布局比例同步（[`RemoteFrame::SubLayout`]）：`(tab_id, 行权重, 各排列权重)`。
type SubLayoutData = (TabId, Vec<f32>, Vec<Vec<f32>>);

/// 两组布局权重是否近似相等（变更检测/回声免疫用；浮点按 `1e-4` 容差，避免归一化微抖动刷帧）。
fn weights_approx_eq(a: &SubLayoutData, b: &SubLayoutData) -> bool {
    const EPS: f32 = 1e-4;
    let near = |x: f32, y: f32| (x - y).abs() <= EPS;
    a.0 == b.0
        && a.1.len() == b.1.len()
        && a.1.iter().zip(&b.1).all(|(x, y)| near(*x, *y))
        && a.2.len() == b.2.len()
        && a.2
            .iter()
            .zip(&b.2)
            .all(|(xc, yc)| xc.len() == yc.len() && xc.iter().zip(yc).all(|(x, y)| near(*x, *y)))
}

/// part3d Phase 3c 控制端多窗格镜像的单个窗格：被控端窗格 id + 无 PTY 的镜像 `Terminal`
/// （喂入该窗格转发来的 [`RemoteFrame::OutputWithId`] 复现内容）。仅订阅会话 >1 窗格时存在
/// （1 窗格走 [`RemoteWs::mirror`] 单 mirror，保 part3a/b 的回看 + 选区）。渲染顺序 = 在
/// [`RemoteWs::mirror_panes`] 中的下标（复刻被控端 panes 顺序）。
pub struct MirrorPane {
    /// 被控端窗格 id（[`RemoteFrame::OutputWithId`] 路由键）。
    pub session_id: SessionId,
    /// 镜像终端（无 PTY，控制端主题就地解析颜色）。
    pub term: Terminal,
    /// 控制端 part4b per-pane 选区（拖选/复制；与 `term` 同坐标系，渲染器据此画高亮）。`None`=无选区。
    pub selection: Option<Selection>,
    /// 该窗格回看边界初值 `(base, screen_top)`（被控端绝对行号，来自 `SubscriptionStarted` 快照）。
    /// 切焦点到本窗格时据此设 `hist_bounds` 起步回看，随后由 `HistoryRowsForPane` 应答刷新精确值。
    pub hist_base: u64,
    pub hist_screen_top: u64,
}

/// part3d Phase 3c 控制端多窗格镜像的结构（比例布局 + 最大化）。**比例双向同步、但焦点不同步**：
/// 任一端拖分隔条改比例都经 [`RemoteFrame::SubLayout`] 同步给对端（不分前后台），控制端按
/// `layout` 画 `pane_rects` / 分隔条。`focused` 仍忽略（控制端不跟随被控端高亮焦点格；控制端
/// 自己的窗格选中留待 Phase 4）。初始 `layout` 复刻被控端 `row_weights`/`col_weights`，之后的
/// 同步走 `SubLayout`（回声免疫见 [`RemoteWs`] 的 `sub_layout_baseline`），不再读 `SubscriptionStarted`
/// 的权重做增量采纳（那条路对连续拖动有回声打架）。
pub struct MirrorLayout {
    /// 控制端镜像窗格比例布局（初始复刻被控端权重；之后控制端拖动或被控端经 `SubLayout` 更新）。
    pub layout: PaneLayout,
    /// 最大化窗格下标（`Some` 时独占、其余隐藏）。
    pub maximized: Option<u32>,
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
    /// part3c-2：开始从被控端获取文件（双击远程文件，传输中）。
    FetchStarted,
    /// part3c-2：获取文件失败（读失败 / 过大 / 传输中断）。
    FetchFailed(FsErr),
    /// part3c-2 #7：开始下载（复制远程项粘贴到本地，传输中）。
    DownloadStarted,
    /// part3c-2 #7：下载结束汇总（完成 / 跳过 / 出错文件数）。
    DownloadDone {
        /// 成功落地文件数。
        done: usize,
        /// 因撞名跳过数。
        skipped: usize,
        /// 出错数。
        errors: usize,
    },
    /// part3c-2 片5：开始上传（复制本地项粘贴到被控端目录，传输中）。
    UploadStarted,
    /// part3c-2 片5：上传结束汇总（完成 / 跳过 / 出错文件数）。
    UploadDone {
        /// 成功写入被控端的文件数。
        done: usize,
        /// 因撞名跳过数。
        skipped: usize,
        /// 出错数。
        errors: usize,
    },
    /// 片8：远程目录递归枚举完成、虚拟文件 descriptor 已就绪（可粘贴到资源管理器）。
    ClipDirReady {
        /// 子树项数（文件 + 目录）。
        count: usize,
        /// 是否因超上限截断（仅复制了前 N 项）。
        truncated: bool,
    },
    /// 片8：远程目录递归枚举失败（权限 / 路径不存在 / 空目录 / 会话断）。
    ClipDirFailed,
    /// part3d Phase 2：远程新建会话失败（超 [`REMOTE_MAX_SESSIONS`](lumen_protocol::remote::REMOTE_MAX_SESSIONS) 等）。
    RemoteNewTabFailed(RemoteOpErr),
    /// part3d Phase 2：远程关闭会话失败（拒关被控端最后一个 / 目标不存在等）。
    RemoteCloseTabFailed(RemoteOpErr),
    /// M6 Phase 3：数据面已切到 P2P 直连（绕开中继，更低延迟）。
    P2pDirect,
    /// M6 Phase 3：数据面已回退到中继转发（直连断开 / idle 超时）。
    P2pRelay,
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
    /// 被控端：待应答的历史行请求 `(目标窗格, top, count)`。`None` = 单窗格镜像的焦点窗格（旧
    /// [`RemoteFrame::HistoryReq`]，回 [`RemoteFrame::HistoryRows`]）；`Some(sid)` = 多窗格指定窗格
    /// （[`RemoteFrame::HistoryReqForPane`]，回 [`RemoteFrame::HistoryRowsForPane`]）。
    pending_history: Vec<(Option<SessionId>, u64, u16)>,
    // ── part3d 多会话 × 多窗格镜像（Phase 1 MVP：列表 + 订阅，单 mirror 只读）──────────
    /// 控制端：被控端推来的会话(tab)列表 + 概览状态（远程视图侧栏渲染源；来源
    /// [`RemoteFrame::TabListSnapshot`] / `TabCreated` / `TabClosed` / `TabUpdated`）。
    remote_tabs: Vec<TabState>,
    /// 控制端：当前订阅查看的会话 id（点击远程列表项切换；`None` = 未订阅）。镜像内容
    /// 来自该会话（被控端按订阅推，被控端焦点不动）。K5：同一时刻只订阅 1 个。
    subscribed_tab: Option<TabId>,
    /// 控制端：订阅会话**焦点窗格**的 session_id（来源 `SubscriptionStarted.panes[focused]`）。
    /// **单窗格镜像**（`mirror_panes` 空）时按此 id 过滤 `OutputWithId`/`ResizeWithId`，只喂
    /// 焦点窗格那一路。多窗格走 `mirror_panes` per-pane 路由。
    mirror_focus_sid: Option<SessionId>,
    /// 控制端：part3d Phase 3c 多窗格镜像——订阅会话 >1 窗格时按渲染序存各窗格镜像 Terminal
    /// （`OutputWithId` 按 session_id 路由）。空 = 单窗格模式（走 `mirror`）。
    mirror_panes: Vec<MirrorPane>,
    /// 控制端：多窗格镜像布局（复刻被控端 `PaneLayout`+焦点+最大化；`mirror_panes` 非空时有效）。
    mirror_layout: Option<MirrorLayout>,
    /// 控制端：part3d Phase 4 多窗格镜像里**控制端自选的焦点窗格** session_id（与被控端焦点解耦，
    /// 需求 e）。驱动：输入转发目标（[`RemoteFrame::InputWithId`]）、per-pane 回看目标、IME 光标源、
    /// 复制源、渲染高亮。**用 SessionId 非下标**（关窗格 Vec 重排，D1）。订阅/结构变时初始化为
    /// 被控端焦点窗格、之后由控制端点击改；单窗格镜像（`mirror_panes` 空）不用此字段（走 `mirror`）。
    mirror_active_pane: Option<SessionId>,
    /// 控制端：part4b 多窗格当前正在拖选的窗格 session_id（左键按下到松开；保拖动跨帧操作同一窗格、
    /// 中途滑出不换格）。`None`=未拖选。
    mirror_pane_selecting: Option<SessionId>,
    /// 控制端：上次发给被控端的订阅会话各窗格目标尺寸（Phase 3 尺寸同步去重；变化才发
    /// [`RemoteFrame::SubViewport`]）。
    last_sub_viewport: Option<SubViewportReq>,
    /// 被控端：控制端请求的订阅会话各窗格目标尺寸（来源 [`RemoteFrame::SubViewport`]）；main 取走后
    /// 在该会话为**后台 tab** 时 resize 其窗格（所有权规则：前台由被控端窗口接管）。
    pending_sub_viewport: Option<SubViewportReq>,
    /// 任一端：收到的对端 [`RemoteFrame::SubLayout`]（订阅会话窗格比例）。main 取走后应用到本端布局
    /// （控制端→镜像布局；被控端→该 tab 布局，前后台均应用），并更新 `sub_layout_baseline` 免回声。
    pending_sub_layout: Option<SubLayoutData>,
    /// 任一端：布局比例同步的「已发/已应用基线」（回声免疫核心）。`send_sub_layout_if_changed` 仅当
    /// 本端当前比例与此基线**不近似相等**才发 [`RemoteFrame::SubLayout`]；收到对端 SubLayout 后亦把基线
    /// 更新为该比例——故应用对端比例不会被本端变更检测当成本地改动回发，连续拖动两向皆无回声打架。
    /// 订阅目标/结构变化时复位为 `None`（首帧据当前比例建立同步）。
    sub_layout_baseline: Option<SubLayoutData>,
    /// 被控端：控制端订阅查看的会话 id（来源 [`RemoteFrame::SubscribeSession`]）。被控端据此
    /// 把该会话焦点窗格快照 + 实时输出推给控制端，**与被控端自身焦点解耦**（需求 c/e）。
    sub_target: Option<TabId>,
    /// 被控端：订阅目标本帧刚变化（收到 `SubscribeSession`，含重订同一会话）——main 取走后
    /// 复位 `mirror_src` 强制重发 `SubscriptionStarted`（否则重订同一会话因窗格 key 未变不重发、
    /// 控制端镜像空白）。
    sub_dirty: bool,
    /// 被控端：上次推给控制端的会话列表（K6 去重基线；归一化后与本帧比对，变化才发
    /// `TabListSnapshot`，免布局浮点 / spinner 标题每帧抖动刷爆链路）。
    last_tab_states: Vec<TabState>,
    /// 被控端：待执行的远程新建会话请求 `req_id`（来源 [`RemoteFrame::NewTab`]；main 取走后
    /// spawn 新 tab 并回 [`RemoteFrame::NewTabResult`]，Phase 2 需求 d）。
    pending_new_tab: Vec<u64>,
    /// 被控端：待执行的远程关闭会话请求 `(req_id, tab_id)`（来源 [`RemoteFrame::CloseTab`]；
    /// main 取走后关 tab 并回 [`RemoteFrame::CloseTabResult`]）。
    pending_close_tab: Vec<(u64, TabId)>,
    /// M5.3 part4b 控制端镜像选区（作用于**当前显示**的镜像终端：跟随=mirror，回看=
    /// hist_term；按绝对行号定位，与渲染/取文本同坐标系）。右键有选区→复制本地剪贴板。
    mirror_selection: Option<Selection>,
    /// 控制端：镜像选区拖动中（左键按下到松开）。
    mirror_selecting: bool,
    /// M5.3 part3c-2 控制端：Option B 远程树状态（按需浏览被控端文件系统；自有展开态 +
    /// 目录缓存 + 在途 ListDir）。
    remote_filetree: Option<RemoteFileTree>,
    /// 被控端：已推给控制端的当前 cwd（去重 [`RemoteFrame::RootChanged`]，cwd 变才重发）。
    remote_root_sent: Option<String>,
    /// 控制端：请求号分配器（单调递增，跳 0 哨兵；ListDir/Fetch/Put 全局共用）。
    req_seq: u64,
    /// 被控端：控制端发来的待处理 ListDir 请求 `(req_id, path, show_hidden)`，main 取走后
    /// 后台读盘（[`Self::spawn_list_dir`]）。
    pending_listdir: Vec<(u64, String, bool)>,
    /// 被控端：控制端发来的待处理 ListDirRecursive 请求（片8 目录递归；main 取走后台递归读盘）。
    pending_listdir_recursive: Vec<(u64, String, bool)>,
    /// 控制端：在途的「复制远程目录」递归枚举 `(req_id, 根目录显示名)`（片8）。req_id 用于
    /// ListDirRecursiveResult 去重（陈旧 / 被新复制动作作废的应答丢弃）；根目录名作为 descriptor
    /// 的顶层前缀，使粘贴出「目标\根目录名\…」（含顶层文件夹本身）而非把内容散落到目标根。
    inflight_clip_dir: Option<(u64, String)>,
    /// 控制端：递归枚举好、待 main 取走构造虚拟文件的子树清单（片8；main `take_clip_dir_ready`
    /// → `clipboard_svc.set_remote_dir`）。
    clip_dir_ready: Option<Vec<RecursiveDirEntry>>,
    /// 被控端：文件服务后台线程 → 主线程的回包发送端（[`Self::start`] 建，worker 克隆）。
    svc_tx: Option<Sender<SvcReply>>,
    /// 被控端：文件服务回包接收端（main 在 `pump_remote` 经 [`Self::drain_service`] 排空发回）。
    svc_rx: Option<Receiver<SvcReply>>,
    /// 被控端：读目录服务有界队列入口（[`Self::start`] 建，常驻 worker 池消费；`stop` 置 `None`
    /// 即 drop sender → 队列关闭、worker 退出）。替代旧「每请求起一线程」（review MED-1）。
    dir_job_tx: Option<SyncSender<DirJob>>,
    /// 控制端：在途 Fetch（接收方）：req_id → 落本地文件的任务（#5 打开 / #7 下载）。
    inflight_fetch: HashMap<u64, FetchJob>,
    /// 被控端：在途 Fetch 源（发送方）：req_id → 给 worker 发「可再发一块」许可的句柄
    /// （每收一个 [`RemoteFrame::FileChunkAck`] +1，滑动窗口背压）。
    inflight_fetch_src: HashMap<u64, FetchSrcJob>,
    /// 跨本地 / 远程的文件剪贴板（复制记引用、粘贴触发传输）。会话结束清 `Remote` 侧。
    file_clipboard: Option<FileClipboard>,
    /// 控制端：进行中的 #7 下载编排（远程 → 本地递归传输）；同一时刻一个。
    download: Option<DownloadWalk>,
    /// 被控端：在途 Put 目标（接收方）：req_id → 落被控端临时文件的写盘任务（片5 上传）。
    inflight_put: HashMap<u64, PutDstJob>,
    /// 控制端：在途 Put 源（发送方）：req_id → 给 worker 发「可再发一块」许可的句柄
    /// （每收一个 [`RemoteFrame::PutChunkAck`] +1，滑动窗口背压；对称于 `inflight_fetch_src`）。
    inflight_put_src: HashMap<u64, PutSrcJob>,
    /// 控制端：在途 Put 的元信息：req_id → (本地源路径, 被控端目标目录, 落地名)，供 `PutReady`
    /// 决策（开始发送 / 重发 Force / 暂停等覆盖）与统计。
    inflight_put_meta: HashMap<u64, PutMeta>,
    /// 控制端：进行中的片5 上传编排（本地 → 被控端递归传输）；同一时刻一个。
    upload: Option<UploadWalk>,
    /// 控制端：菜单触发的远程单次文件操作（新建文件夹/文件、删除）在途映射：req_id → **操作完成后
    /// 要刷新的远程目录**（新建项的父目录 / 删除项的父目录）。回 `MkDir/MkFile/DeleteResult` 时据此
    /// 刷新该目录让变更立即反映。区别于上传遍历的 `inflight_mkdir`（那是 UploadWalk 内部）。
    inflight_remote_fsop: HashMap<u64, String>,
    /// M5.3 part4 被控端待执行的远程输入 `(tab_id, session_id, 字节)`（控制端 [`RemoteFrame::InputWithId`]
    /// 转发来）：main 每帧取走、按双 id 查目标窗格、经 **per-pane**「本地输入优先」仲裁后写入该窗格 PTY。
    pending_input: Vec<(TabId, SessionId, Vec<u8>)>,
    /// M5.3 part3d Phase 4 需求②：被控端待执行的远程窗格操作 `(tab_id, session_id, op)`（控制端
    /// [`RemoteFrame::PaneOp`] 发来）：main 取走后按双 id 查窗格执行关闭/最大化/换位，布局变化经
    /// `SubscriptionStarted` 重发同步回控制端。
    pending_pane_ops: Vec<(TabId, SessionId, PaneOpKind)>,
    /// M5.3 被控端待应用的远程视口尺寸（控制端请求；SSH 式跟随）：main 取走后
    /// 把焦点窗格 resize 到此 (rows, cols)。仅保留最新值。
    pending_viewport: Option<(u16, u16)>,
    /// 待消费的一次性通知（main 取走弹 toast）。
    notices: Vec<Notice>,
    /// M6 P2P 直连引擎（会话期存在；QUIC 打洞 + mTLS 握手 + 数据面收发）。Phase 3：数据面就绪后
    /// `send_frame` 选路到 QUIC 直连、入站经 `P2pEvent::DataFrame` 汇回 `apply_relay`；中继全程在线
    /// 作信令通道 + 兜底。经 `reset_multi_session` 统一拆除（会话结束 / 断连）。
    p2p: Option<P2pEngine>,
    /// 任一端：数据面是否已处于直连态（去重 `DataPlaneUp`/`Down` 事件，避免重复 toast / 重订阅风暴）。
    /// 控制端据此重订阅重建镜像、被控端据此把输出切到 QUIC；两端各自维护。
    p2p_data_active: bool,
    /// UI → 后台 出站命令发送端。
    cmd_tx: Option<Sender<RemoteC2S>>,
    /// 后台 → UI 事件接收端。
    evt_rx: Option<Receiver<WsEvent>>,
    /// 停止标志（登出 / Drop 时置位）。
    stop: Option<Arc<AtomicBool>>,
    /// 唤醒主线程用（被控端文件服务 worker 读盘完成后 nudge——否则空闲被控端的 ListDirResult
    /// 卡到下个偶发事件才发回，控制端目录加载慢数秒）。`start` 时存，与 WS 线程共用同套 wake。
    ctx: Option<egui::Context>,
    proxy: Option<EventLoopProxy<PtyWake>>,
    wake_pending: Option<Arc<AtomicBool>>,
}

/// M5.3 part3c-2 控制端 Option B 远程树：**按需浏览被控端文件系统**的状态机。
///
/// 替代 part3c-1 的 Option A（被控端推「可见行 + 展开态」快照）。控制端自持展开态
/// （`open`，**不与被控端同步** → 修 #6）+ 按被控端不透明路径缓存目录 `listings`
/// （展开未缓存目录即发 [`RemoteFrame::ListDir`]、被控端直接读盘回 → 修 #2/#3/#4）。
/// 节点 id = `nodes` 下标，根恒为 id 0 且默认展开。换根（[`RemoteFrame::RootChanged`]）
/// 整体重置（清空 `nodes`/`listings`/`open`/`pending`）。乱序 / 陈旧 / 换根后到达的
/// [`RemoteFrame::ListDirResult`] 被三重丢弃：① `reset_to_root` 清空 `pending` →
/// `apply_dir_entries` 的 `pending.get(dir_id)==Some(req_id)` 双键不命中；② `req_seq`
/// 进程级单调（从不在 start/stop/clear 重置）使换根前后 `req_id` 永不相等；③ `find_dir`
/// 只沿 `listings` 可达节点匹配路径。渲染**只读**（[`Self::visible_rows`]），点击经
/// `RemoteFileTreeOutput` 闭包后由 [`RemoteWs`] 以 `&mut` 施加——规避 `ShellInput` 的
/// `&remote_ws` 借用与渲染 `&mut` 的冲突。
#[derive(Default)]
pub struct RemoteFileTree {
    /// 被控端焦点窗格 cwd（树根；来源 [`RemoteFrame::RootChanged`]）。`None` = 等待 cwd。
    root: Option<String>,
    /// 节点表（append-only，换根重建；id = 下标，根恒为 0）。
    nodes: Vec<RemoteNode>,
    /// 已读目录的子项缓存：dir id → listing（折叠后保留，再展开秒开）。
    listings: HashMap<usize, RemoteDirListing>,
    /// 控制端自持展开态（dir id 集合；根 id 0 初始即在内）。
    open: HashSet<usize>,
    /// 在途 ListDir：dir id → req_id（双键校验，丢弃陈旧 / 乱序应答）。
    pending: HashMap<usize, u64>,
    /// 远程「显示隐藏项」开关（工具条勾选；经 `ListDir.show_hidden` 下发）。
    show_hidden: bool,
    /// 控制端选中的节点 id（单击选中、渲染高亮；Ctrl+C 复制它作为下载源）。换根 / 重列时清空。
    selected: Option<usize>,
}

/// 远程树一个真实节点（目录 / 文件；占位行不入表，渲染时合成）。
struct RemoteNode {
    /// 被控端不透明路径（往返键 + 显示）。
    path: String,
    /// 显示名（被控端 `display_name` 算好）。
    name: String,
    /// 是否目录。
    is_dir: bool,
    /// 文件字节数（目录 0）；片6 复制为虚拟文件时填 descriptor 的 `FD_FILESIZE`。
    size: u64,
}

/// 已读目录的 listing（子节点 + 溢出 / 不可读元信息，占位行渲染时据此合成）。
struct RemoteDirListing {
    /// 子节点 id（目录在前，被控端已排序）。
    children: Vec<usize>,
    /// 单层截断未显示项数（`>0` 画「溢出」占位）。
    overflow: u32,
    /// 读目录失败（画「无法读取」占位）。
    unreadable: bool,
}

/// 远程树可见行（[`RemoteFileTree::visible_rows`] 产出，供 `show_remote` 只读渲染）。
pub struct RemoteRow {
    /// 真实节点 id（占位行为 `usize::MAX`，不可点）。
    pub id: usize,
    /// 不透明路径（占位行为空）。
    pub path: String,
    /// 显示名（占位行为空，渲染查 i18n 文案）。
    pub name: String,
    /// 缩进深度（根 = 0）。
    pub depth: u32,
    /// 行种类。
    pub kind: RemoteRowKind,
    /// 文件字节数（目录 / 占位行 0）；右键 / 快捷键复制为虚拟文件时带给 descriptor。
    pub size: u64,
}

/// 远程树行种类。
pub enum RemoteRowKind {
    /// 目录（`open` 决定三角朝向与是否下钻）。
    Dir {
        /// 是否展开。
        open: bool,
    },
    /// 文件。
    File,
    /// 「加载中…」占位（该目录 ListDir 在途）。
    Loading,
    /// 「无法读取」占位（被控端读目录失败）。
    Unreadable,
    /// 「还有 N 项未显示」溢出占位。
    Overflow(u32),
}

impl RemoteRow {
    fn placeholder(depth: u32, kind: RemoteRowKind) -> Self {
        Self {
            id: usize::MAX,
            path: String::new(),
            name: String::new(),
            depth,
            kind,
            size: 0,
        }
    }
}

impl RemoteFileTree {
    /// 换根（被控端 cwd 变）：根不同才整体重置，返回是否真的换了。
    fn set_root(&mut self, path: String) -> bool {
        if self.root.as_deref() == Some(path.as_str()) {
            return false;
        }
        self.root = Some(path);
        self.reset_to_root();
        true
    }

    /// 重建节点表（换根 / 切显示隐藏项）：清空缓存 / 展开态 / 在途请求，重置为「根 +
    /// 根默认展开」（root 为 None 时只清空）。清空 `pending` 即令旧 ListDir 应答失配作废
    /// （配合 `req_seq` 进程级单调 + `find_dir` 路径可达，三重丢弃陈旧 / 乱序应答）。
    fn reset_to_root(&mut self) {
        self.nodes.clear();
        self.listings.clear();
        self.open.clear();
        self.pending.clear();
        self.selected = None;
        if let Some(root) = &self.root {
            let name = last_path_segment(root);
            self.nodes.push(RemoteNode {
                path: root.clone(),
                name,
                is_dir: true,
                size: 0,
            });
            self.open.insert(0); // 根默认展开
        }
    }

    /// 切「显示隐藏项」：变化才重列（重置回根，调用方随后 re-request 根 listing）。
    fn set_show_hidden(&mut self, show: bool) -> bool {
        if self.show_hidden == show {
            return false;
        }
        self.show_hidden = show;
        self.reset_to_root();
        true
    }

    /// 应答 ListDir：按 `dir_path` DFS 找 dir id（只命中可达节点），`pending` 双键校验后把
    /// 子项填进 listing（陈旧 / 乱序 / 换根后的应答静默丢弃）。
    fn apply_dir_entries(
        &mut self,
        req_id: u64,
        dir_path: &str,
        entries: Vec<DirEntry>,
        overflow: u32,
        err: Option<FsErr>,
    ) {
        let Some(dir_id) = self.find_dir(dir_path) else {
            return;
        };
        if self.pending.get(&dir_id) != Some(&req_id) {
            return; // 双键不匹配：陈旧 / 乱序 / 已被换根作废。
        }
        self.pending.remove(&dir_id);
        if err.is_some() {
            self.listings.insert(
                dir_id,
                RemoteDirListing {
                    children: Vec::new(),
                    overflow: 0,
                    unreadable: true,
                },
            );
            return;
        }
        let mut children = Vec::with_capacity(entries.len());
        for e in entries {
            let cid = self.push_node(RemoteNode {
                path: e.path,
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
            });
            children.push(cid);
        }
        self.listings.insert(
            dir_id,
            RemoteDirListing {
                children,
                overflow,
                unreadable: false,
            },
        );
    }

    fn push_node(&mut self, node: RemoteNode) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// 按不透明路径找 **Dir** 节点 id——从根沿 listings DFS，只命中当前可达节点
    /// （append-only 表里被覆盖的旧 listing 节点不被任何 listing 引用，天然跳过）。
    fn find_dir(&self, path: &str) -> Option<usize> {
        self.find_dir_visit(0, path)
    }

    fn find_dir_visit(&self, id: usize, path: &str) -> Option<usize> {
        let node = self.nodes.get(id)?;
        if node.is_dir && node.path == path {
            return Some(id);
        }
        if let Some(listing) = self.listings.get(&id) {
            for &child in &listing.children {
                if let Some(found) = self.find_dir_visit(child, path) {
                    return Some(found);
                }
            }
        }
        None
    }

    // —— 展开态 / 节点查询（供 RemoteWs 编排点击与 ListDir 请求）——
    fn is_open(&self, id: usize) -> bool {
        self.open.contains(&id)
    }
    fn set_open(&mut self, id: usize, open: bool) {
        if open {
            self.open.insert(id);
        } else {
            self.open.remove(&id);
        }
    }
    fn has_listing(&self, id: usize) -> bool {
        self.listings.contains_key(&id)
    }
    fn is_pending(&self, id: usize) -> bool {
        self.pending.contains_key(&id)
    }
    fn mark_pending(&mut self, id: usize, req_id: u64) {
        self.pending.insert(id, req_id);
    }
    fn clear_pending(&mut self, id: usize) {
        self.pending.remove(&id);
    }
    /// 作废一个目录的缓存（刷新用）：删 listing + 在途，下次按需重拉。
    fn clear_listing(&mut self, id: usize) {
        self.listings.remove(&id);
        self.pending.remove(&id);
    }
    fn node_path(&self, id: usize) -> Option<&str> {
        self.nodes.get(id).map(|n| n.path.as_str())
    }
    fn node_is_dir(&self, id: usize) -> bool {
        self.nodes.get(id).is_some_and(|n| n.is_dir)
    }

    /// 当前选中的节点 id（渲染高亮用，单击设置）。
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
    fn set_selected(&mut self, id: usize) {
        self.selected = Some(id);
    }
    /// 选中节点的 `(path, name, is_dir)`——Ctrl+C 复制为下载源用；占位 / 越界返回 `None`。
    #[must_use]
    pub fn selected_item(&self) -> Option<(String, String, bool, u64)> {
        let n = self.nodes.get(self.selected?)?;
        Some((n.path.clone(), n.name.clone(), n.is_dir, n.size))
    }
    /// 选中节点若是目录则返回其 path——Ctrl+V 粘贴（上传）目标用；否则 `None`。
    #[must_use]
    pub fn selected_dir(&self) -> Option<String> {
        let n = self.nodes.get(self.selected?)?;
        n.is_dir.then(|| n.path.clone())
    }

    /// 树根的不透明路径（工具条标题 + 悬停看全路径）。`None` = 等待 cwd。
    #[must_use]
    pub fn root_label(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// 「显示隐藏项」当前开关（工具条勾选框回显）。
    #[must_use]
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// 当前可见行（DFS 按展开态下钻；展开但 listing 未到画「加载中」占位）。供只读渲染。
    #[must_use]
    pub fn visible_rows(&self) -> Vec<RemoteRow> {
        let mut rows = Vec::new();
        if !self.nodes.is_empty() {
            self.visit(0, 0, &mut rows);
        }
        rows
    }

    fn visit(&self, id: usize, depth: u32, rows: &mut Vec<RemoteRow>) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        let open = node.is_dir && self.open.contains(&id);
        rows.push(RemoteRow {
            id,
            path: node.path.clone(),
            name: node.name.clone(),
            depth,
            kind: if node.is_dir {
                RemoteRowKind::Dir { open }
            } else {
                RemoteRowKind::File
            },
            size: node.size,
        });
        if !open {
            return;
        }
        if self.pending.contains_key(&id) {
            rows.push(RemoteRow::placeholder(depth + 1, RemoteRowKind::Loading));
        } else if let Some(listing) = self.listings.get(&id) {
            for &child in &listing.children {
                self.visit(child, depth + 1, rows);
            }
            if listing.unreadable {
                rows.push(RemoteRow::placeholder(depth + 1, RemoteRowKind::Unreadable));
            }
            if listing.overflow > 0 {
                rows.push(RemoteRow::placeholder(
                    depth + 1,
                    RemoteRowKind::Overflow(listing.overflow),
                ));
            }
        } else {
            // 展开但既无 listing 又非 pending（点击当帧、主线程尚未发 ListDir）：临时占位。
            rows.push(RemoteRow::placeholder(depth + 1, RemoteRowKind::Loading));
        }
    }
}

/// 被控端读目录服务任务（主线程 [`RemoteWs::spawn_list_dir`]/`spawn_list_dir_recursive` 入有界
/// 队列，常驻 worker 池消费）。Fetch/Put 大文件传输各有滑动窗口背压，不经此队列。
enum DirJob {
    /// 单层列目录（→ [`SvcReply::ListDir`]）。
    List {
        req_id: u64,
        path: String,
        show_hidden: bool,
    },
    /// 片8 递归列目录树（→ [`SvcReply::ListDirRecursive`]）。
    Recursive {
        req_id: u64,
        path: String,
        show_hidden: bool,
    },
}

/// 被控端文件服务后台线程 → 主线程的回包（主线程在 `pump_remote` 排空后处理）。
enum SvcReply {
    /// ListDir 读目录结果（主线程发回 [`RemoteFrame::ListDirResult`]）。
    ListDir {
        req_id: u64,
        path: String,
        entries: Vec<DirEntry>,
        overflow: u32,
        err: Option<FsErr>,
    },
    /// Fetch 源 worker 已结束（文件读完 / 出错 / 被中止）：主线程移除 `inflight_fetch_src`
    /// 该项（worker 自己经 `cmd_tx` 直发 `FileBegin/Chunk/End/Err`，此信号仅用于清理 map）。
    FetchSrcDone { req_id: u64 },
    /// 片5 上传：Put 源 worker 正常发完 `PutEnd`：主线程移除 `inflight_put_src`（等被控端
    /// `PutResult` 减 `active_puts`、计统计）。
    PutSrcDone { req_id: u64 },
    /// 片5 上传：Put 源 worker open/read 失败中止（未发 `PutEnd`）：被控端不会回 `PutResult`，
    /// 故主线程须自行计错 + 减 `active_puts` + 清元信息 + 续 pump（H2 修复，否则上传挂死）。
    PutSrcFailed { req_id: u64 },
    /// 片8 ListDirRecursive 递归读目录树结果（主线程发回 [`RemoteFrame::ListDirRecursiveResult`]）。
    ListDirRecursive {
        req_id: u64,
        path: String,
        entries: Vec<RecursiveDirEntry>,
        truncated: bool,
        err: Option<FsErr>,
    },
}

/// 片6 虚拟文件剪贴板：OLE 线程的 `RemoteFileStream`（见 `virtual_files`）收到资源管理器
/// `IStream::Read` 时发本命令 + `PtyWake` 唤醒 UI 主线程起一次 [`FetchKind::Clipboard`] 流式拉取；
/// UI 主线程把分块经 `data_tx` 边下边喂给该流，进度条随真实下载平滑推进。
pub struct ClipFetchCmd {
    /// 被控端文件不透明路径。
    pub path: String,
    /// 流数据下行端：UI 主线程把 [`StreamMsg`] 喂进来，OLE 线程的 IStream 读取。
    pub data_tx: std::sync::mpsc::Sender<StreamMsg>,
}

/// 片6 流式下载：UI 主线程 → OLE 线程 IStream 的数据消息。
pub enum StreamMsg {
    /// 一块文件字节（顺序，≤ `FILE_CHUNK`）。
    Chunk(Vec<u8>),
    /// 文件传完（`FileEnd`）：IStream 读到此即 EOF。
    Done,
    /// 中止（对端 `FileErr` / 乱序 / 停滞 / 会话断）：IStream 据此返回错误（资源管理器粘贴失败，
    /// 不落不完整文件）。
    Failed,
}

/// 控制端在途 Fetch 的用途：决定落地位置与收完动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchKind {
    /// #5 双击打开：落临时文件，`FileEnd` 后用本地默认程序打开。
    Open,
    /// #7 下载：落到目标本地路径，`FileEnd` 后只关句柄（不打开），并推进下载编排。
    Download,
    /// 片6 虚拟文件剪贴板：**不落盘**，把分块边收边经 `clip_stream` 喂给 OLE 线程的
    /// `RemoteFileStream`（资源管理器 `IStream::Read` 阻塞消费），按需流式下载 + 实时进度。
    Clipboard,
}

/// 控制端在途 Fetch 任务（接收方）：把被控端分块传来的文件字节顺序写入本地文件
/// （Open=临时文件 / Download=目标路径），ACK 背压。
struct FetchJob {
    /// 用途（打开 vs 下载）。
    kind: FetchKind,
    /// 被控端源文件不透明路径（打开去重键 / 日志）。
    src_path: String,
    /// 临时文件用的安全 basename（仅 `Open`：被控端 `DirEntry.name` 清洗，保留扩展名）。
    name: String,
    /// 正在写入的本地文件路径（`Open`=临时文件、`Download`=目标路径；`FileBegin` 时确定）。
    dest: Option<std::path::PathBuf>,
    /// 已打开的写入句柄（`FileBegin` 后 `Some`）。
    file: Option<std::fs::File>,
    /// 下一个期望块序号（连续性校验）。
    next_seq: u32,
    /// 已写入字节累计（控制端硬上限：超 [`FETCH_MAX_LEN`] 即中止，不轻信被控端的上限）。
    written: u64,
    /// 文件总字节（`FileBegin.total_len`；状态栏下载进度分母，0=未知/未开始）。仅展示用，
    /// finalize 仍以 `FileEnd` 为准（不轻信此值）。
    total: u64,
    /// 上次收到块的时刻（停滞超时清理；`FileBegin` 与每块刷新）。
    last_at: Instant,
    /// 仅 `Clipboard`：把收到的分块 / 结束 / 失败喂给 OLE 线程 IStream 的下行端。
    clip_stream: Option<std::sync::mpsc::Sender<StreamMsg>>,
}

/// 跨「本地 / 远程」两侧的文件剪贴板（复制只记引用、零传输；粘贴才触发字节流）。
pub struct FileClipboard {
    /// 复制来源侧。
    pub side: ClipSide,
    /// 复制的项（path 不透明、name 显示名、is_dir 决定递归）。
    pub items: Vec<ClipItem>,
}

/// 剪贴板来源 / 粘贴目标侧。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipSide {
    /// 控制端本机（本地文件树）。
    Local,
    /// 被控端（远程文件树）。
    Remote,
}

/// 剪贴板一项。
#[derive(Clone)]
pub struct ClipItem {
    /// 不透明路径（远程=被控端路径 / 本地=控制端路径）。
    pub path: String,
    /// 显示名（落地时的 basename）。
    pub name: String,
    /// 是否目录（决定递归 vs 单文件）。
    pub is_dir: bool,
}

/// 状态栏数据面链路指示（[`RemoteWs::p2p_link_state`]）：当前走 P2P 直连还是中继。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum P2pLink {
    /// 数据面走 P2P 直连（绕开中继，更低延迟）。
    Direct,
    /// 数据面走中继转发（无直连 / 直连断后回退）。
    Relay,
}

/// 控制端状态栏文件传输进度聚合（每帧由 [`RemoteWs::transfer_status`] 算；空闲返回 `None`）。
/// 下载（含双击打开）有 `written`/`total` → 聚合字节进度条；上传仅计数（控制端不集中跟踪上传
/// 字节）。剪贴板流式拉取不计入（进度在资源管理器 IStream 侧）。
pub struct TransferStatus {
    /// 活跃下载文件数（含双击打开）。
    pub downloads: usize,
    /// 活跃上传文件数。
    pub uploads: usize,
    /// 下载已传字节聚合（`sum(written)`）。
    pub down_done: u64,
    /// 下载总字节聚合（`sum(total)`；可能为 0 = 尚未收到 `FileBegin`，此时进度条画不定态）。
    pub down_total: u64,
    /// 在传文件名（下载 + 上传；状态栏按帧时间轮换展示其一）。
    pub names: Vec<String>,
}

impl TransferStatus {
    /// 下载聚合进度比 `[0,1]`；`down_total==0`（未知）时返回 `None`（状态栏画不定态）。
    #[must_use]
    pub fn down_ratio(&self) -> Option<f32> {
        (self.down_total > 0).then(|| (self.down_done as f32 / self.down_total as f32).clamp(0.0, 1.0))
    }
}

/// #7 下载编排（远程 → 本地）：控制端用 ListDir 递归走被控端目录树、Fetch 拉文件落到本地
/// 对应路径。复用 `inflight_fetch`（`kind=Download`）做文件传输，本结构跟踪目录遍历 + 队列。
struct DownloadWalk {
    /// 撞名策略：`true`=覆盖已存在、`false`=跳过已存在（粘贴时一次性决策，套用整次递归）。
    overwrite: bool,
    /// 在途目录列举：req_id → (该远程目录的子项要落入的本地目录, 深度)。
    dir_listdir: HashMap<u64, (std::path::PathBuf, usize)>,
    /// 待列举目录队列 (远程目录 path, 本地落地目录, 深度)（受 [`DOWNLOAD_MAX_LISTDIR`] 节流，
    /// 防深 / 宽树一次性向被控端洪泛 ListDir）。
    dir_queue: std::collections::VecDeque<(String, std::path::PathBuf, usize)>,
    /// 待拉取文件队列 (远程文件 path, 本地落地 path)（受并发上限节流）。
    file_queue: std::collections::VecDeque<(String, std::path::PathBuf)>,
    /// 正在 Fetch 的文件数（并发上限 [`DOWNLOAD_MAX_FILES`]）。
    active_files: usize,
    /// 已访问远程目录路径（防 junction / symlink 成环）。
    visited: HashSet<String>,
    /// 统计：完成 / 跳过 / 出错文件数（结束 toast 汇总）。
    done: usize,
    skipped: usize,
    errors: usize,
}

/// 片5 上传编排（本地 → 被控端）：控制端用 MkDir 递归建被控端目录结构、PutBegin/Chunk/End
/// 把本地文件写到被控端对应路径。复用 `inflight_put_src`/`inflight_put_meta` 做文件传输，本
/// 结构跟踪目录遍历 + 队列 + 撞名决策（镜像 [`DownloadWalk`]，方向相反）。
struct UploadWalk {
    /// 被控端落地根目录（粘贴目标目录；仅供日志 / 不变量）。
    remote_root: String,
    /// 撞名策略：`None` = 遇冲突弹覆盖模态（暂停）、`Some(true)` = 覆盖全部、`Some(false)` =
    /// 跳过全部已存在（一次性决策后套用整次递归）。
    policy: Option<bool>,
    /// 待发送文件队列 (本地源路径, 被控端目标目录, 落地名)（受 [`DOWNLOAD_MAX_FILES`] 节流）。
    file_queue: VecDeque<(PathBuf, String, String)>,
    /// 在途 MkDir：req_id → (本地目录, 深度)。回 [`RemoteFrame::MkDirResult`] 后读该本地目录、
    /// 把子项入队（子目录续发 MkDir、文件入 file_queue）。
    inflight_mkdir: HashMap<u64, (PathBuf, usize)>,
    /// 正在传输（PutBegin 已发、未 PutResult）的文件数（并发上限 [`DOWNLOAD_MAX_FILES`]）。
    active_puts: usize,
    /// 已访问本地目录 canonical 路径（防 junction / symlink 成环）。
    visited: HashSet<PathBuf>,
    /// 撞名待决队列（`policy==None` 期间 Probe 探得冲突的 req_id；**队列**而非单值——并发探测
    /// 时多个冲突同时回来不会互相覆盖，H1 修复）。非空 + 未决期间 pump 不起新文件。决策后
    /// 一次性套用 policy 排空。元信息复用 `inflight_put_meta[req_id]`。
    conflict_queue: VecDeque<u64>,
    /// 统计：完成 / 跳过 / 出错文件数（结束 toast 汇总）。
    done: usize,
    skipped: usize,
    errors: usize,
}

/// 被控端在途 Fetch 源（发送方）：仅持给 worker 发许可的句柄，worker 经 `cmd_tx` 直发帧。
struct FetchSrcJob {
    /// 每收一个 [`RemoteFrame::FileChunkAck`] 发一个许可，worker 领许可才读发下一块（背压）。
    permit_tx: Sender<()>,
}

/// 被控端在途 Put 目标（接收方，片5 上传）：把控制端分块传来的字节顺序写**临时文件**
/// （同目录 `.lumen-put-{req_id}.tmp` 保证 rename 同卷原子），`PutEnd` 时 flush + rename 到目标。
struct PutDstJob {
    /// 临时文件路径（`dir/.lumen-put-{req_id}.tmp`；同目标目录同卷）。
    tmp_path: PathBuf,
    /// 最终目标路径（`dir.join(name)`；rename 落地点，Force 覆盖语义）。
    target: PathBuf,
    /// 已打开的写入句柄（`PutBegin` 建临时文件后 `Some`）。
    file: Option<std::fs::File>,
    /// 下一个期望块序号（连续性校验）。
    next_seq: u32,
    /// 已写入字节累计（控制端硬上限：超 [`FETCH_MAX_LEN`] 中止；对称于 `FetchJob`）。
    written: u64,
}

/// 控制端在途 Put 源（发送方，片5 上传）：仅持给 worker 发许可的句柄，worker 经 `cmd_tx`
/// 直发 [`RemoteFrame::PutChunk`] / [`RemoteFrame::PutEnd`]（对称于 [`FetchSrcJob`]）。
struct PutSrcJob {
    /// 每收一个 [`RemoteFrame::PutChunkAck`] 发一个许可，worker 领许可才读发下一块（背压）。
    permit_tx: Sender<()>,
}

/// 控制端在途 Put 的元信息（片5 上传）：req_id → 决策 `PutReady` / 重发 Force / 统计所需的上下文。
struct PutMeta {
    /// 本地源文件路径（worker 读取源；冲突后重发 Force 复用）。
    local_path: PathBuf,
    /// 被控端目标目录（不透明；重发 Force 复用）。
    remote_dir: String,
    /// 落地文件名（被控端 `dir.join(name)` 拼路径；重发 Force 复用）。
    name: String,
}

/// 取路径末段作显示名（`C:\Users\hf` → `hf`；盘符根 `C:\` 等无末段时返回整串）。
fn last_path_segment(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// 取不透明远程路径的父目录（剥末段，兼容 `/` 与 `\`）。无父（盘符根 / 单段）则回原串（刷新自身）。
/// 删除完成后用它刷新被删项所在目录。
fn parent_remote_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(i) if i > 0 => trimmed[..i].to_owned(),
        _ => trimmed.to_owned(),
    }
}

/// 把任意名字清洗成安全文件名（非 `[A-Za-z0-9._-]` 一律换 `_`；空则 `file`）。临时文件命名
/// 用（沿用 `update::installer_dest` 思路，扩展名通常纯 alnum 得以保留，本地默认程序按其匹配）。
fn sanitize_basename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "file".to_owned()
    } else {
        s
    }
}

/// 名字是否是安全的单层子项名（恰一个普通组件）：拒 `..` / 绝对路径 / 盘符 / 路径分隔符 /
/// 空 / `.`。用于下载落地与上传写入，防对端给的 `name` 驱动路径穿越（H1 安全）。保留 Unicode
/// （不像 `sanitize_basename` 把 CJK 改写成 `_`）。
fn is_safe_child_name(name: &str) -> bool {
    use std::path::Component;
    let mut comps = std::path::Path::new(name).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// `io::Error` → 机器可读 [`FsErr`]（控制端本地化提示用）。
fn io_err_to_fs(e: &std::io::Error) -> FsErr {
    match e.kind() {
        std::io::ErrorKind::NotFound => FsErr::NotFound,
        std::io::ErrorKind::PermissionDenied => FsErr::PermissionDenied,
        _ => FsErr::Io,
    }
}

/// 被控端读目录服务 worker（[`RemoteWs::start`] 起 `DIR_SERVICE_WORKERS` 个常驻线程）：从有界
/// 队列取任务、读盘构造回包、经 `svc_tx` 回主线程 + nudge 唤醒。多 worker 共享 `Arc<Mutex<Receiver>>`：
/// **仅在锁内 `recv`，取到立即解锁再读盘**（读盘不持锁，放并发）。`recv` 返回 `Err`（主线程 drop
/// 队列 sender，即会话停止）即退出——无需 join。
fn dir_service_worker(
    job_rx: &Arc<Mutex<Receiver<DirJob>>>,
    svc_tx: &Sender<SvcReply>,
    nudge_h: &(egui::Context, EventLoopProxy<PtyWake>, Arc<AtomicBool>),
) {
    loop {
        let job = {
            // 仅在锁内 recv（不持锁读盘，放并发）。锁中毒（理论上持 guard 期间 panic——当前 recv
            // 不 panic、build_* 在解锁后执行故不会毒化，此为防御）：recover 内层 Receiver 继续，
            // 使一次 panic 不致经中毒锁连环拖垮整池，仅损一个 worker。
            let guard = job_rx.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.recv() {
                Ok(j) => j,
                Err(_) => return, // sender 全 drop（会话停止）→ 队列关闭，退出。
            }
        };
        let reply = match job {
            DirJob::List {
                req_id,
                path,
                show_hidden,
            } => build_list_dir_reply(req_id, path, show_hidden),
            DirJob::Recursive {
                req_id,
                path,
                show_hidden,
            } => build_list_dir_recursive_reply(req_id, path, show_hidden),
        };
        // UI 先退出 / 会话停止时 svc 通道已关：发送失败即收尾退出。
        if svc_tx.send(reply).is_err() {
            return;
        }
        let (ctx, proxy, wake) = nudge_h;
        nudge(ctx, proxy, wake);
    }
}

/// 构造单层列目录回包（worker 内调，与旧 `spawn_list_dir` 逻辑等价）。
fn build_list_dir_reply(req_id: u64, path: String, show_hidden: bool) -> SvcReply {
    match crate::shell::filetree::list_dir_entries(std::path::Path::new(&path), show_hidden) {
        Ok((entries, overflow)) => SvcReply::ListDir {
            req_id,
            path,
            entries,
            overflow: u32::try_from(overflow).unwrap_or(u32::MAX),
            err: None,
        },
        Err(()) => SvcReply::ListDir {
            req_id,
            path,
            entries: Vec::new(),
            overflow: 0,
            err: Some(FsErr::Io),
        },
    }
}

/// 构造片8 递归列目录树回包（worker 内调，与旧 `spawn_list_dir_recursive` 逻辑等价）：先探根
/// 可读性（失败回 `err`），再 DFS 枚举，5s deadline 防慢盘长阻塞、超上限截断。
fn build_list_dir_recursive_reply(req_id: u64, path: String, show_hidden: bool) -> SvcReply {
    let reply = match std::fs::read_dir(&path) {
        Err(e) => SvcReply::ListDirRecursive {
            req_id,
            path,
            entries: Vec::new(),
            truncated: false,
            err: Some(io_err_to_fs(&e)),
        },
        Ok(_) => {
            let deadline = Some(Instant::now() + Duration::from_secs(5));
            let (entries, truncated) = crate::shell::filetree::list_dir_recursive(
                std::path::Path::new(&path),
                show_hidden,
                LIST_DIR_RECURSIVE_MAX_DEPTH,
                LIST_DIR_RECURSIVE_MAX_ENTRIES,
                deadline,
            );
            SvcReply::ListDirRecursive {
                req_id,
                path,
                entries,
                truncated,
                err: None,
            }
        }
    };
    if let SvcReply::ListDirRecursive {
        entries,
        truncated,
        err,
        ..
    } = &reply
    {
        log::debug!(
            "[片8] 被控端递归枚举完成: {} 项 truncated={truncated} err={err:?}",
            entries.len()
        );
    }
    reply
}

/// 控制端「双击打开远程文件」临时夹（`temp/lumen_remote_open`）的总字节上限：单会话内反复打开
/// 远程文件会不断在此堆副本（`fetch_end` 后不删，留给外部程序占用），仅靠会话结束整夹清。超此
/// 上限即 LRU 淘汰最旧的。500 MiB（roadmap §3.1.3 默认；后续可经设置项调，暂用常量）。
const REMOTE_OPEN_DIR_CAP: u64 = 500 * 1024 * 1024;

/// 对控制端远程打开临时夹做容量淘汰（每次 `fetch_end` 打开新文件后调）。见 [`enforce_dir_byte_cap`]。
fn enforce_remote_open_cap(cap: u64) {
    enforce_dir_byte_cap(&std::env::temp_dir().join("lumen_remote_open"), cap);
}

/// 对 `dir` 做按 mtime 近似的 LRU 字节上限淘汰：累计文件字节超 `cap` 时从**最旧**（mtime 最小）
/// 起删，直到 ≤ `cap`。**best-effort**：删不掉的（Windows 上正被打开它的程序占用——通常正是最新、
/// 最该留的）静默跳过，留到会话结束整夹清兜底。纯维护，无错误传播。抽出固定夹外以便单测。
fn enforce_dir_byte_cap(dir: &std::path::Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return; // 夹不存在 / 不可读：无可淘汰。
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total = total.saturating_add(size);
        files.push((entry.path(), size, mtime));
    }
    if total <= cap {
        return;
    }
    files.sort_by_key(|(_, _, mtime)| *mtime); // 最旧在前。
    for (path, size, _) in files {
        if total <= cap {
            break;
        }
        // 删不掉（占用）则 total 不减，继续尝试更旧的其它文件。
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// 被控端 Fetch 源后台线程：打开文件 → 发 `FileBegin` → 受许可（ACK 窗口）逐块读发
/// `FileChunk` → `FileEnd`。出错发 `FileErr`。帧经 `cmd_tx` 直投 WS 出站（不经主线程，
/// 避免每块一次主线程往返）；许可通道关闭（会话结束 / 主线程清 `inflight_fetch_src`）即中止。
fn fetch_src_worker(
    req_id: u64,
    path: &str,
    cmd_tx: &Sender<RemoteC2S>,
    permit_rx: &Receiver<()>,
) {
    let send = |frame: &RemoteFrame| {
        if let Ok(v) = frame.to_value() {
            let _ = cmd_tx.send(RemoteC2S::Relay(v));
        }
    };
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            send(&RemoteFrame::FileErr {
                req_id,
                err: io_err_to_fs(&e),
            });
            return;
        }
    };
    // metadata 失败视为不可信（不能兜底为 0 而绕过上限）：直接报错中止。
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            send(&RemoteFrame::FileErr {
                req_id,
                err: io_err_to_fs(&e),
            });
            return;
        }
    };
    if len > FETCH_MAX_LEN {
        send(&RemoteFrame::FileErr {
            req_id,
            err: FsErr::TooLarge,
        });
        return;
    }
    send(&RemoteFrame::FileBegin {
        req_id,
        total_len: len,
    });
    let mut seq: u32 = 0;
    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        // 领许可：控制端 ACK 驱动；通道关闭（会话结束 / 被清理）即中止。
        if permit_rx.recv().is_err() {
            return;
        }
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                send(&RemoteFrame::FileErr {
                    req_id,
                    err: io_err_to_fs(&e),
                });
                return;
            }
        };
        if n == 0 {
            send(&RemoteFrame::FileEnd { req_id });
            return;
        }
        send(&RemoteFrame::FileChunk {
            req_id,
            seq,
            data: buf[..n].to_vec(),
        });
        seq = seq.wrapping_add(1);
    }
}

/// 控制端 Put 源后台线程（片5 上传，镜像 [`fetch_src_worker`]，方向相反）：编排已先发
/// `PutBegin` 并收到 `PutReady{conflict:None}`，本 worker 只在领许可（ACK 窗口）后逐块读
/// **本地源文件**发 [`RemoteFrame::PutChunk`]，EOF 发 [`RemoteFrame::PutEnd`]。读失败静默退出
/// （不发 PutChunk/PutEnd，被控端临时文件会因会话清理或停滞被回收）。帧经 `cmd_tx` 直投 WS
/// 出站（不经主线程）；许可通道关闭（会话结束 / 编排清 `inflight_put_src` / 取消）即中止。
/// 返回 `true`=正常发完 `PutEnd`；`false`=open/read 失败或许可通道中途关闭（中止）。调用方据此
/// 发 `PutSrcDone`（成功，等被控端 `PutResult`）或 `PutSrcFailed`（失败，控制端即时计错收尾，
/// 否则 `active_puts` 永不归零 → 上传挂死，H2 修复）。
fn put_send_worker(
    req_id: u64,
    local_path: &str,
    cmd_tx: &Sender<RemoteC2S>,
    permit_rx: &Receiver<()>,
) -> bool {
    let send = |frame: &RemoteFrame| {
        if let Ok(v) = frame.to_value() {
            let _ = cmd_tx.send(RemoteC2S::Relay(v));
        }
    };
    let mut file = match std::fs::File::open(local_path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("上传打开本地文件失败 {local_path}: {e}");
            return false;
        }
    };
    let mut seq: u32 = 0;
    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        // 领许可：控制端收 PutChunkAck 驱动；通道关闭（会话结束 / 被清理 / 取消）即中止。
        if permit_rx.recv().is_err() {
            return false;
        }
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("上传读本地文件失败 {local_path}: {e}");
                return false;
            }
        };
        if n == 0 {
            send(&RemoteFrame::PutEnd { req_id });
            return true;
        }
        send(&RemoteFrame::PutChunk {
            req_id,
            seq,
            data: buf[..n].to_vec(),
        });
        seq = seq.wrapping_add(1);
    }
}

pub struct MirrorFrame<'a> {
    /// 本帧要渲染的终端（live 镜像或历史窗口 scratch）。
    pub term: &'a Terminal,
    /// 光标 `(row, col, visible)`：跟随态取 live 光标，回看态 `(0, 0, false)` 隐藏。
    pub cursor: (usize, usize, bool),
    /// 控制端镜像选区（part4b）：与 `term` 同坐标系，渲染器据此画高亮。空/无则 `None`。
    pub selection: Option<&'a Selection>,
}

impl RemoteWs {
    /// 登录后启动后台 WS 线程（已在跑则先停旧的）。`token` 为**共享**账户 JWT 句柄——心跳 worker
    /// 自动续期时写回同一句柄，WS 每次（重）连读其当前值，确保续期后重连用新 token（免 7 天到期 401）。
    ///
    /// `proxy` + `wake_pending`：后台收到消息时除 `ctx.request_repaint()` 外，再发
    /// `PtyWake` user event 唤醒 winit 事件循环——**否则窗口失焦时 `request_repaint`
    /// 唤不醒空闲循环，远程消息（配对/输入/镜像）会卡到焦点回来才处理**（与 PTY
    /// 输出同款唤醒机制，共用 `wake_pending` 去重防事件风暴）。
    pub fn start(
        &mut self,
        token: Arc<RwLock<String>>,
        ctx: egui::Context,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    ) {
        self.stop();
        // part3c-2 #5：清上次会话残留的远程打开临时文件。成功打开的副本在会话结束时被外部
        // 程序占用、删不掉（fetch_end 不删），故启动时整目录 best-effort 清一次（删不掉的
        // 旧文件留到下次）——同时兜底所有中止路径删除失败的半成品。
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("lumen_remote_open"));
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        // 被控端文件服务回包通道（worker 线程 → 主线程；纯主线程内部，不经 WS 后台线程）。
        let (svc_tx, svc_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.cmd_tx = Some(cmd_tx);
        self.evt_rx = Some(evt_rx);
        self.svc_tx = Some(svc_tx.clone());
        self.svc_rx = Some(svc_rx);
        self.stop = Some(stop.clone());
        // 存一套 nudge 句柄：被控端文件服务 worker（spawn_list_dir）读盘完成后唤醒主线程
        // drain_service 立即发回结果（修「远程目录加载慢 ~10s」）。
        self.ctx = Some(ctx.clone());
        self.proxy = Some(proxy.clone());
        self.wake_pending = Some(wake_pending.clone());
        // 读目录服务常驻 worker 池 + 有界队列（review MED-1）：替代旧「每请求起一线程、无上限」。
        // 多 worker 共享 Arc<Mutex<Receiver>>；主线程 drop dir_job_tx（stop）即关队列、worker 退出。
        let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<DirJob>(DIR_SERVICE_QUEUE_CAP);
        self.dir_job_tx = Some(job_tx);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let nudge_h = (ctx.clone(), proxy.clone(), wake_pending.clone());
        for i in 0..DIR_SERVICE_WORKERS {
            let job_rx = Arc::clone(&job_rx);
            let svc_tx = svc_tx.clone();
            let nudge_h = nudge_h.clone();
            if let Err(e) = thread::Builder::new()
                .name(format!("lumen-dir-svc-{i}"))
                .spawn(move || dir_service_worker(&job_rx, &svc_tx, &nudge_h))
            {
                log::error!("启动读目录服务 worker {i} 失败: {e}");
            }
        }
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
        self.reset_multi_session();
        // 先 clear（其内排空 svc_rx 丢弃在途回包）再断 svc 通道——否则 svc_rx 已 None
        // 时排空成死代码（与 end_session/Disconnected 路径行为不一致）。
        self.clear_remote_filetree();
        // 先 drop 读目录队列入口：关 sync_channel → worker 的 recv 返回 Err 即退出（无需 join）。
        self.dir_job_tx = None;
        self.svc_tx = None;
        self.svc_rx = None;
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

    /// part3d：复位多会话镜像态（会话起止 / 断线 / 被取代时调）。控制端清远程列表 + 订阅；
    /// 被控端清订阅目标 + 列表去重基线。与 [`Self::reset_history`] 互补（后者管单 mirror 回看态）。
    fn reset_multi_session(&mut self) {
        self.remote_tabs.clear();
        self.subscribed_tab = None;
        self.mirror_focus_sid = None;
        self.mirror_panes.clear();
        self.mirror_layout = None;
        self.mirror_active_pane = None;
        self.mirror_pane_selecting = None;
        self.last_sub_viewport = None;
        self.pending_sub_viewport = None;
        self.pending_sub_layout = None;
        self.sub_layout_baseline = None;
        self.sub_target = None;
        self.sub_dirty = false;
        self.last_tab_states.clear();
        self.pending_new_tab.clear();
        self.pending_close_tab.clear();
        // M6：会话结束 / 断连 → 停 P2P 引擎（Drop 发停机信号）+ 复位直连态。
        self.p2p = None;
        self.p2p_data_active = false;
    }

    /// 控制端：多窗格镜像各窗格（渲染序）；空 = 单窗格模式（走 [`Self::mirror_render`]）。
    #[must_use]
    pub fn mirror_panes(&self) -> &[MirrorPane] {
        &self.mirror_panes
    }

    /// 控制端：多窗格镜像布局（`mirror_panes` 非空时有效）。
    #[must_use]
    pub fn mirror_layout(&self) -> Option<&MirrorLayout> {
        self.mirror_layout.as_ref()
    }

    /// 控制端：可变镜像布局——main 据 shell 回传的 `mirror_divider_drag` / `mirror_divider_reset`
    /// 调 [`PaneLayout::drag_col_to`] / [`PaneLayout::reset_rows`] 等改比例（与本地窗格分隔条同一
    /// 套 API），下一帧 `pane_rects` 变 → `SubViewport` 让后台被控端 resize 到此比例。
    pub fn mirror_layout_mut(&mut self) -> Option<&mut MirrorLayout> {
        self.mirror_layout.as_mut()
    }

    /// 控制端：焦点窗格在 `mirror_panes` 中的渲染下标（shell 据此高亮；按 session_id 查、非缓存下标）。
    #[must_use]
    pub fn mirror_active_pane_idx(&self) -> Option<usize> {
        let sid = self.mirror_active_pane?;
        self.mirror_panes.iter().position(|p| p.session_id == sid)
    }

    /// 控制端：设焦点窗格（点击镜像窗格）。变化时复位回看态（绝对行号体系按窗格独立，切窗格须重置），
    /// 由调用方据被控端在 `SubscriptionStarted` 给的该窗格 `(base, screen_top)` 重设边界。无效 id（不在
    /// 当前 `mirror_panes`）忽略。返回是否真的切换了焦点窗格。
    pub fn set_mirror_active_pane(&mut self, session_id: SessionId) -> bool {
        if self.mirror_active_pane == Some(session_id) {
            return false;
        }
        let Some(bounds) = self
            .mirror_panes
            .iter()
            .find(|p| p.session_id == session_id)
            .map(|p| (p.hist_base, p.hist_screen_top))
        else {
            return false;
        };
        self.mirror_active_pane = Some(session_id);
        self.reset_history(); // 切焦点窗格：清旧窗格回看态（hist_top/cache/term），避免绝对行串台。
        self.hist_bounds = Some(bounds); // 据新焦点窗格快照边界起步回看（随 HistoryRowsForPane 刷新）。
        true
    }

    /// 控制端：发订阅会话各窗格目标尺寸给被控端（Phase 3 尺寸同步；与上次相同则不发）。被控端据此
    /// resize 该会话窗格使镜像 1:1 忠实显示（仅其在被控端为后台 tab 时生效，见 `SubViewport` 规则）。
    pub fn send_sub_viewport(&mut self, tab_id: TabId, panes: PaneSizes) {
        if !self.is_controlling() {
            return;
        }
        let key = (tab_id, panes);
        if self.last_sub_viewport.as_ref() == Some(&key) {
            return;
        }
        self.last_sub_viewport = Some(key.clone());
        let panes = key
            .1
            .into_iter()
            .map(|(session_id, rows, cols)| PaneViewport {
                session_id,
                rows,
                cols,
            })
            .collect();
        self.send_frame(&RemoteFrame::SubViewport {
            tab_id: key.0,
            panes,
        });
    }

    /// 被控端：取走控制端请求的订阅会话各窗格目标尺寸（main 在该会话为后台 tab 时 resize 其窗格）。
    pub fn take_sub_viewport(&mut self) -> Option<SubViewportReq> {
        self.pending_sub_viewport.take()
    }

    /// 任一端：取走对端发来的订阅会话窗格比例（main 应用到本端布局后须调
    /// [`Self::note_sub_layout_baseline`] 更新基线免回声）。
    pub fn take_sub_layout(&mut self) -> Option<SubLayoutData> {
        self.pending_sub_layout.take()
    }

    /// 任一端：把本端**当前**订阅会话窗格比例发给对端——仅当与「已发/已应用基线」不近似相等才发
    /// （[`RemoteFrame::SubLayout`]，双向），并把基线推进到本次值。会话中（控制或被控）皆可发。
    pub fn send_sub_layout_if_changed(
        &mut self,
        tab_id: TabId,
        row_weights: Vec<f32>,
        col_weights: Vec<Vec<f32>>,
    ) {
        if self.session.is_none() {
            return;
        }
        let cur: SubLayoutData = (tab_id, row_weights, col_weights);
        if self
            .sub_layout_baseline
            .as_ref()
            .is_some_and(|base| weights_approx_eq(base, &cur))
        {
            return; // 与基线一致（含刚应用的对端比例）：不发，免回声。
        }
        self.sub_layout_baseline = Some(cur.clone());
        self.send_frame(&RemoteFrame::SubLayout {
            tab_id: cur.0,
            row_weights: cur.1,
            col_weights: cur.2,
        });
    }

    /// 任一端：把比例同步基线设为给定值（main 应用完对端 [`RemoteFrame::SubLayout`] 后调用，
    /// 使本端变更检测不再把「应用对端比例」当本地改动回发）。
    pub fn note_sub_layout_baseline(
        &mut self,
        tab_id: TabId,
        row_weights: Vec<f32>,
        col_weights: Vec<Vec<f32>>,
    ) {
        self.sub_layout_baseline = Some((tab_id, row_weights, col_weights));
    }

    /// 控制端：复位比例同步基线（换订阅/结构变时调用，使下一帧据当前比例重新建立同步）。
    pub fn reset_sub_layout_baseline(&mut self) {
        self.sub_layout_baseline = None;
    }

    /// 控制端：订阅会话已不在远程列表（被关）→ **按位置回退到邻位**（右邻顶上原位、无右邻取
    /// 末位，与被控端 `close_tab` 切邻位一致；D6 按排序非 HashMap 迭代序），列表空则清订阅 + 清镜像。
    /// 仍在列表则无操作。`old_idx` = 被订阅会话在**变更前**列表中的下标（调用方在 mutate 前算好）。
    fn fallback_subscription(&mut self, old_idx: Option<usize>) {
        let Some(sub) = self.subscribed_tab else {
            return;
        };
        if self.remote_tabs.iter().any(|t| t.id == sub) {
            return;
        }
        // 删除会令后继会话左移一位，故「旧下标」处现在正是右邻；越界则取末位。
        let target = old_idx
            .filter(|_| !self.remote_tabs.is_empty())
            .map(|i| self.remote_tabs[i.min(self.remote_tabs.len() - 1)].id)
            .or_else(|| self.remote_tabs.first().map(|t| t.id));
        match target {
            Some(id) => self.subscribe_tab(id),
            None => {
                self.subscribed_tab = None;
                self.mirror_focus_sid = None;
                self.mirror = None;
                self.mirror_panes.clear();
                self.mirror_layout = None;
                self.mirror_active_pane = None;
        self.mirror_pane_selecting = None;
                self.reset_history();
            }
        }
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
        // M6：排空 P2P 引擎事件（发信令 → send_frame / 直连建立提示）。
        let p2p_events = self.p2p.as_ref().map(P2pEngine::poll).unwrap_or_default();
        let p2p_changed = !p2p_events.is_empty();
        for ev in p2p_events {
            self.apply_p2p_event(ev);
        }
        changed || p2p_changed
    }

    /// 处理一条 P2P 引擎事件：发信令（转 `send_frame`→中继盲转）/ 连接建立 / 数据面切换 / 入站数据帧。
    fn apply_p2p_event(&mut self, ev: P2pEvent) {
        match ev {
            P2pEvent::SendSignal { kind, payload } => match serde_json::to_string(&payload) {
                // 信令必走中继（直连断时才能协商回退）；send_frame 内对 P2pSignal 强制走中继。
                Ok(s) => self.send_frame(&RemoteFrame::P2pSignal { kind, payload: s }),
                Err(e) => log::error!("P2pSignal payload 序列化失败: {e}"),
            },
            P2pEvent::Connected => log::info!("P2P 直连 QUIC 连接已建立"),
            P2pEvent::DataPlaneUp => {
                if !self.p2p_data_active {
                    self.p2p_data_active = true;
                    log::info!("P2P 数据面已切到直连");
                    self.notices.push(Notice::P2pDirect);
                    self.resubscribe_after_switch();
                }
            }
            P2pEvent::DataPlaneDown => {
                if self.p2p_data_active {
                    self.p2p_data_active = false;
                    log::info!("P2P 数据面已回退中继");
                    self.notices.push(Notice::P2pRelay);
                    self.resubscribe_after_switch();
                }
            }
            // 经 QUIC 直连收到的数据面帧：与中继帧汇入同一状态机入口（上层零改动）。
            P2pEvent::DataFrame(v) => self.apply_relay(&v),
        }
    }

    /// 切换（直连↔中继）后控制端补发一次订阅，触发被控端重发整屏快照重建镜像——消除切换瞬间可能的
    /// VT 错位/丢失（被控端 `sub_dirty` 即使重订同一会话也强制重发 `SubscriptionStarted`）。
    ///
    /// **不走 `subscribe_tab`**（那会 `clear_remote_filetree` 清空文件树→闪「等待 shell 上报路径」）：
    /// 同会话 cwd 未变，被控端重推的 RootChanged 经 `set_root` 去重为 no-op，文件树保持不动。镜像由
    /// 被控端重发的 `SubscriptionStarted` 重建。被控端 `subscribed_tab` 恒为 `None`，此处自然 no-op。
    fn resubscribe_after_switch(&mut self) {
        if let Some(tab) = self.subscribed_tab {
            self.reset_history(); // 换源/重建：回看绝对行号体系复位（同 subscribe_tab）。
            self.send_frame(&RemoteFrame::SubscribeSession { tab_id: tab });
        }
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
        self.reset_multi_session();
        self.clear_remote_filetree();
    }

    // part3d（K2）：无 id 的 `send_output`/`send_resize`（旧单焦点镜像）已移除，被控端改用
    // `send_subscription_started` + `send_output_with_id`（带双 id）。控制端 `apply_relay` 仍保留
    // `Output`/`Resize` 收处理臂，但版本门已挡住会发旧帧的 v1 对端，故休眠无害。

    /// 控制端：把用户输入的 VT 字节转发给被控端（所有按键 / Ctrl+C / win32 / IME / 粘贴的收口）。
    ///
    /// **part3d Phase 4**：按 `(subscribed_tab, 焦点窗格 session_id)` 包成 [`RemoteFrame::InputWithId`]
    /// 发给被控端，被控端按双 id 路由到对应窗格 PTY（与被控端焦点解耦，需求 e）。目标窗格 = 多窗格镜像
    /// 取 `mirror_active_pane`（控制端自选），单窗格镜像取 `mirror_focus_sid`。未订阅 / 无焦点窗格则丢弃
    /// （不发，杜绝旧 part4a「落到被控端激活会话」的缺陷）。回看态下转发即 snap 回跟随底部，使用户看到回显。
    ///
    /// 返回是否真正发出：未在控制态 / 未订阅 tab / 无目标窗格 / 空字节时返回 `false`
    /// （键盘路径忽略返回值即可；文件树「进入文件夹」cd 注入据此判定能否送达、
    /// 否则给用户提示而非静默无效）。
    pub fn send_input(&mut self, bytes: &[u8]) -> bool {
        if !self.is_controlling() || bytes.is_empty() {
            return false;
        }
        let Some(tab_id) = self.subscribed_tab else {
            return false;
        };
        let target = if self.mirror_panes.is_empty() {
            self.mirror_focus_sid
        } else {
            self.mirror_active_pane
        };
        let Some(session_id) = target else {
            return false;
        };
        self.hist_top = None; // 转发输入 → 回跟随实时底部（看到回显）。
        self.send_frame(&RemoteFrame::InputWithId {
            tab_id,
            session_id,
            data: bytes.to_vec(),
        });
        true
    }

    /// 转发输入到**指定会话** `session_id`（镜像鼠标上报专用：拖动 / 释放钉在按下
    /// 时的窗格 sid，不随 `mirror_active_pane` 漂移、不夺焦点，杜绝「发错会话 /
    /// 被控端幻影按住」）。其余语义同 [`Self::send_input`]（仅控制态 + 已订阅 tab
    /// 时发出，回看态 snap 回底部）。返回是否真正发出。
    pub fn send_input_to(&mut self, session_id: SessionId, bytes: &[u8]) -> bool {
        if !self.is_controlling() || bytes.is_empty() {
            return false;
        }
        let Some(tab_id) = self.subscribed_tab else {
            return false;
        };
        self.hist_top = None;
        self.send_frame(&RemoteFrame::InputWithId {
            tab_id,
            session_id,
            data: bytes.to_vec(),
        });
        true
    }

    /// 当前镜像输入 / 鼠标上报的**目标会话** sid，与 [`Self::send_input`] 的 target
    /// 同口径：多窗格取 `mirror_active_pane`（控制端自选焦点），单窗格取
    /// `mirror_focus_sid`。镜像 hover 上报据此只对焦点镜像窗格上报。
    #[must_use]
    pub fn mirror_target_sid(&self) -> Option<SessionId> {
        if self.mirror_panes.is_empty() {
            self.mirror_focus_sid
        } else {
            self.mirror_active_pane
        }
    }

    // part3d：控制端 SSH 式视口跟随已移除（多会话模型下被控端焦点不动、订阅会话可为后台
    // tab，不强制其 resize）。被控端侧 `ViewportResize` 收处理 + `take_viewport` 暂保留休眠，
    // 留待 Phase 3/4 若需「订阅=被控端焦点 tab」时的 1:1 满屏渲染再启用。

    /// 被控端：取走待应用的远程视口尺寸（main 把焦点窗格 resize 到它）。
    pub fn take_viewport(&mut self) -> Option<(u16, u16)> {
        self.pending_viewport.take()
    }

    /// 被控端：取走待应答的历史行请求 `(目标窗格, top, count)`（main 据目标窗格序列化后应答：
    /// `None` 回 `HistoryRows`、`Some(sid)` 回 `HistoryRowsForPane`）。
    pub fn take_history_reqs(&mut self) -> Vec<(Option<SessionId>, u64, u16)> {
        std::mem::take(&mut self.pending_history)
    }

    /// 被控端：应答 per-pane 历史行请求（携 `session_id` 供控制端校验是否仍是焦点窗格回看）。
    pub fn send_history_rows_for_pane(
        &self,
        session_id: SessionId,
        top: u64,
        base: u64,
        screen_top: u64,
        lines: Vec<Vec<u8>>,
    ) {
        self.send_frame(&RemoteFrame::HistoryRowsForPane {
            session_id,
            top,
            base,
            screen_top,
            lines,
        });
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

    // ── part3d 多会话 × 多窗格镜像（Phase 1 MVP）收发 ───────────────────────────

    /// 控制端：远程会话列表（远程视图侧栏渲染源）。
    #[must_use]
    pub fn remote_tabs(&self) -> &[TabState] {
        &self.remote_tabs
    }

    /// 控制端：当前订阅查看的会话 id（`None` = 未订阅）。
    #[must_use]
    pub fn subscribed_tab(&self) -> Option<TabId> {
        self.subscribed_tab
    }

    /// 控制端：订阅查看某会话（点击远程列表项）。发 [`RemoteFrame::SubscribeSession`]，记下
    /// 订阅目标；镜像由被控端随后的 [`RemoteFrame::SubscriptionStarted`] 重建（含初始快照）。
    /// K5：新订阅隐式取代旧的。回看态复位（换源后绝对行号体系变）。
    pub fn subscribe_tab(&mut self, tab_id: TabId) {
        if !self.is_controlling() {
            return;
        }
        self.subscribed_tab = Some(tab_id);
        self.last_sub_viewport = None; // 换订阅：强制为新会话重发目标尺寸。
        self.reset_sub_layout_baseline(); // 换订阅：据新会话当前比例重建 SubLayout 双向同步。
        self.reset_history();
        self.clear_remote_filetree(); // 修③：换订阅清旧树，等被控端按新订阅会话 cwd 推 RootChanged。
        self.send_frame(&RemoteFrame::SubscribeSession { tab_id });
    }

    /// 被控端：控制端当前订阅查看的会话 id（main 据此推该会话快照 + 实时输出）。
    #[must_use]
    pub fn sub_target(&self) -> Option<TabId> {
        self.sub_target
    }

    /// 被控端：取走「订阅目标刚变化」标志（main 据此复位 `mirror_src` 强制重发快照）。
    pub fn take_sub_dirty(&mut self) -> bool {
        std::mem::take(&mut self.sub_dirty)
    }

    /// 被控端：推会话列表给控制端（K6 去重：归一化后与上次一致则不发）。会话增删 / 改名 /
    /// cwd 变 / 忙闲翻转 / 未读变 / 窗格增删都会改变 `tabs`，自然触发；高频抖动字段已在
    /// [`TabState`] 构造侧归一化（`busy` 是布尔判定、不含 spinner 字形），不刷链路。
    pub fn push_tab_list(&mut self, tabs: Vec<TabState>) {
        if tabs == self.last_tab_states {
            return;
        }
        self.last_tab_states = tabs.clone();
        self.send_frame(&RemoteFrame::TabListSnapshot { tabs });
    }

    /// 被控端：发订阅会话的初始整屏快照（先于任何 [`RemoteFrame::OutputWithId`]，D3 保序）。
    /// Phase 3 起回**全部窗格 + 布局**（`row_weights`/`col_weights`/`maximized`），控制端据此复刻
    /// 多窗格几何。`panes` 渲染顺序 = 下标，`focused` 为焦点窗格下标。
    #[allow(clippy::too_many_arguments)]
    pub fn send_subscription_started(
        &self,
        tab_id: TabId,
        focused: u32,
        panes: Vec<PaneSnapshot>,
        row_weights: Vec<f32>,
        col_weights: Vec<Vec<f32>>,
        maximized: Option<u32>,
    ) {
        self.send_frame(&RemoteFrame::SubscriptionStarted {
            tab_id,
            focused,
            panes,
            row_weights,
            col_weights,
            maximized,
        });
    }

    /// 被控端：转发订阅会话某窗格的实时 PTY 输出（带双 id；替代无 id 的 `Output`）。
    pub fn send_output_with_id(&self, tab_id: TabId, session_id: SessionId, bytes: &[u8]) {
        self.send_frame(&RemoteFrame::OutputWithId {
            tab_id,
            session_id,
            data: bytes.to_vec(),
        });
    }

    // 被控端 `send_resize_with_id`（带双 id 增量 resize）留待 Phase 3 per-pane 路由再加：
    // MVP 单 mirror 下被控端订阅窗格 resize 即触发 mirror_src 变 → 整屏 SubscriptionStarted
    // 重发（含新尺寸快照），无需独立 ResizeWithId。控制端 apply_relay 的 ResizeWithId 臂已就绪。

    // ── part3d Phase 2 远程增删会话（需求 d）收发 ──────────────────────────────

    /// 控制端：请被控端新建一个会话(tab)（侧栏「＋」）。被控端回 [`RemoteFrame::NewTabResult`]，
    /// 成功则控制端自动订阅新会话（见 apply_relay）。
    pub fn new_remote_tab(&mut self) {
        if !self.is_controlling() {
            return;
        }
        let req_id = self.next_req_id();
        self.send_frame(&RemoteFrame::NewTab { req_id });
    }

    /// 控制端：请被控端关闭指定会话(tab)（远程列表右键「关闭」）。被控端回
    /// [`RemoteFrame::CloseTabResult`]，列表 / 订阅回退由后续 `TabListSnapshot` 驱动。
    pub fn close_remote_tab(&mut self, tab_id: TabId) {
        if !self.is_controlling() {
            return;
        }
        let req_id = self.next_req_id();
        self.send_frame(&RemoteFrame::CloseTab { req_id, tab_id });
    }

    /// 被控端：取走待执行的远程新建会话请求（main spawn 新 tab 后回 `NewTabResult`）。
    pub fn take_new_tab_reqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_new_tab)
    }

    /// 被控端：取走待执行的远程关闭会话请求（main 关 tab 后回 `CloseTabResult`）。
    pub fn take_close_tab_reqs(&mut self) -> Vec<(u64, TabId)> {
        std::mem::take(&mut self.pending_close_tab)
    }

    /// 被控端：回 [`RemoteFrame::NewTabResult`]（`err` 优先于 `tab_id`）。
    pub fn send_new_tab_result(&self, req_id: u64, tab_id: Option<TabId>, err: Option<RemoteOpErr>) {
        self.send_frame(&RemoteFrame::NewTabResult {
            req_id,
            tab_id,
            err,
        });
    }

    /// 被控端：回 [`RemoteFrame::CloseTabResult`]（`err=None` 即已关）。
    pub fn send_close_tab_result(&self, req_id: u64, tab_id: TabId, err: Option<RemoteOpErr>) {
        self.send_frame(&RemoteFrame::CloseTabResult {
            req_id,
            tab_id,
            err,
        });
    }

    // ── part3c-2 Option B 目录树收发 ───────────────────────────────────────

    /// 控制端：分配下一个请求号（单调递增，跳 0 哨兵；ListDir/Fetch/Put 全局共用）。
    fn next_req_id(&mut self) -> u64 {
        self.req_seq = self.req_seq.wrapping_add(1);
        if self.req_seq == 0 {
            self.req_seq = 1;
        }
        self.req_seq
    }

    /// 被控端：焦点窗格 cwd 变化即推 [`RemoteFrame::RootChanged`]（去重：同 cwd 不重发）。
    /// 控制端据此重置远程树根。替代 Option A 的整树快照推送。
    pub fn send_root_changed(&mut self, path: String) {
        if self.remote_root_sent.as_deref() == Some(path.as_str()) {
            return;
        }
        self.remote_root_sent = Some(path.clone());
        self.send_frame(&RemoteFrame::RootChanged { path });
    }

    /// 控制端：当前 Option B 远程树状态（远程视图侧栏渲染源）。
    #[must_use]
    pub fn remote_filetree(&self) -> Option<&RemoteFileTree> {
        self.remote_filetree.as_ref()
    }

    /// 控制端：单击选中远程树节点（渲染高亮；Ctrl+C 据此复制下载源）。
    pub fn set_remote_selected(&mut self, id: usize) {
        if let Some(ft) = self.remote_filetree.as_mut() {
            ft.set_selected(id);
        }
    }


    /// 控制端：用户点击远程树某目录行 → 翻转展开态（纯本地，不发帧 → 修 #6）；若新展开
    /// 且该目录尚未缓存 / 在途，则发 [`RemoteFrame::ListDir`] 按需拉取（修 #2/#3/#4）。
    pub fn remote_dir_clicked(&mut self, id: usize) {
        // 先在受限作用域内翻转展开态、取出「需发 ListDir」的 (path, show_hidden)，释放 ft 借用。
        let need = {
            let Some(ft) = self.remote_filetree.as_mut() else {
                return;
            };
            if !ft.node_is_dir(id) {
                return;
            }
            let now_open = !ft.is_open(id);
            ft.set_open(id, now_open);
            if now_open {
                if !ft.has_listing(id) && !ft.is_pending(id) {
                    let show_hidden = ft.show_hidden();
                    ft.node_path(id).map(|p| (p.to_owned(), show_hidden))
                } else {
                    None
                }
            } else {
                // 折叠一个仍在途且未缓存的目录（应答可能因会话切换 / 通道关闭永不达）：清
                // 其 pending，使再次展开能重新发 ListDir（否则 is_pending 恒真、永卡加载中）。
                if ft.is_pending(id) && !ft.has_listing(id) {
                    ft.clear_pending(id);
                }
                None
            }
        };
        if let Some((path, show_hidden)) = need {
            let req_id = self.next_req_id();
            if let Some(ft) = self.remote_filetree.as_mut() {
                ft.mark_pending(id, req_id);
            }
            self.send_frame(&RemoteFrame::ListDir {
                req_id,
                path,
                show_hidden,
            });
        }
    }

    /// 控制端：按不透明路径刷新远程目录（上传完成后刷新被控端目标目录，使新文件可见）。沿
    /// `listings` 找到该路径的 dir 节点即作废重拉；找不到（未展开/已换根）则静默忽略。
    pub fn refresh_remote_path(&mut self, path: &str) {
        let id = self.remote_filetree.as_ref().and_then(|ft| ft.find_dir(path));
        if let Some(id) = id {
            self.remote_refresh_dir(id);
        }
    }

    /// 控制端：远程菜单「新建文件夹」——在被控端 `dir` 下建 `name`（复用 MkDir 协议）。登记
    /// `inflight_remote_fsop` 以便结果回来刷新 `dir`。需控制中 + 名非空。
    pub fn remote_make_dir(&mut self, dir: String, name: String) {
        if !self.is_controlling() || name.trim().is_empty() {
            return;
        }
        let req_id = self.next_req_id();
        self.inflight_remote_fsop.insert(req_id, dir.clone());
        self.send_frame(&RemoteFrame::MkDir { req_id, dir, name });
    }

    /// 控制端：远程菜单「新建文件」——在被控端 `dir` 下建空文件 `name`（MkFile 协议）。
    pub fn remote_make_file(&mut self, dir: String, name: String) {
        if !self.is_controlling() || name.trim().is_empty() {
            return;
        }
        let req_id = self.next_req_id();
        self.inflight_remote_fsop.insert(req_id, dir.clone());
        self.send_frame(&RemoteFrame::MkFile { req_id, dir, name });
    }

    /// 控制端：远程菜单「删除」——删被控端 `path`（`is_dir` 递归）。完成后刷新其**父目录**。
    pub fn remote_delete(&mut self, path: String, is_dir: bool) {
        if !self.is_controlling() || path.trim().is_empty() {
            return;
        }
        let req_id = self.next_req_id();
        self.inflight_remote_fsop.insert(req_id, parent_remote_dir(&path));
        self.send_frame(&RemoteFrame::Delete {
            req_id,
            path,
            is_dir,
        });
    }

    /// 控制端：单次远程文件操作（新建文件夹/文件、删除）完成——刷新登记的目录（变更立即反映）；
    /// 出错记日志（刷新后界面即反映真实结果：新项未出现 / 被删项仍在）。
    fn finish_remote_fsop(&mut self, req_id: u64, err: Option<FsErr>) {
        let Some(dir) = self.inflight_remote_fsop.remove(&req_id) else {
            return;
        };
        if let Some(e) = err {
            log::warn!("远程文件操作失败 req={req_id}: {e:?}");
        }
        self.refresh_remote_path(&dir);
    }

    /// 控制端：用户点目录行的「刷新」图标 → 作废该目录缓存、保持展开、重发 ListDir 拉最新内容
    /// （被控端新增 / 删除文件后，控制端已缓存的目录可借此刷新；问题 #6）。
    pub fn remote_refresh_dir(&mut self, id: usize) {
        let need = {
            let Some(ft) = self.remote_filetree.as_mut() else {
                return;
            };
            if !ft.node_is_dir(id) {
                return;
            }
            ft.clear_listing(id); // 删旧缓存 + 在途
            ft.set_open(id, true); // 保持展开以便看到刷新结果
            let show_hidden = ft.show_hidden();
            ft.node_path(id).map(|p| (p.to_owned(), show_hidden))
        };
        if let Some((path, show_hidden)) = need {
            let req_id = self.next_req_id();
            if let Some(ft) = self.remote_filetree.as_mut() {
                ft.mark_pending(id, req_id);
            }
            self.send_frame(&RemoteFrame::ListDir {
                req_id,
                path,
                show_hidden,
            });
        }
    }

    /// 控制端：切「显示隐藏项」开关。变化即重列（折叠回根 + 清缓存）并重发根 ListDir。
    pub fn set_remote_show_hidden(&mut self, show: bool) {
        let changed = self
            .remote_filetree
            .as_mut()
            .is_some_and(|ft| ft.set_show_hidden(show));
        if changed {
            self.request_root_listing();
        }
    }

    /// 控制端：为当前树根（id 0）发 ListDir（换根 / 切隐藏项后；已缓存 / 在途则跳过）。
    fn request_root_listing(&mut self) {
        let need = self.remote_filetree.as_ref().and_then(|ft| {
            if ft.has_listing(0) || ft.is_pending(0) {
                return None;
            }
            ft.node_path(0).map(|p| (p.to_owned(), ft.show_hidden()))
        });
        let Some((path, show_hidden)) = need else {
            return;
        };
        let req_id = self.next_req_id();
        if let Some(ft) = self.remote_filetree.as_mut() {
            ft.mark_pending(0, req_id);
        }
        self.send_frame(&RemoteFrame::ListDir {
            req_id,
            path,
            show_hidden,
        });
    }

    /// 被控端：取走待处理的 ListDir 请求（main 后台读盘服务）。
    pub fn take_listdir_reqs(&mut self) -> Vec<(u64, String, bool)> {
        std::mem::take(&mut self.pending_listdir)
    }

    /// 被控端：取走待处理的 ListDirRecursive 请求（片8 目录递归；main 后台递归读盘服务）。
    pub fn take_listdir_recursive_reqs(&mut self) -> Vec<(u64, String, bool)> {
        std::mem::take(&mut self.pending_listdir_recursive)
    }

    /// 被控端：把单层读目录请求投入有界服务队列（绝不在主循环同步 IO——慢速网络盘会冻结整个
    /// 应用），worker 读完经 `svc_tx` 回主线程，由 [`Self::drain_service`] 发回控制端。
    ///
    /// review MED-1：已由「每请求起一线程、无上限」收敛为「常驻 worker 池 + 有界队列」
    /// （见 [`Self::start`]）。队列满即回 `FsErr::Io`（控制端不空挂）。正常负载下控制端的
    /// `has_listing`/`is_pending` 闸使每目录至多一次在途，远不及上限；威胁模型上控制端本就是配对
    /// 鉴权方（可在被控端跑任意命令），队列仅防慢盘大量展开堆积，不构成提权阻断。
    pub fn spawn_list_dir(&self, req_id: u64, path: String, show_hidden: bool) {
        self.enqueue_dir_job(
            DirJob::List {
                req_id,
                path: path.clone(),
                show_hidden,
            },
            req_id,
            path,
            false,
        );
    }

    /// 被控端：把片8 递归读目录树请求投入有界服务队列，worker 读完经 `svc_tx` 回主线程发回控制端。
    /// 同 [`Self::spawn_list_dir`] 不阻塞主循环；worker 内先探根可读性（失败回 `err`），再 DFS 枚举，
    /// 5s deadline 防慢盘长阻塞、超上限截断。
    pub fn spawn_list_dir_recursive(&self, req_id: u64, path: String, show_hidden: bool) {
        self.enqueue_dir_job(
            DirJob::Recursive {
                req_id,
                path: path.clone(),
                show_hidden,
            },
            req_id,
            path,
            true,
        );
    }

    /// 被控端：把读目录任务投入有界队列（`try_send` 非阻塞——绝不阻塞主线程）。队列满 / 通道断时
    /// 立即回 `FsErr::Io`（按 `recursive` 选回包类型），让控制端清 `is_pending`、显式失败，而非空挂。
    fn enqueue_dir_job(&self, job: DirJob, req_id: u64, path: String, recursive: bool) {
        let Some(tx) = self.dir_job_tx.as_ref() else {
            return; // 会话未起 / 已停：静默（控制端会因断连整体复位）。
        };
        // try_send 非阻塞：队列满（worker 全卡在慢盘）/ 通道断即回 err，绝不阻塞主线程。
        if tx.try_send(job).is_err() {
            log::warn!("被控端读目录队列已满，拒绝 req_id={req_id} path={path}");
            // 经 svc_tx 回一条 err（drain_service 下帧发回控制端），让控制端清 is_pending、显式失败。
            if let Some(svc) = self.svc_tx.as_ref() {
                let reply = if recursive {
                    SvcReply::ListDirRecursive {
                        req_id,
                        path,
                        entries: Vec::new(),
                        truncated: false,
                        err: Some(FsErr::Io),
                    }
                } else {
                    SvcReply::ListDir {
                        req_id,
                        path,
                        entries: Vec::new(),
                        overflow: 0,
                        err: Some(FsErr::Io),
                    }
                };
                let _ = svc.send(reply);
            }
        }
    }

    /// 被控端：排空文件服务后台回包（main 每帧 `pump_remote` 调）。先收齐释放 `svc_rx` 借用，
    /// 再处理（ListDir 发回控制端、FetchSrcDone 清 `inflight_fetch_src`）。
    pub fn drain_service(&mut self) {
        let mut replies = Vec::new();
        if let Some(rx) = self.svc_rx.as_ref() {
            while let Ok(r) = rx.try_recv() {
                replies.push(r);
            }
        }
        for reply in replies {
            match reply {
                SvcReply::ListDir {
                    req_id,
                    path,
                    entries,
                    overflow,
                    err,
                } => self.send_frame(&RemoteFrame::ListDirResult {
                    req_id,
                    path,
                    entries,
                    overflow,
                    err,
                }),
                SvcReply::ListDirRecursive {
                    req_id,
                    path,
                    entries,
                    truncated,
                    err,
                } => self.send_frame(&RemoteFrame::ListDirRecursiveResult {
                    req_id,
                    path,
                    entries,
                    truncated,
                    err,
                }),
                SvcReply::FetchSrcDone { req_id } => {
                    self.inflight_fetch_src.remove(&req_id);
                }
                SvcReply::PutSrcDone { req_id } => {
                    self.inflight_put_src.remove(&req_id);
                }
                SvcReply::PutSrcFailed { req_id } => {
                    // H2：worker open/read 失败、被控端不会回 PutResult → 主线程自行收尾，
                    // 否则 active_puts 永不归零、上传挂死。
                    self.inflight_put_src.remove(&req_id);
                    self.inflight_put_meta.remove(&req_id);
                    if let Some(u) = self.upload.as_mut() {
                        u.errors += 1;
                        u.active_puts = u.active_puts.saturating_sub(1);
                    }
                    self.pump_upload();
                }
            }
        }
    }

    // ── part3c-2 文件读取（Fetch）：#5 打开 / 片4 下载源端读取 ────────────────

    /// 控制端：双击远程文件 → 起一个 Fetch，传完用本地默认程序打开（#5）。同一文件已在途
    /// 则跳过（防连点起多份临时文件 + 多次拉起本地程序）。
    pub fn start_fetch_open(&mut self, path: String) {
        // 去重仅看「打开」用途的在途 Fetch（同文件正下载时仍允许双击打开）。
        if self
            .inflight_fetch
            .values()
            .any(|j| matches!(j.kind, FetchKind::Open) && j.src_path == path)
        {
            return;
        }
        let req_id = self.next_req_id();
        let name = last_path_segment(&path);
        self.inflight_fetch.insert(
            req_id,
            FetchJob {
                kind: FetchKind::Open,
                src_path: path.clone(),
                name,
                dest: None,
                file: None,
                next_seq: 0,
                written: 0,
                total: 0,
                last_at: Instant::now(),
                clip_stream: None,
            },
        );
        self.notices.push(Notice::FetchStarted);
        self.send_frame(&RemoteFrame::FetchReq { req_id, path });
    }

    /// 片6 虚拟文件剪贴板：资源管理器读虚拟文件 → OLE 线程经 [`ClipFetchCmd`] 请主线程起一次
    /// [`FetchKind::Clipboard`] 流式拉取，分块经 `data_tx` 边下边喂给 OLE 线程的 IStream。
    /// 会话不在控制中（已断开）直接喂 [`StreamMsg::Failed`]，避免 OLE 线程空等（其侧另有超时兜底）。
    pub fn start_clip_fetch(&mut self, path: String, data_tx: Sender<StreamMsg>) {
        if !self.is_controlling() {
            let _ = data_tx.send(StreamMsg::Failed);
            return;
        }
        let req_id = self.next_req_id();
        self.inflight_fetch.insert(
            req_id,
            FetchJob {
                kind: FetchKind::Clipboard,
                src_path: path.clone(),
                name: String::new(),
                dest: None,
                file: None,
                next_seq: 0,
                written: 0,
                total: 0,
                last_at: Instant::now(),
                clip_stream: Some(data_tx),
            },
        );
        self.send_frame(&RemoteFrame::FetchReq { req_id, path });
    }

    /// 片8：控制端复制远程目录 → 发 [`RemoteFrame::ListDirRecursive`] 请被控端递归枚举整棵子树，
    /// 记 `inflight_clip_dir` 供应答去重。**调用方（main）须先 `clipboard_svc.clear()`** 关竞态窗口
    /// （枚举返回前剪贴板为空，立即 Ctrl+V 不会粘到上一次的旧虚拟文件）。会话不在控制中直接忽略。
    pub fn start_clip_dir(&mut self, root_path: String, root_name: String) {
        if !self.is_controlling() {
            return;
        }
        // 递归与单层浏览取同一 `show_hidden`，列出项一致（对抗审查 M6）。
        let show_hidden = self
            .remote_filetree
            .as_ref()
            .is_some_and(RemoteFileTree::show_hidden);
        let req_id = self.next_req_id();
        self.inflight_clip_dir = Some((req_id, root_name));
        self.clip_dir_ready = None;
        log::debug!("[片8] 控制端发 ListDirRecursive req={req_id} path={root_path}");
        self.send_frame(&RemoteFrame::ListDirRecursive {
            req_id,
            path: root_path,
            show_hidden,
            max_depth: LIST_DIR_RECURSIVE_MAX_DEPTH,
            max_entries: LIST_DIR_RECURSIVE_MAX_ENTRIES,
        });
    }

    /// 片8：作废在途的「复制远程目录」枚举（控制端改复制单文件 / 本地项时调，防陈旧目录应答
    /// 晚到覆盖刚设的剪贴板——对抗审查 M1）。
    pub fn cancel_clip_dir(&mut self) {
        self.inflight_clip_dir = None;
        self.clip_dir_ready = None;
    }

    /// 片8：控制端收 [`RemoteFrame::ListDirRecursiveResult`]。req_id 去重（陈旧 / 已被新复制动作
    /// 作废的应答丢弃）；成功把 entries 暂存 `clip_dir_ready`（main 取走调 `set_remote_dir`）并 push
    /// [`Notice::ClipDirReady`]；失败 / 空树 push [`Notice::ClipDirFailed`]（剪贴板保持 main 先前 clear
    /// 的空态，不粘出旧文件）。
    fn apply_list_dir_recursive(
        &mut self,
        req_id: u64,
        root_path: String,
        entries: Vec<RecursiveDirEntry>,
        truncated: bool,
        err: Option<FsErr>,
    ) {
        // req_id 去重；同时取出根目录显示名（descriptor 顶层前缀）。
        let root_name = match &self.inflight_clip_dir {
            Some((id, name)) if *id == req_id => name.clone(),
            _ => return, // 陈旧 / 已作废
        };
        self.inflight_clip_dir = None;
        log::debug!(
            "[片8] 控制端收枚举应答 req={req_id} root={root_name} entries={} truncated={truncated} err={err:?}",
            entries.len()
        );
        if let Some(e) = err {
            log::warn!("复制远程目录枚举失败: {e:?}");
            self.notices.push(Notice::ClipDirFailed);
            return;
        }
        // 给整棵子树套「根目录名」顶层前缀，并在最前插入根目录项——使资源管理器粘贴出
        // 「目标\根目录名\…」（含顶层文件夹本身），而非把子项散落到目标根。空目录树也借此粘出
        // 一个空的根文件夹。父目录项在子项之前的不变量仍成立（顶层项在最前、DFS 序不变）。
        let mut prefixed = Vec::with_capacity(entries.len() + 1);
        prefixed.push(RecursiveDirEntry {
            rel_path: root_name.clone(),
            path: root_path,
            is_dir: true,
            size: 0,
        });
        for mut e in entries {
            e.rel_path = format!("{root_name}/{}", e.rel_path);
            prefixed.push(e);
        }
        let count = prefixed.len();
        self.clip_dir_ready = Some(prefixed);
        self.notices.push(Notice::ClipDirReady { count, truncated });
    }

    /// 片8：main 取走递归枚举好的子树清单（控制端）去调 `clipboard_svc.set_remote_dir` 构造虚拟文件。
    pub fn take_clip_dir_ready(&mut self) -> Option<Vec<RecursiveDirEntry>> {
        self.clip_dir_ready.take()
    }

    /// 控制端：收 `FileBegin`。Open=临时目录 `{req_id}-名`；Download=目标路径（先 `create_dir_all`
    /// 父目录）；Clipboard 流式**不落盘**，仅重置连续性计数。`total_len` 存入 job 供状态栏进度
    /// 展示（仅展示，finalize 仍以 `FileEnd` 为准）。
    fn fetch_begin(&mut self, req_id: u64, total_len: u64) {
        let (kind, name, dest_opt) = {
            let Some(job) = self.inflight_fetch.get(&req_id) else {
                return;
            };
            (job.kind, job.name.clone(), job.dest.clone())
        };
        // 片6 流式：不落盘，仅重置计数（防异常重发 FileBegin 后误判乱序）。
        if matches!(kind, FetchKind::Clipboard) {
            if let Some(job) = self.inflight_fetch.get_mut(&req_id) {
                job.next_seq = 0;
                job.written = 0;
                job.total = total_len;
                job.last_at = Instant::now();
            }
            return;
        }
        let target = match kind {
            FetchKind::Open => {
                let dir = std::env::temp_dir().join("lumen_remote_open");
                let _ = std::fs::create_dir_all(&dir);
                dir.join(format!("{req_id}-{}", sanitize_basename(&name)))
            }
            FetchKind::Download => {
                let Some(d) = dest_opt else {
                    self.fetch_abort(req_id, FsErr::Io);
                    return;
                };
                if let Some(parent) = d.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                d
            }
            // 上面已 return。
            FetchKind::Clipboard => return,
        };
        match std::fs::File::create(&target) {
            Ok(f) => {
                if let Some(job) = self.inflight_fetch.get_mut(&req_id) {
                    job.file = Some(f);
                    job.dest = Some(target);
                    job.next_seq = 0; // 重置连续性计数（防异常重发 FileBegin 后误判乱序）。
                    job.written = 0;
                    job.total = total_len;
                    job.last_at = Instant::now();
                }
            }
            Err(e) => {
                log::error!("创建落地文件失败 {}: {e}", target.display());
                // 经 fetch_abort 统一收口：移除在途 + 发 FetchCancel + 打开失败弹提示 / 下载计错。
                self.fetch_abort(req_id, io_err_to_fs(&e));
            }
        }
    }

    /// 控制端：收 `FileChunk` → 连续性校验 + 累计字节硬上限后落地（Open/Download 顺序写文件、
    /// Clipboard 喂 OLE 线程 IStream），回 `FileChunkAck`（背压）。`Some(err)` = 中止原因
    /// （乱序 / 写失败 / 流被关 = `Io`，超上限 = `TooLarge`）。
    fn fetch_chunk(&mut self, req_id: u64, seq: u32, data: &[u8]) {
        let abort: Option<FsErr> = {
            let Some(job) = self.inflight_fetch.get_mut(&req_id) else {
                return;
            };
            if seq != job.next_seq {
                log::warn!("Fetch 块乱序 req={req_id} seq={seq} 期望={}", job.next_seq);
                Some(FsErr::Io)
            } else {
                let put: Result<(), FsErr> = match job.kind {
                    // 流被关 = 资源管理器取消粘贴 → Io 中止（下方发 FetchCancel 回收源 worker）。
                    FetchKind::Clipboard => match job.clip_stream.as_ref() {
                        Some(tx) => tx
                            .send(StreamMsg::Chunk(data.to_vec()))
                            .map_err(|_| FsErr::Io),
                        None => Err(FsErr::Io),
                    },
                    FetchKind::Open | FetchKind::Download => match job.file.as_mut() {
                        Some(file) => file.write_all(data).map_err(|e| {
                            log::error!("写临时文件失败: {e}");
                            FsErr::Io
                        }),
                        None => return, // FileBegin 失败/未到：忽略（清理已在别处）。
                    },
                };
                match put {
                    Err(e) => Some(e),
                    Ok(()) => {
                        job.next_seq = job.next_seq.wrapping_add(1);
                        job.written = job
                            .written
                            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                        job.last_at = Instant::now();
                        // 控制端硬上限：不轻信被控端的 FETCH_MAX_LEN（其 metadata 可能失败兜底为 0）。
                        if job.written > FETCH_MAX_LEN {
                            Some(FsErr::TooLarge)
                        } else {
                            None
                        }
                    }
                }
            }
        };
        if let Some(err) = abort {
            self.fetch_abort(req_id, err);
        } else {
            self.send_frame(&RemoteFrame::FileChunkAck { req_id, seq });
        }
    }

    /// 控制端：收 `FileEnd`。Open → 系统默认程序打开（#5）；Download → 计完成 + 推进队列（#7）；
    /// Clipboard → 给 OLE 线程 IStream 发 `Done`（EOF，资源管理器据此完成落地完整文件）。
    fn fetch_end(&mut self, req_id: u64) {
        let Some(mut job) = self.inflight_fetch.remove(&req_id) else {
            return;
        };
        match job.kind {
            FetchKind::Clipboard => {
                if let Some(tx) = job.clip_stream.take() {
                    let _ = tx.send(StreamMsg::Done);
                }
            }
            FetchKind::Open => {
                let dest = job.dest.take();
                if let Some(mut file) = job.file.take() {
                    let _ = file.flush();
                    drop(file); // 关句柄后再交系统程序（Windows 打开前须释放写锁）。
                    if let Some(d) = dest {
                        crate::shell::filetree::open_with_default(&d);
                    }
                }
                // 新副本已落地：LRU 淘汰临时夹超额旧文件（刚打开的最新，正常不会被淘汰）。
                enforce_remote_open_cap(REMOTE_OPEN_DIR_CAP);
            }
            FetchKind::Download => {
                let dest = job.dest.take();
                if let Some(mut file) = job.file.take() {
                    let _ = file.flush();
                    drop(file);
                    self.download_file_done(false);
                } else {
                    // FileBegin 未成功就 End（异常）：清理半成品 + 计错。
                    if let Some(d) = dest {
                        let _ = std::fs::remove_file(d);
                    }
                    self.download_file_done(true);
                }
            }
        }
    }

    /// 控制端：中止一个在途 Fetch（乱序 / 写失败 / 超上限 / 对端 `FileErr` / 停滞超时 / 建文件
    /// 失败）：关句柄、删半成品文件，发 `FetchCancel` 让被控端即时回收源 worker。Open 弹失败
    /// 提示；Download 计错 + 推进队列（不逐文件弹 toast，结束汇总）。
    fn fetch_abort(&mut self, req_id: u64, err: FsErr) {
        let job = self.inflight_fetch.remove(&req_id);
        let kind = job.as_ref().map(|j| j.kind);
        if let Some(mut job) = job {
            // 片6：通知 OLE 线程 IStream 失败（资源管理器粘贴报错、不落不完整文件）。
            if let Some(tx) = job.clip_stream.take() {
                let _ = tx.send(StreamMsg::Failed);
            }
            job.file = None; // 关句柄（Windows 删前须释放）。
            if let Some(d) = job.dest.take() {
                let _ = std::fs::remove_file(&d);
            }
        }
        self.send_frame(&RemoteFrame::FetchCancel { req_id });
        match kind {
            Some(FetchKind::Download) => self.download_file_done(true),
            // Open 失败弹提示；Clipboard 已发 Failed；未知 req（已移除/幂等）：静默。
            Some(FetchKind::Open) => self.notices.push(Notice::FetchFailed(err)),
            Some(FetchKind::Clipboard) | None => {}
        }
    }

    /// 控制端：聚合当前活跃文件传输供状态栏展示（main 每帧调）。无活跃传输返回 `None`
    /// （状态栏照常显示 cwd）。下载（含双击打开）汇聚字节进度；上传仅计数；剪贴板流式不计入
    /// （进度在资源管理器 IStream 侧）。
    #[must_use]
    pub fn transfer_status(&self) -> Option<TransferStatus> {
        let mut downloads = 0usize;
        let mut down_done = 0u64;
        let mut down_total = 0u64;
        let mut names: Vec<String> = Vec::new();
        for job in self.inflight_fetch.values() {
            if matches!(job.kind, FetchKind::Clipboard) {
                continue; // 剪贴板流不落盘、进度在 OLE 侧，不计入状态栏。
            }
            downloads += 1;
            down_done = down_done.saturating_add(job.written);
            down_total = down_total.saturating_add(job.total);
            if !job.name.is_empty() {
                names.push(job.name.clone());
            }
        }
        // 上传计数取活跃发送方（inflight_put_src）；名字经同 req_id 的 put_meta 取，与计数一致。
        let uploads = self.inflight_put_src.len();
        for req_id in self.inflight_put_src.keys() {
            if let Some(meta) = self.inflight_put_meta.get(req_id) {
                if !meta.name.is_empty() {
                    names.push(meta.name.clone());
                }
            }
        }
        if downloads + uploads == 0 {
            return None;
        }
        Some(TransferStatus {
            downloads,
            uploads,
            down_done,
            down_total,
            names,
        })
    }

    /// 当前会话数据面链路状态（状态栏持久指示用）。仅在 P2P 引擎存在（活跃远程会话且已启 P2P）
    /// 时返回 `Some`：`p2p_data_active` → [`P2pLink::Direct`]，否则 [`P2pLink::Relay`]；无 P2P
    /// 会话返回 `None`（状态栏不显示链路指示）。两端各自维护，控被两端均可显示。
    #[must_use]
    pub fn p2p_link_state(&self) -> Option<P2pLink> {
        self.p2p.as_ref()?;
        Some(if self.p2p_data_active {
            P2pLink::Direct
        } else {
            P2pLink::Relay
        })
    }

    /// 控制端：清理停滞（超 [`FETCH_STALL_TIMEOUT`] 没收到新块）的在途 Fetch。
    pub fn sweep_fetch_stalls(&mut self) {
        let stalled: Vec<u64> = self
            .inflight_fetch
            .iter()
            .filter(|(_, j)| j.last_at.elapsed() > FETCH_STALL_TIMEOUT)
            .map(|(&id, _)| id)
            .collect();
        for id in stalled {
            log::warn!("Fetch req={id} 停滞超时，中止清理");
            self.fetch_abort(id, FsErr::Io);
        }
    }

    // ── part3c-2 #7 下载编排（远程 → 本地，复制粘贴递归）────────────────────

    /// 设置文件剪贴板（复制：记引用，零传输）。空项忽略。
    pub fn set_file_clipboard(&mut self, side: ClipSide, items: Vec<ClipItem>) {
        if items.is_empty() {
            return;
        }
        self.file_clipboard = Some(FileClipboard { side, items });
    }

    /// 当前文件剪贴板（粘贴时按来源 / 目标侧决定方向）。
    #[must_use]
    pub fn file_clipboard(&self) -> Option<&FileClipboard> {
        self.file_clipboard.as_ref()
    }

    /// 清空 Lumen 内部文件剪贴板。本地文件改走系统剪贴板（CF_HDROP）后，复制本地文件时调用，
    /// 清掉可能残留的远程项，避免随后「粘贴到本地」误判为下载。
    pub fn clear_file_clipboard(&mut self) {
        self.file_clipboard = None;
    }

    /// 控制端：开始把剪贴板里的远程项下载到本地 `dest_dir`（递归）。`overwrite`：撞名覆盖
    /// （否则跳过已存在），由粘贴时的覆盖弹窗一次性决定、套用整次递归。
    ///
    /// 守卫：非控制中（会话已结束）不起（防 H2 死会话复活下载态）；已有下载在途则忽略
    /// （防 M1 并发粘贴污染状态机——同一时刻仅一个下载）。
    pub fn start_download(&mut self, items: Vec<ClipItem>, dest_dir: String, overwrite: bool) {
        log::info!(
            "[下载] start_download: items={} dest={dest_dir} controlling={}",
            items.len(),
            self.is_controlling()
        );
        if items.is_empty() || !self.is_controlling() {
            log::info!(
                "[下载] 守卫拦截：empty={} not_controlling={}",
                items.is_empty(),
                !self.is_controlling()
            );
            return;
        }
        if self.download.is_some() || self.upload.is_some() {
            log::info!("[下载] 已有传输在途，忽略本次下载");
            return;
        }
        log::info!("[下载] 编排启动：{} 项 → {dest_dir}", items.len());
        self.download = Some(DownloadWalk {
            overwrite,
            dir_listdir: HashMap::new(),
            dir_queue: VecDeque::new(),
            file_queue: VecDeque::new(),
            active_files: 0,
            visited: HashSet::new(),
            done: 0,
            skipped: 0,
            errors: 0,
        });
        self.notices.push(Notice::DownloadStarted);
        let dest_root = PathBuf::from(&dest_dir);
        for item in items {
            // H1 安全：落地名必须是单个普通组件（拒 `..` / 绝对 / 分隔符 → 防对端驱动路径穿越）。
            self.download_enqueue_named(&dest_root, &item.name, item.path, item.is_dir, 0);
        }
        self.pump_download();
    }

    /// 下载编排：校验对端给的落地名安全后入队（目录 / 文件）。名字非法（穿越）→ 计错跳过。
    fn download_enqueue_named(
        &mut self,
        parent_dest: &std::path::Path,
        name: &str,
        src: String,
        is_dir: bool,
        depth: usize,
    ) {
        if !is_safe_child_name(name) {
            log::warn!("下载落地名非法（拒绝穿越）: {name:?}");
            if let Some(d) = self.download.as_mut() {
                d.errors += 1;
            }
            return;
        }
        let dest = parent_dest.join(name);
        if is_dir {
            self.download_enqueue_dir(src, dest, depth);
        } else {
            self.download_enqueue_file(src, dest);
        }
    }

    /// 下载编排：把一个远程目录入队（受 [`DOWNLOAD_MAX_LISTDIR`] 节流；pump 时才建目录 + 发
    /// ListDir）。环 / 超深跳过。
    fn download_enqueue_dir(&mut self, src: String, dest: PathBuf, depth: usize) {
        if depth > TRANSFER_MAX_DEPTH {
            return;
        }
        let Some(d) = self.download.as_mut() else {
            return;
        };
        if !d.visited.insert(src.clone()) {
            return; // 已访问 → 防 junction / symlink 成环。
        }
        d.dir_queue.push_back((src, dest, depth));
    }

    /// 下载编排：把一个远程文件入队（撞名按策略跳过 / 排队拉取）。
    fn download_enqueue_file(&mut self, src: String, dest: PathBuf) {
        let Some(d) = self.download.as_mut() else {
            return;
        };
        if !d.overwrite && dest.exists() {
            d.skipped += 1;
            return;
        }
        d.file_queue.push_back((src, dest));
    }

    /// 下载编排：收 ListDir 结果（属于下载遍历的 req_id）→ 子目录递归、文件入队。
    fn download_dir_result(&mut self, req_id: u64, entries: Vec<DirEntry>, err: Option<FsErr>) {
        let Some((dest, depth)) = self
            .download
            .as_mut()
            .and_then(|d| d.dir_listdir.remove(&req_id))
        else {
            return;
        };
        if err.is_some() {
            if let Some(d) = self.download.as_mut() {
                d.errors += 1;
            }
        } else {
            for e in entries {
                // H1 安全：子项名同样校验（被控端给的 name 不可信）。
                self.download_enqueue_named(&dest, &e.name, e.path, e.is_dir, depth + 1);
            }
        }
        self.pump_download();
    }

    /// 下载编排：在并发上限内从队列起目录列举 + 文件 Fetch；全队列空 + 无在途 → 结束汇总。
    fn pump_download(&mut self) {
        // 先在 ListDir 并发上限内出队目录：建本地目录 + 发 ListDir（dir_listdir 即在途计数）。
        loop {
            let next = {
                let Some(d) = self.download.as_mut() else {
                    return;
                };
                if d.dir_listdir.len() >= DOWNLOAD_MAX_LISTDIR {
                    break;
                }
                d.dir_queue.pop_front()
            };
            let Some((src, dest, depth)) = next else {
                break;
            };
            let _ = std::fs::create_dir_all(&dest);
            let req_id = self.next_req_id();
            if let Some(d) = self.download.as_mut() {
                d.dir_listdir.insert(req_id, (dest, depth));
            }
            // 下载列举含隐藏项（完整镜像子树）。
            self.send_frame(&RemoteFrame::ListDir {
                req_id,
                path: src,
                show_hidden: true,
            });
        }
        // 再在文件并发上限内出队文件 Fetch。
        loop {
            let next = {
                let Some(d) = self.download.as_mut() else {
                    return;
                };
                if d.active_files >= DOWNLOAD_MAX_FILES {
                    break;
                }
                d.file_queue.pop_front()
            };
            let Some((src, dest)) = next else {
                break;
            };
            let req_id = self.next_req_id();
            self.inflight_fetch.insert(
                req_id,
                FetchJob {
                    kind: FetchKind::Download,
                    src_path: src.clone(),
                    name: last_path_segment(&src),
                    dest: Some(dest),
                    file: None,
                    next_seq: 0,
                    written: 0,
                    total: 0,
                    last_at: Instant::now(),
                    clip_stream: None,
                },
            );
            if let Some(d) = self.download.as_mut() {
                d.active_files += 1;
            }
            self.send_frame(&RemoteFrame::FetchReq { req_id, path: src });
        }
        self.download_check_complete();
    }

    /// 下载编排：一个文件传输结束（成功 / 失败）→ 减并发计数 + 续推队列。
    fn download_file_done(&mut self, error: bool) {
        if let Some(d) = self.download.as_mut() {
            d.active_files = d.active_files.saturating_sub(1);
            if error {
                d.errors += 1;
            } else {
                d.done += 1;
            }
        }
        self.pump_download();
    }

    /// 下载编排：全部完成（无在途文件 + 文件 / 目录队列空 + 无在列目录）→ 汇总 toast + 结束。
    fn download_check_complete(&mut self) {
        let complete = self.download.as_ref().is_some_and(|d| {
            d.active_files == 0
                && d.file_queue.is_empty()
                && d.dir_queue.is_empty()
                && d.dir_listdir.is_empty()
        });
        if complete {
            if let Some(d) = self.download.take() {
                self.notices.push(Notice::DownloadDone {
                    done: d.done,
                    skipped: d.skipped,
                    errors: d.errors,
                });
            }
        }
    }

    /// 被控端：收 `FetchReq` → 起一个源 worker（后台逐块读、ACK 窗口背压），帧经 `cmd_tx`
    /// 直发。预授 `FETCH_WINDOW` 个许可作为滑动窗口起始额度。
    fn start_fetch_src(&mut self, req_id: u64, path: String) {
        let Some(cmd_tx) = self.cmd_tx.clone() else {
            return;
        };
        let (permit_tx, permit_rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..FETCH_WINDOW {
            let _ = permit_tx.send(());
        }
        self.inflight_fetch_src
            .insert(req_id, FetchSrcJob { permit_tx });
        let svc_tx = self.svc_tx.clone();
        thread::spawn(move || {
            fetch_src_worker(req_id, &path, &cmd_tx, &permit_rx);
            // 通知主线程清理 map 项（worker 已自经 cmd_tx 发完 FileEnd/FileErr）。
            if let Some(tx) = svc_tx {
                let _ = tx.send(SvcReply::FetchSrcDone { req_id });
            }
        });
    }

    /// 被控端：收 `FileChunkAck` → 给对应源 worker 发一个许可（控制端每收一块即放行下一块）。
    fn fetch_src_ack(&self, req_id: u64) {
        if let Some(job) = self.inflight_fetch_src.get(&req_id) {
            let _ = job.permit_tx.send(()); // worker 已退出 → 通道关 → 忽略。
        }
    }

    /// 控制端：收 `PutChunkAck`（片5 上传）→ 给对应 Put 源 worker 发一个许可（被控端每落一块
    /// 即放行下一块；对称于 [`Self::fetch_src_ack`]）。
    fn put_src_ack(&self, req_id: u64) {
        if let Some(job) = self.inflight_put_src.get(&req_id) {
            let _ = job.permit_tx.send(()); // worker 已退出 → 通道关 → 忽略。
        }
    }

    // ── part3c-2 片5 被控端 Put/MkDir 写盘服务（接收方；同步写，块小本地盘，与 fetch_begin 对称）─

    /// 被控端：`dir`/`name` 的子树断言 + 名字校验。返回 `Ok(target)` = 安全可写的目标路径；
    /// `Err(FsErr)` = 名字非法 / 路径穿越被拒（H1 安全，critique HIGH）。
    ///
    /// 防御：① `validate_entry_name` 拒 `..`/`.`/分隔符/盘符/非法字符/控制字符/结尾点空格；
    /// ② canonical 子树断言——`dir` 必须 `canonicalize` 成功，且 `dir.join(name)` 的父目录
    /// canonical 必须 `starts_with(dir_canonical)`（拒 `name` 含未被 ① 拦下的穿越组件 / symlink 逃逸）。
    fn put_resolve_target(dir: &str, name: &str) -> Result<PathBuf, FsErr> {
        if crate::shell::filetree::validate_entry_name(name).is_err() {
            return Err(FsErr::PermissionDenied);
        }
        let dir_path = Path::new(dir);
        let Ok(dir_canon) = dir_path.canonicalize() else {
            // dir 不存在 / 不可达：上传前目标目录必存在（取自远程树节点），失败即拒。
            return Err(FsErr::NotFound);
        };
        let target = dir_path.join(name);
        // target 的父目录必须落在 dir_canon 子树内（target 本身可能尚不存在，故按父目录断言）。
        // canonicalize 失败即拒（不再 fallback 放行，消除「断言永真」的纵深防御假象）。
        let parent = target.parent().unwrap_or(dir_path);
        let Ok(parent_canon) = parent.canonicalize() else {
            return Err(FsErr::PermissionDenied);
        };
        if !parent_canon.starts_with(&dir_canon) {
            log::warn!("Put 子树断言失败（拒穿越）: dir={dir} name={name:?}");
            return Err(FsErr::PermissionDenied);
        }
        Ok(target)
    }

    /// 被控端：收 `PutBegin`（片5 上传）。两阶段 + 撞名决议 + 子树断言：
    /// - 名字非法 / 路径穿越 → `PutReady{err}`；
    /// - `Skip` → `PutResult{Skipped}`（不写）；
    /// - `Probe` 且目标已存在 → `PutReady{conflict}`（不写，等控制端重发 Force/Skip）；
    /// - 否则（`Probe` 无冲突 / `Force`）→ 建同目录唯一临时文件、存 `inflight_put`、回 `PutReady{None}`。
    fn handle_put_begin(
        &mut self,
        req_id: u64,
        dir: String,
        name: String,
        _total_len: u64,
        overwrite: PutOverwrite,
    ) {
        let target = match Self::put_resolve_target(&dir, &name) {
            Ok(t) => t,
            Err(err) => {
                self.send_frame(&RemoteFrame::PutReady {
                    req_id,
                    conflict: None,
                    err: Some(err),
                });
                return;
            }
        };
        // Skip：用户已选不覆盖（重发 Skip 走这里）→ 直接回跳过，不写。
        if matches!(overwrite, PutOverwrite::Skip) {
            self.send_frame(&RemoteFrame::PutResult {
                req_id,
                status: PutStatus::Skipped,
                err: None,
            });
            return;
        }
        // Probe：探测撞名。命中已存在目标 → 回 conflict，不建临时文件、不写。
        if matches!(overwrite, PutOverwrite::Probe) && target.try_exists().unwrap_or(false) {
            let (is_dir, existing_len) = match std::fs::metadata(&target) {
                Ok(m) => (m.is_dir(), m.len()),
                Err(_) => (false, 0),
            };
            self.send_frame(&RemoteFrame::PutReady {
                req_id,
                conflict: Some(PutConflict { is_dir, existing_len }),
                err: None,
            });
            return;
        }
        // Probe 无冲突 / Force：建同目录唯一临时文件（保证 rename 同卷原子替换）。
        let parent = target.parent().map_or_else(|| PathBuf::from(&dir), Path::to_path_buf);
        let tmp_path = parent.join(format!(".lumen-put-{req_id}.tmp"));
        match std::fs::File::create(&tmp_path) {
            Ok(file) => {
                self.inflight_put.insert(
                    req_id,
                    PutDstJob {
                        tmp_path,
                        target,
                        file: Some(file),
                        next_seq: 0,
                        written: 0,
                    },
                );
                self.send_frame(&RemoteFrame::PutReady {
                    req_id,
                    conflict: None,
                    err: None,
                });
            }
            Err(e) => {
                log::warn!("Put 建临时文件失败 {}: {e}", tmp_path.display());
                self.send_frame(&RemoteFrame::PutReady {
                    req_id,
                    conflict: None,
                    err: Some(io_err_to_fs(&e)),
                });
            }
        }
    }

    /// 被控端：收 `PutChunk` → 连续性校验 + 累计上限后顺序写临时文件，回 `PutChunkAck`（背压）。
    /// 乱序 / 写失败 / 超上限 → 删临时 + 移除在途 + `PutResult{err:Io}`。
    fn handle_put_chunk(&mut self, req_id: u64, seq: u32, data: &[u8]) {
        let abort: Option<FsErr> = {
            let Some(job) = self.inflight_put.get_mut(&req_id) else {
                return;
            };
            let Some(file) = job.file.as_mut() else {
                return; // 临时文件未建 / 已中止：忽略。
            };
            if seq != job.next_seq {
                log::warn!("Put 块乱序 req={req_id} seq={seq} 期望={}", job.next_seq);
                Some(FsErr::Io)
            } else if let Err(e) = file.write_all(data) {
                log::warn!("Put 写临时文件失败: {e}");
                Some(FsErr::Io)
            } else {
                job.next_seq = job.next_seq.wrapping_add(1);
                job.written = job
                    .written
                    .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if job.written > FETCH_MAX_LEN {
                    Some(FsErr::TooLarge)
                } else {
                    None
                }
            }
        };
        if let Some(err) = abort {
            self.put_dst_abort(req_id, err);
        } else {
            self.send_frame(&RemoteFrame::PutChunkAck { req_id, seq });
        }
    }

    /// 被控端：收 `PutEnd` → flush + 关句柄 + 原子 rename 临时文件到目标（Force 覆盖语义，同卷
    /// 原子替换）→ `PutResult{Written}`；rename 失败 → 删临时 + `PutResult{err:Io}`。
    fn handle_put_end(&mut self, req_id: u64) {
        let Some(mut job) = self.inflight_put.remove(&req_id) else {
            return;
        };
        if let Some(mut file) = job.file.take() {
            let _ = file.flush();
            drop(file); // 关句柄后再 rename（Windows 替换前须释放写锁）。
        }
        match std::fs::rename(&job.tmp_path, &job.target) {
            Ok(()) => self.send_frame(&RemoteFrame::PutResult {
                req_id,
                status: PutStatus::Written,
                err: None,
            }),
            Err(e) => {
                log::warn!(
                    "Put rename 失败 {} → {}: {e}",
                    job.tmp_path.display(),
                    job.target.display()
                );
                let _ = std::fs::remove_file(&job.tmp_path);
                self.send_frame(&RemoteFrame::PutResult {
                    req_id,
                    status: PutStatus::Written,
                    err: Some(io_err_to_fs(&e)),
                });
            }
        }
    }

    /// 被控端：中止一个在途 Put（乱序 / 写失败 / 超上限）→ 关句柄、删临时文件、回 `PutResult{err}`。
    fn put_dst_abort(&mut self, req_id: u64, err: FsErr) {
        if let Some(mut job) = self.inflight_put.remove(&req_id) {
            job.file = None; // 关句柄（Windows 删前须释放）。
            let _ = std::fs::remove_file(&job.tmp_path);
        }
        self.send_frame(&RemoteFrame::PutResult {
            req_id,
            status: PutStatus::Written,
            err: Some(err),
        });
    }

    /// 被控端：收 `MkDir`（片5 递归上传建目录）→ 名字校验 + 子树断言 + `create_dir`（已存在视为
    /// 成功）。成功 → `MkDirResult{path:创建目录, err:None}`；失败 → `MkDirResult{path:"", err}`。
    fn handle_mkdir(&mut self, req_id: u64, dir: String, name: String) {
        let target = match Self::put_resolve_target(&dir, &name) {
            Ok(t) => t,
            Err(err) => {
                self.send_frame(&RemoteFrame::MkDirResult {
                    req_id,
                    path: String::new(),
                    err: Some(err),
                });
                return;
            }
        };
        let err = match std::fs::create_dir(&target) {
            Ok(()) => None,
            // 幂等：已存在视为成功（递归上传可能重复建同一目录）。
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(e) => {
                log::warn!("MkDir 失败 {}: {e}", target.display());
                Some(io_err_to_fs(&e))
            }
        };
        if let Some(err) = err {
            self.send_frame(&RemoteFrame::MkDirResult {
                req_id,
                path: String::new(),
                err: Some(err),
            });
        } else {
            self.send_frame(&RemoteFrame::MkDirResult {
                req_id,
                path: target.display().to_string(),
                err: None,
            });
        }
    }

    /// 被控端：处理 `MkFile`——在 `dir` 下建**空文件** `name`。同 `handle_mkdir` 经 `put_resolve_target`
    /// 校验名/子树；用 `create_new`（已存在则失败、**不覆盖**，区别于 MkDir 幂等）。成功回路径。
    fn handle_mkfile(&mut self, req_id: u64, dir: String, name: String) {
        let target = match Self::put_resolve_target(&dir, &name) {
            Ok(t) => t,
            Err(err) => {
                self.send_frame(&RemoteFrame::MkFileResult {
                    req_id,
                    path: String::new(),
                    err: Some(err),
                });
                return;
            }
        };
        let err = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_f) => None, // 句柄即时 drop，留下空文件
            Err(e) => {
                log::warn!("MkFile 失败 {}: {e}", target.display());
                Some(io_err_to_fs(&e))
            }
        };
        if let Some(err) = err {
            self.send_frame(&RemoteFrame::MkFileResult {
                req_id,
                path: String::new(),
                err: Some(err),
            });
        } else {
            self.send_frame(&RemoteFrame::MkFileResult {
                req_id,
                path: target.display().to_string(),
                err: None,
            });
        }
    }

    /// 被控端：处理 `Delete`——把 `path` **移入回收站**（`trash` crate，文件/目录通用、可恢复，与
    /// 本地树删除同语义；`is_dir` 仅协议保留，trash 不需要）。`path` 是先前 ListDir 枚举出的被控端
    /// 真实绝对路径（控制端为已配对鉴权方、本就可在被控端跑任意命令，删任意路径在威胁模型内、与终端
    /// 删除等价，故不额外沙箱；仅防空路径）。结果回 `DeleteResult`。
    fn handle_delete(&mut self, req_id: u64, path: String, _is_dir: bool) {
        let err = if path.trim().is_empty() {
            Some(FsErr::Io)
        } else {
            match trash::delete(&path) {
                Ok(()) => None,
                Err(e) => {
                    log::warn!("Delete(回收站) 失败 {path}: {e}");
                    Some(FsErr::Io)
                }
            }
        };
        self.send_frame(&RemoteFrame::DeleteResult { req_id, err });
    }

    // ── part3c-2 片5 控制端上传编排（本地 → 被控端递归传输）────────────────────

    /// 控制端：开始把剪贴板里的本地项上传到被控端 `remote_dir`（递归）。
    ///
    /// 守卫：非控制中（会话已结束）不起（防死会话复活上传态）；已有上传在途则忽略（同一时刻
    /// 仅一个上传）。撞名首发用 `Probe`（除非已 `policy==Some(true)` 用 Force），被控端探得已存在
    /// 时弹覆盖模态一次性决策、套用整次递归。
    pub fn start_upload(&mut self, items: Vec<ClipItem>, remote_dir: String) {
        // 诊断日志（对标 start_download 透明度）：上传不工作时一眼看出卡在哪个守卫。
        if items.is_empty() {
            log::info!("[上传] 无可上传项，忽略");
            return;
        }
        if !self.is_controlling() {
            log::warn!("[上传] 非控制端态，忽略（需先发起对设备的控制）");
            return;
        }
        if self.upload.is_some() || self.download.is_some() {
            log::warn!(
                "[上传] 已有传输进行中（upload={} download={}），忽略本次",
                self.upload.is_some(),
                self.download.is_some()
            );
            return;
        }
        log::info!("[上传] 启动：{} 项 → 被控端目录 {remote_dir}", items.len());
        self.upload = Some(UploadWalk {
            remote_root: remote_dir.clone(),
            policy: None,
            file_queue: VecDeque::new(),
            inflight_mkdir: HashMap::new(),
            active_puts: 0,
            visited: HashSet::new(),
            conflict_queue: VecDeque::new(),
            done: 0,
            skipped: 0,
            errors: 0,
        });
        self.notices.push(Notice::UploadStarted);
        for item in items {
            let local_path = PathBuf::from(&item.path);
            if item.is_dir {
                self.upload_send_mkdir(local_path, remote_dir.clone(), item.name, 0);
            } else {
                self.upload_enqueue_file(local_path, remote_dir.clone(), item.name);
            }
        }
        self.pump_upload();
    }

    /// 上传编排：发一个 MkDir 在被控端 `remote_dir` 下建 `name`，并记 `inflight_mkdir[req_id]`。
    /// 环 / 超深 / 名字非法跳过（计错）。
    fn upload_send_mkdir(
        &mut self,
        local_dir: PathBuf,
        remote_dir: String,
        name: String,
        depth: usize,
    ) {
        if depth > TRANSFER_MAX_DEPTH {
            return;
        }
        if !is_safe_child_name(&name) {
            log::warn!("上传目录名非法（拒绝穿越）: {name:?}");
            if let Some(u) = self.upload.as_mut() {
                u.errors += 1;
            }
            return;
        }
        // 防 junction / symlink 成环：按本地 canonical 去重。
        let canon = local_dir.canonicalize().unwrap_or_else(|_| local_dir.clone());
        {
            let Some(u) = self.upload.as_mut() else {
                return;
            };
            if !u.visited.insert(canon) {
                return;
            }
        }
        let req_id = self.next_req_id();
        if let Some(u) = self.upload.as_mut() {
            u.inflight_mkdir.insert(req_id, (local_dir, depth));
        }
        self.send_frame(&RemoteFrame::MkDir {
            req_id,
            dir: remote_dir,
            name,
        });
    }

    /// 上传编排：把一个本地文件入队（撞名由被控端 `Probe` 决定，不在控制端本地判）。
    fn upload_enqueue_file(&mut self, local_path: PathBuf, remote_dir: String, name: String) {
        if !is_safe_child_name(&name) {
            log::warn!("上传文件名非法（拒绝穿越）: {name:?}");
            if let Some(u) = self.upload.as_mut() {
                u.errors += 1;
            }
            return;
        }
        if let Some(u) = self.upload.as_mut() {
            u.file_queue.push_back((local_path, remote_dir, name));
        }
    }

    /// 上传编排：收 `MkDirResult`（属于上传遍历的 req_id）→ 用返回的被控端 `path` 作为子项 `dir`，
    /// 读本地目录、子目录续发 MkDir、文件入队。
    fn upload_mkdir_result(&mut self, req_id: u64, path: String, err: Option<FsErr>) {
        let Some((local_dir, depth)) = self
            .upload
            .as_mut()
            .and_then(|u| u.inflight_mkdir.remove(&req_id))
        else {
            return;
        };
        if err.is_some() {
            if let Some(u) = self.upload.as_mut() {
                u.errors += 1;
            }
            self.pump_upload();
            return;
        }
        // 读本地目录，分流子目录 / 文件（深度受 TRANSFER_MAX_DEPTH 限，环由 visited 防）。
        if depth < TRANSFER_MAX_DEPTH {
            match std::fs::read_dir(&local_dir) {
                Ok(rd) => {
                    for entry in rd.flatten() {
                        let child = entry.path();
                        let child_name = entry.file_name().to_string_lossy().into_owned();
                        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        if is_dir {
                            self.upload_send_mkdir(child, path.clone(), child_name, depth + 1);
                        } else {
                            self.upload_enqueue_file(child, path.clone(), child_name);
                        }
                    }
                }
                Err(e) => {
                    log::warn!("上传读本地目录失败 {}: {e}", local_dir.display());
                    if let Some(u) = self.upload.as_mut() {
                        u.errors += 1;
                    }
                }
            }
        }
        self.pump_upload();
    }

    /// 上传编排：在并发上限内从 `file_queue` 出队起文件上传（首发 `PutBegin` 探测撞名）；
    /// 未决期间 `conflict_queue` 非空时暂停起新文件（等覆盖模态）。全队列空 + 无在途 → 结束汇总。
    fn pump_upload(&mut self) {
        loop {
            let next = {
                let Some(u) = self.upload.as_mut() else {
                    return;
                };
                // 未决期间已积压撞名冲突：暂停起新文件（等用户在覆盖模态拍板）。
                if u.policy.is_none() && !u.conflict_queue.is_empty() {
                    break;
                }
                if u.active_puts >= DOWNLOAD_MAX_FILES {
                    break;
                }
                u.file_queue.pop_front()
            };
            let Some((local_path, remote_dir, name)) = next else {
                break;
            };
            // 本地文件长度（仅供进度；读不到兜底 0，不阻断上传）。
            let total_len = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
            // 首发策略：policy==Some(true) 用 Force 直接覆盖；否则（None / Some(false)）用 Probe
            // 探测——None 探到冲突弹模态、Some(false) 探到冲突时被控端按 Skip 重发跳过。
            let overwrite = if self.upload.as_ref().and_then(|u| u.policy) == Some(true) {
                PutOverwrite::Force
            } else {
                PutOverwrite::Probe
            };
            let req_id = self.next_req_id();
            self.inflight_put_meta.insert(
                req_id,
                PutMeta {
                    local_path,
                    remote_dir: remote_dir.clone(),
                    name: name.clone(),
                },
            );
            if let Some(u) = self.upload.as_mut() {
                u.active_puts += 1;
            }
            self.send_frame(&RemoteFrame::PutBegin {
                req_id,
                dir: remote_dir,
                name,
                total_len,
                overwrite,
            });
        }
        self.upload_check_complete();
    }

    /// 控制端：收 `PutReady`（属于上传的 req_id）→ 决策：
    /// - `err` → 计错 + 减并发 + 续 pump；
    /// - `conflict:None` → 开始发送（spawn `put_send_worker` + 预授许可）；
    /// - `conflict:Some` → 按 `policy`：覆盖全部→重发 Force；跳过全部→计跳过 + 减并发；
    ///   未决（None）→ park 进 `conflict_queue`、暂停起新文件，等覆盖模态。
    fn put_ready(&mut self, req_id: u64, conflict: Option<PutConflict>, err: Option<FsErr>) {
        if !self.inflight_put_meta.contains_key(&req_id) {
            return; // 非上传的 req_id（理论上不会，防御）。
        }
        if err.is_some() {
            self.inflight_put_meta.remove(&req_id);
            if let Some(u) = self.upload.as_mut() {
                u.errors += 1;
                u.active_puts = u.active_puts.saturating_sub(1);
            }
            self.pump_upload();
            return;
        }
        if conflict.is_none() {
            // 无冲突（Probe 探得不存在 / Force 已建临时）→ 开始发送字节流。
            self.put_start_send(req_id);
            return;
        }
        // 撞名：按 policy 决策。
        let policy = self.upload.as_ref().and_then(|u| u.policy);
        match policy {
            Some(true) => {
                // 覆盖全部：重发 PutBegin(Force) 让被控端建临时文件（Probe 冲突时没建），再等其
                // PutReady{None} 才发送。
                self.put_resend_force(req_id);
            }
            Some(false) => {
                // 跳过全部已存在：计跳过 + 减并发 + 续 pump。
                self.inflight_put_meta.remove(&req_id);
                if let Some(u) = self.upload.as_mut() {
                    u.skipped += 1;
                    u.active_puts = u.active_puts.saturating_sub(1);
                }
                self.pump_upload();
            }
            None => {
                // 未决：把冲突 req park 进队列（**不覆盖**已有的，H1 修复），等覆盖模态拍板。
                // 不减 active_puts；pump 因 conflict_queue 非空而暂停起新文件。
                if let Some(u) = self.upload.as_mut() {
                    u.conflict_queue.push_back(req_id);
                }
            }
        }
    }

    /// 控制端：对一个 `PutReady{conflict:None}` 的文件开始发送字节流（spawn `put_send_worker` +
    /// 预授 `FETCH_WINDOW` 许可）。源路径取自 `inflight_put_meta`。
    fn put_start_send(&mut self, req_id: u64) {
        // LOW-4 幂等：同 req 已在发送（被控端异常重发 PutReady{None}）则忽略，避免双 worker / 双 seq。
        if self.inflight_put_src.contains_key(&req_id) {
            return;
        }
        let Some(local_path) = self
            .inflight_put_meta
            .get(&req_id)
            .map(|m| m.local_path.display().to_string())
        else {
            return;
        };
        let Some(cmd_tx) = self.cmd_tx.clone() else {
            return;
        };
        let (permit_tx, permit_rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..FETCH_WINDOW {
            let _ = permit_tx.send(());
        }
        self.inflight_put_src
            .insert(req_id, PutSrcJob { permit_tx });
        let svc_tx = self.svc_tx.clone();
        thread::spawn(move || {
            let ok = put_send_worker(req_id, &local_path, &cmd_tx, &permit_rx);
            // 成功 → PutSrcDone（等被控端 PutResult 收尾）；失败 → PutSrcFailed（主线程自行收尾）。
            if let Some(tx) = svc_tx {
                let _ = tx.send(if ok {
                    SvcReply::PutSrcDone { req_id }
                } else {
                    SvcReply::PutSrcFailed { req_id }
                });
            }
        });
    }

    /// 控制端：撞名后用户选覆盖 → 对 `req_id` 重发 `PutBegin(Force)`（被控端 Probe 冲突时没建临时
    /// 文件，须重发 Force 让其建、再等 `PutReady{None}` 才发送）。元信息复用 `inflight_put_meta`。
    fn put_resend_force(&mut self, req_id: u64) {
        let Some((remote_dir, name, total_len)) = self.inflight_put_meta.get(&req_id).map(|m| {
            let total_len = std::fs::metadata(&m.local_path).map(|md| md.len()).unwrap_or(0);
            (m.remote_dir.clone(), m.name.clone(), total_len)
        }) else {
            return;
        };
        self.send_frame(&RemoteFrame::PutBegin {
            req_id,
            dir: remote_dir,
            name,
            total_len,
            overwrite: PutOverwrite::Force,
        });
    }

    /// 控制端：收 `PutResult`（属于上传的 req_id）→ 计统计 + 减并发 + 清元信息 + 续 pump。
    fn put_result(&mut self, req_id: u64, status: PutStatus, err: Option<FsErr>) {
        if self.inflight_put_meta.remove(&req_id).is_none() {
            return;
        }
        self.inflight_put_src.remove(&req_id); // worker 通常已自退（PutEnd 后），幂等清理。
        if let Some(u) = self.upload.as_mut() {
            if err.is_some() {
                u.errors += 1;
            } else {
                match status {
                    PutStatus::Written => u.done += 1,
                    PutStatus::Skipped => u.skipped += 1,
                }
            }
            u.active_puts = u.active_puts.saturating_sub(1);
        }
        self.pump_upload();
    }

    /// 上传编排：全部完成（文件队列空 + 无在途 MkDir + 无在途 Put + 无待决冲突）→ 汇总 + 结束。
    fn upload_check_complete(&mut self) {
        let complete = self.upload.as_ref().is_some_and(|u| {
            u.file_queue.is_empty()
                && u.inflight_mkdir.is_empty()
                && u.active_puts == 0
                && u.conflict_queue.is_empty()
        });
        if complete {
            if let Some(u) = self.upload.take() {
                log::debug!(
                    "上传完成 root={} done={} skipped={} errors={}",
                    u.remote_root,
                    u.done,
                    u.skipped,
                    u.errors
                );
                self.notices.push(Notice::UploadDone {
                    done: u.done,
                    skipped: u.skipped,
                    errors: u.errors,
                });
            }
        }
    }

    /// 控制端：上传撞名时供覆盖模态的待决冲突项数（`conflict_queue` 长度；未决期间非空）。
    #[must_use]
    pub fn upload_conflict_count(&self) -> Option<usize> {
        self.upload.as_ref().and_then(|u| {
            if u.policy.is_none() && !u.conflict_queue.is_empty() {
                Some(u.conflict_queue.len())
            } else {
                None
            }
        })
    }

    /// 控制端：用户在覆盖模态拍板（上传撞名）：覆盖 → `policy=Some(true)` + 对**全部**待决文件
    /// 重发 Force；跳过 → `policy=Some(false)` + 计跳过全部待决；取消 → 中止整个上传。决策后
    /// `policy` 锁定，后续冲突直接套用、不再弹窗。
    pub fn resolve_upload_conflict(&mut self, choice: crate::shell::OverwriteChoice) {
        use crate::shell::OverwriteChoice;
        // 取走整个待决队列（一次决策套用全部，H1：多冲突不丢）。
        let Some(queued) = self
            .upload
            .as_mut()
            .map(|u| std::mem::take(&mut u.conflict_queue))
        else {
            return;
        };
        if queued.is_empty() {
            return;
        }
        match choice {
            OverwriteChoice::Overwrite => {
                if let Some(u) = self.upload.as_mut() {
                    u.policy = Some(true);
                }
                for req_id in queued {
                    self.put_resend_force(req_id); // active_puts 已计在内、不变
                }
                self.pump_upload();
            }
            OverwriteChoice::Skip => {
                let n = queued.len();
                for req_id in &queued {
                    self.inflight_put_meta.remove(req_id);
                }
                if let Some(u) = self.upload.as_mut() {
                    u.policy = Some(false);
                    u.skipped += n;
                    u.active_puts = u.active_puts.saturating_sub(n);
                }
                self.pump_upload();
            }
            OverwriteChoice::Cancel => {
                // 中止整个上传：清在途源 / 元信息（被控端临时文件由会话清理回收）。
                self.inflight_put_src.clear();
                self.inflight_put_meta.clear();
                self.upload = None;
            }
        }
    }

    /// 清空文件树同步态（会话起止 / 断线；**不**在终端 Resize 时清——resize 不动文件树）。
    fn clear_remote_filetree(&mut self) {
        self.remote_filetree = None;
        self.remote_root_sent = None;
        self.pending_listdir.clear();
        self.pending_listdir_recursive.clear();
        self.inflight_clip_dir = None;
        self.clip_dir_ready = None;
        // 控制端在途 Fetch：关句柄 + 删半成品文件（临时 / 下载半成品）。
        for (_, mut job) in self.inflight_fetch.drain() {
            job.file = None;
            if let Some(d) = job.dest.take() {
                let _ = std::fs::remove_file(d);
            }
        }
        // 被控端在途 Fetch 源：drop permit_tx → worker 领许可失败自行退出。
        self.inflight_fetch_src.clear();
        // #7 下载编排终止（在途文件的部分落地文件已由上面 inflight_fetch.drain 删除）。
        self.download = None;
        // 片5 被控端在途 Put 目标：关句柄 + 删临时文件（半成品 `.lumen-put-*.tmp`）。
        for (_, mut job) in self.inflight_put.drain() {
            job.file = None;
            let _ = std::fs::remove_file(&job.tmp_path);
        }
        // 片5 控制端在途 Put 源：drop permit_tx → worker 领许可失败自行退出。
        self.inflight_put_src.clear();
        self.inflight_put_meta.clear();
        // 片5 上传编排终止。
        self.upload = None;
        // 远程侧剪贴板失效（被控端不可达，防粘贴野路径）；本地侧保留（仍指向控制端本机）。
        if matches!(
            self.file_clipboard.as_ref().map(|c| c.side),
            Some(ClipSide::Remote)
        ) {
            self.file_clipboard = None;
        }
        // svc_rx 里可能残留旧会话回包：排空丢弃（新会话 req_id 体系不同、且态已清）。
        if let Some(rx) = self.svc_rx.as_ref() {
            while rx.try_recv().is_ok() {}
        }
    }

    /// 是否为控制端（控制中）：true 时本端键盘输入应转发而非本地执行。
    #[must_use]
    pub fn is_controlling(&self) -> bool {
        matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller))
    }

    /// 是否为被控端（被控中）：用于把 `SubLayout` 比例应用到本端 tab 布局（vs 控制端应用到镜像布局）。
    #[must_use]
    pub fn is_controlled(&self) -> bool {
        matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled))
    }

    /// 被控端：取走待执行的远程输入 `(tab_id, session_id, 字节)`（main 按双 id 查窗格 + per-pane 仲裁后写 PTY）。
    pub fn take_input(&mut self) -> Vec<(TabId, SessionId, Vec<u8>)> {
        std::mem::take(&mut self.pending_input)
    }

    /// 控制端：对订阅窗格发远程操作（[`RemoteFrame::PaneOp`]：关闭/最大化/换位）。被控端执行后布局变化
    /// 经 `SubscriptionStarted` 重发同步。仅控制中可发。
    pub fn send_pane_op(&self, tab_id: TabId, session_id: SessionId, op: PaneOpKind) {
        if !self.is_controlling() {
            return;
        }
        self.send_frame(&RemoteFrame::PaneOp {
            tab_id,
            session_id,
            op,
        });
    }

    /// 被控端：取走待执行的远程窗格操作（main 按双 id 查窗格执行新建/关闭/最大化/换位）。
    pub fn take_pane_ops(&mut self) -> Vec<(TabId, SessionId, PaneOpKind)> {
        std::mem::take(&mut self.pending_pane_ops)
    }

    /// 控制端：对**当前焦点镜像窗格**发 PaneOp（远程视图下快捷键/按钮路由远程操作焦点窗格——
    /// 关闭/最大化等）。无订阅 / 无焦点窗格则忽略。
    pub fn send_focused_pane_op(&self, op: PaneOpKind) {
        if let (Some(tab_id), Some(sid)) = (self.subscribed_tab, self.mirror_active_pane) {
            self.send_pane_op(tab_id, sid, op);
        }
    }

    /// 控制端：远程视图下「新建窗格」→ 在订阅会话加一格（`PaneOpKind::New` 忽略 session_id，填 0）。
    pub fn send_new_remote_pane(&self) {
        if let Some(tab_id) = self.subscribed_tab {
            self.send_pane_op(tab_id, 0, PaneOpKind::New);
        }
    }

    /// 控制端：滚轮回看镜像历史（part3d 按需拉取）。`lines > 0` 向上看更旧、`< 0` 向下；
    /// 按**绝对行**锚定窗口——被控端实时输出推进时回看内容不被推走（标准终端回滚行为）。
    /// 滚回底部即恢复「跟随实时」。返回是否改变了视图（驱动重绘）。
    pub fn scroll_mirror(&mut self, lines: isize) -> bool {
        // 单窗格借 `mirror`，多窗格借**焦点窗格**（`mirror_active_pane`）；都无则无可回看。
        if self.focused_mirror_term().is_none() {
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
        // 视图窗口变了（跟随↔回看 / 换窗口）：旧选区坐标作废（单窗格 + 多窗格焦点窗格都清）。
        self.mirror_selection = None;
        self.mirror_selecting = false;
        self.mirror_pane_selecting = None;
        if let Some(sid) = self.mirror_active_pane {
            if let Some(mp) = self.mirror_panes.iter_mut().find(|p| p.session_id == sid) {
                mp.selection = None;
            }
        }
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

    // ── part3d Phase 4 多窗格**焦点窗格**回看（复用单窗格 hist 机制，按 mirror_active_pane 驱动）──

    /// 控制端：多窗格焦点窗格是否处于回看态（`hist_top` 有值；渲染据此用 `hist_term` 代替 live 窗格 term）。
    #[must_use]
    pub fn mirror_pane_in_hist(&self) -> bool {
        !self.mirror_panes.is_empty() && self.hist_top.is_some()
    }

    /// 控制端：焦点窗格在回看态时，按 `(rows, cols)` 拉取缺失历史行 + 构建 `hist_term` scratch（与单窗格
    /// `mirror_render` 回看分支同源；多窗格渲染段在画焦点窗格前调，之后 [`Self::mirror_hist_term`] 借出）。
    /// 非回看态无操作。
    pub fn prepare_focused_pane_hist(&mut self, rows: usize, cols: usize) {
        // 规整回看锚点（同 mirror_render：触底回跟随 / 越下界夹到 base）。
        if let (Some(top), Some((base, screen_top))) = (self.hist_top, self.hist_bounds) {
            let fixed = if top >= screen_top {
                None
            } else if top < base {
                Some(base)
            } else {
                Some(top)
            };
            self.hist_top = fixed;
        }
        let Some(top) = self.hist_top else {
            return;
        };
        self.fetch_history_window(top, rows);
        self.build_hist_term(top, rows, cols);
    }

    /// 控制端：当前回看 scratch 终端（`prepare_focused_pane_hist` 构建后借出，渲染焦点窗格回看用）。
    #[must_use]
    pub fn mirror_hist_term(&self) -> Option<&Terminal> {
        self.hist_term.as_ref()
    }

    /// 控制端：清理「被控端字节夹紧后未返回」的尾部缺口 inflight 行——从 `from` 起把**连续在途**行移出
    /// `hist_inflight`，使下次 [`Self::fetch_history_window`] 扫描据缺口从 `from` 续请求（被控端再从此处
    /// 继续字节夹紧、逐段推进），杜绝缺口行永久卡在 inflight 致回看永久空白。整段返回（无字节夹紧）时
    /// `from` 通常已非在途、即时停手，无副作用；偶发与相邻在途请求重叠时仅多一次幂等重拉，不致错乱。
    fn clear_inflight_gap(&mut self, from: u64) {
        let mut a = from;
        while self.hist_inflight.remove(&a) {
            a = a.saturating_add(1);
        }
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

    /// 控制端：请求历史行 `[top, top+count)`，标记在途。多窗格镜像（`mirror_panes` 非空）发
    /// `HistoryReqForPane`（带焦点窗格 `mirror_active_pane`），单窗格镜像发旧 `HistoryReq`（焦点窗格）。
    fn request_history(&mut self, top: u64, count: u16) {
        if count == 0 {
            return;
        }
        for a in top..top.saturating_add(u64::from(count)) {
            self.hist_inflight.insert(a);
        }
        match (self.subscribed_tab, self.mirror_panes.is_empty(), self.mirror_active_pane) {
            (Some(tab_id), false, Some(session_id)) => self.send_frame(
                &RemoteFrame::HistoryReqForPane { tab_id, session_id, top, count },
            ),
            _ => self.send_frame(&RemoteFrame::HistoryReq { top, count }),
        }
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
        self.focused_mirror_term().is_some_and(Terminal::win32_input)
    }

    /// 控制端（part4c）：当前镜像光标 `(row, col)`（跟随态 Some；回看态 None）。IME
    /// 候选框定位到被控端光标处用。
    #[must_use]
    pub fn mirror_cursor(&self) -> Option<(usize, usize)> {
        if self.hist_top.is_some() {
            return None;
        }
        self.focused_mirror_term().map(|m| {
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

    // ── part3d Phase 4 多窗格 per-pane 选区 / 复制 / 焦点窗格状态 ───────────────────

    /// 控制端：当前**焦点镜像窗格**的 term（多窗格取 `mirror_active_pane` 对应窗格、单窗格取 `mirror`）。
    /// 供输入编码取目标窗格实时 win32 / bracketed-paste 模式、IME 光标定位。
    fn focused_mirror_term(&self) -> Option<&Terminal> {
        if self.mirror_panes.is_empty() {
            self.mirror.as_ref()
        } else {
            let sid = self.mirror_active_pane?;
            self.mirror_panes
                .iter()
                .find(|p| p.session_id == sid)
                .map(|p| &p.term)
        }
    }

    /// 控制端：窗格 `sid` 当前**显示源**的可视区首行绝对行号——焦点窗格处于回看态时取 `hist_term`
    /// 的 view_top（与渲染一致），否则取该窗格 live term 的 view_top。选区起点/终点据此换算 line，
    /// 保证回看态坐标系与所画 `hist_term` 对齐（BUG-1：否则用 live 大绝对行号、高亮/复制错位）。
    fn pane_view_top(&self, sid: SessionId) -> u64 {
        if self.mirror_active_pane == Some(sid) && self.mirror_pane_in_hist() {
            if let Some(ht) = self.hist_term.as_ref() {
                return ht.grid().view_top_abs_line();
            }
        }
        self.mirror_panes
            .iter()
            .find(|p| p.session_id == sid)
            .map_or(0, |mp| mp.term.grid().view_top_abs_line())
    }

    /// 控制端：多窗格在窗格 `sid` 的 `(row, col)` 起选（建空选区 + 进拖选态 + 清其它窗格选区，
    /// 保「一时刻一个选区源」复制无歧义）。`row/col` 为该窗格内容矩形内行列（调用方按窗格像素换算）。
    pub fn mirror_pane_sel_start(&mut self, sid: SessionId, row: usize, col: usize) {
        let line = self.pane_view_top(sid) + row as u64; // 回看态对齐 hist_term 口径（BUG-1）。
        for mp in self.mirror_panes.iter_mut() {
            if mp.session_id == sid {
                let p = SelPoint { line, col };
                mp.selection = Some(Selection { anchor: p, head: p });
            } else {
                mp.selection = None;
            }
        }
        self.mirror_pane_selecting = Some(sid);
    }

    /// 控制端：拖动更新当前拖选窗格的选区终点。返回是否真移动（驱动重绘）。
    pub fn mirror_pane_sel_update(&mut self, row: usize, col: usize) -> bool {
        let Some(sid) = self.mirror_pane_selecting else {
            return false;
        };
        let head = SelPoint {
            line: self.pane_view_top(sid) + row as u64, // 同 sel_start 口径（回看态对齐 hist_term）。
            col,
        };
        if let Some(mp) = self.mirror_panes.iter_mut().find(|p| p.session_id == sid) {
            if let Some(sel) = mp.selection.as_mut() {
                if sel.head != head {
                    sel.head = head;
                    return true;
                }
            }
        }
        false
    }

    /// 控制端：是否正在多窗格拖选。
    #[must_use]
    pub fn mirror_pane_selecting(&self) -> bool {
        self.mirror_pane_selecting.is_some()
    }

    /// 控制端：当前拖选窗格 session_id（main 据此把鼠标位置 clamp 到该窗格矩形换算 cell，
    /// 拖出窗格收在边缘、不跳别格）。
    #[must_use]
    pub fn mirror_pane_selecting_sid(&self) -> Option<SessionId> {
        self.mirror_pane_selecting
    }

    /// 控制端：结束多窗格拖选（仅点击未拖 = 空选区则清掉）。
    pub fn mirror_pane_sel_end(&mut self) {
        if let Some(sid) = self.mirror_pane_selecting.take() {
            if let Some(mp) = self.mirror_panes.iter_mut().find(|p| p.session_id == sid) {
                if mp.selection.is_some_and(|s| s.is_empty()) {
                    mp.selection = None;
                }
            }
        }
    }

    /// 控制端：焦点窗格当前是否有非空选区（Ctrl+C 裁决：复制 vs 中断转发）。
    #[must_use]
    pub fn has_mirror_pane_selection(&self) -> bool {
        self.mirror_active_pane
            .and_then(|sid| self.mirror_panes.iter().find(|p| p.session_id == sid))
            .is_some_and(|mp| mp.selection.is_some_and(|s| !s.is_empty()))
    }

    /// 控制端：取**焦点窗格**选区文本（复制到本地剪贴板；空选区 / 空文本返回 `None`）。回看态从所显示的
    /// `hist_term` 取文本（与选区坐标系一致，BUG-1），跟随态从 live 窗格 term 取。
    #[must_use]
    pub fn copy_mirror_pane_selection(&self) -> Option<String> {
        let sid = self.mirror_active_pane?;
        let mp = self.mirror_panes.iter().find(|p| p.session_id == sid)?;
        let sel = mp.selection.filter(|s| !s.is_empty())?;
        let term = if self.mirror_pane_in_hist() {
            self.hist_term.as_ref().unwrap_or(&mp.term)
        } else {
            &mp.term
        };
        let text = term.selection_text(&sel);
        (!text.is_empty()).then_some(text)
    }

    /// 控制端：复制当前焦点镜像选区文本（多窗格焦点窗格优先，单窗格回退）。复制收口。
    #[must_use]
    pub fn copy_mirror_active(&self) -> Option<String> {
        self.copy_mirror_pane_selection()
            .or_else(|| self.copy_mirror_selection())
    }

    /// 控制端：焦点镜像当前是否有非空选区（Ctrl+C 裁决：复制 vs 中断转发）。多窗格 or 单窗格。
    #[must_use]
    pub fn has_mirror_active_selection(&self) -> bool {
        self.has_mirror_pane_selection() || self.has_mirror_selection()
    }

    /// 控制端：清空焦点镜像选区（多窗格焦点窗格 + 单窗格都清；复制成功 / 切视图后调用）。
    pub fn clear_mirror_active_selection(&mut self) {
        self.mirror_selection = None;
        self.mirror_selecting = false;
        self.mirror_pane_selecting = None;
        if let Some(sid) = self.mirror_active_pane {
            if let Some(mp) = self.mirror_panes.iter_mut().find(|p| p.session_id == sid) {
                mp.selection = None;
            }
        }
    }

    /// 控制端：把文本作为「粘贴」转发给被控端 PTY——换行规整为 CR，按被控端 bracketed
    /// paste 模式（镜像跟踪自 VT 流）包裹，经 `RemoteFrame::Input` 发送。
    pub fn send_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self
            .focused_mirror_term()
            .is_some_and(Terminal::bracketed_paste);
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

    /// 把数据面帧投出：M6 Phase 3 选路。**仅被控端→控制端「输出方向」走 QUIC 直连**（输出乱序由切换时
    /// 的整屏快照重建自愈、文件块带 seq 重组，故走直连安全）；**控制端→被控端「输入方向」恒走中继**——
    /// 输入若走 QUIC，切换瞬间 QUIC 帧可越过在飞的中继输入帧、致被控端 PTY 收到乱序字节且无快照可自愈
    /// （审查发现，见 docs/M6 §5）。`P2pSignal` 信令亦必走中继（直连断时才能协商回退）。直连未就绪 /
    /// 通道断则回退中继。序列化失败仅记日志、不断连。
    fn send_frame(&self, frame: &RemoteFrame) {
        let v = match frame.to_value() {
            Ok(v) => v,
            Err(e) => {
                log::error!("远程数据面帧序列化失败: {e}");
                return;
            }
        };
        // 仅被控端的非信令数据帧（即输出方向）在直连就绪时走 QUIC；发送失败（通道断）回退中继。
        if self.is_controlled() && !matches!(frame, RemoteFrame::P2pSignal { .. }) {
            if let Some(p2p) = &self.p2p {
                if p2p.is_data_ready() && p2p.try_send_frame(v.clone()) {
                    return;
                }
            }
        }
        self.send(RemoteC2S::Relay(v));
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
            RemoteFrame::Input(_bytes) => {
                // part3d Phase 4：旧无 id `Input` 休眠（版本门已挡 v1 对端；本端只发带双 id 的
                // InputWithId）。无 (tab,session) 无法安全路由，丢弃。
            }
            RemoteFrame::InputWithId {
                tab_id,
                session_id,
                data,
            } => {
                // 仅被控端会话期间接受：入队，main 按 (tab_id, session_id) 查窗格 + per-pane 仲裁后写 PTY。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_input.push((tab_id, session_id, data));
                }
            }
            RemoteFrame::PaneOp {
                tab_id,
                session_id,
                op,
            } => {
                // 仅被控端会话期间接受：入队，main 按 (tab_id, session_id) 查窗格执行关闭/最大化/换位。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_pane_ops.push((tab_id, session_id, op));
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
                // 仅被控端应答（单窗格镜像焦点窗格）：入队（目标 None），main 序列化后回 HistoryRows。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_history.push((None, top, count));
                }
            }
            RemoteFrame::HistoryReqForPane {
                tab_id: _,
                session_id,
                top,
                count,
            } => {
                // 仅被控端应答（多窗格指定窗格）：入队（目标 Some(sid)），main 序列化后回 HistoryRowsForPane。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_history.push((Some(session_id), top, count));
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
                    let n = lines.len() as u64;
                    for (i, bytes) in lines.into_iter().enumerate() {
                        let abs = top + i as u64;
                        self.hist_inflight.remove(&abs);
                        self.hist_cache.insert(abs, bytes);
                    }
                    self.clear_inflight_gap(top + n);
                    self.hist_version = self.hist_version.wrapping_add(1);
                    self.trim_history_cache();
                }
            }
            RemoteFrame::HistoryRowsForPane {
                session_id,
                top,
                base,
                screen_top,
                lines,
            } => {
                // 仅控制端 且 仍是当前焦点窗格的回看（切焦点窗格后到达的陈旧应答按 session_id 丢弃，
                // 避免把别窗格绝对行喂进当前焦点窗格回看态串台）。绝对行号体系按该窗格独立。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller))
                    && self.mirror_active_pane == Some(session_id)
                {
                    self.hist_bounds = Some((base, screen_top));
                    let n = lines.len() as u64;
                    for (i, bytes) in lines.into_iter().enumerate() {
                        let abs = top + i as u64;
                        self.hist_inflight.remove(&abs);
                        self.hist_cache.insert(abs, bytes);
                    }
                    self.clear_inflight_gap(top + n);
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
            // ── part3c-2 Option B 目录树 + 双向传输（按角色路由）──────────────
            RemoteFrame::RootChanged { path } => {
                // 仅控制端：被控端焦点窗格 cwd 变 → 换根（清缓存 + pending）并按需拉根 listing。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    let changed = {
                        let ft = self
                            .remote_filetree
                            .get_or_insert_with(RemoteFileTree::default);
                        ft.set_root(path)
                    };
                    if changed {
                        self.request_root_listing();
                    }
                }
            }
            RemoteFrame::ListDir {
                req_id,
                path,
                show_hidden,
            } => {
                // 仅被控端：入队，main 后台读盘服务（spawn_list_dir）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_listdir.push((req_id, path, show_hidden));
                }
            }
            RemoteFrame::ListDirResult {
                req_id,
                path,
                entries,
                overflow,
                err,
            } => {
                // 仅控制端：先看是否属于 #7 下载遍历的 req_id（路由到下载编排），否则填浏览树。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    let is_download = self
                        .download
                        .as_ref()
                        .is_some_and(|d| d.dir_listdir.contains_key(&req_id));
                    if is_download {
                        self.download_dir_result(req_id, entries, err);
                    } else if let Some(ft) = self.remote_filetree.as_mut() {
                        ft.apply_dir_entries(req_id, &path, entries, overflow, err);
                    }
                }
            }
            RemoteFrame::ListDirRecursive {
                req_id,
                path,
                show_hidden,
                ..
            } => {
                // 仅被控端：入队，main 后台递归读盘（spawn_list_dir_recursive）。max_depth/max_entries
                // 忽略（两端共享同一份协议常量，被控端用本地上限即等价）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    log::debug!("[片8] 被控端收 ListDirRecursive req={req_id} path={path}");
                    self.pending_listdir_recursive
                        .push((req_id, path, show_hidden));
                }
            }
            RemoteFrame::ListDirRecursiveResult {
                req_id,
                path,
                entries,
                truncated,
                err,
            } => {
                // 仅控制端：复制远程目录的递归枚举应答 → 暂存子树供构造虚拟文件 descriptor。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.apply_list_dir_recursive(req_id, path, entries, truncated, err);
                }
            }
            // ── part3c-2 文件读取 Fetch（#5 打开 / 片4 下载）────────────────
            RemoteFrame::FetchReq { req_id, path } => {
                // 仅被控端：起源 worker（后台读 + ACK 背压）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.start_fetch_src(req_id, path);
                }
            }
            RemoteFrame::FetchCancel { req_id } => {
                // 仅被控端：控制端中止该 Fetch → 移除源任务（drop 许可通道 → worker 自退、
                // 即时释放文件句柄 / 线程）。worker 已自然退出时为幂等无操作。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.inflight_fetch_src.remove(&req_id);
                }
            }
            RemoteFrame::FileBegin { req_id, total_len } => {
                // 仅控制端：建临时落地文件。total_len 存入 job 供状态栏进度，finalize 以 FileEnd 为准。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.fetch_begin(req_id, total_len);
                }
            }
            RemoteFrame::FileChunk { req_id, seq, data } => {
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.fetch_chunk(req_id, seq, &data);
                }
            }
            RemoteFrame::FileChunkAck { req_id, .. } => {
                // 仅被控端：放行源 worker 下一块（滑动窗口背压）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.fetch_src_ack(req_id);
                }
            }
            RemoteFrame::FileEnd { req_id } => {
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.fetch_end(req_id);
                }
            }
            RemoteFrame::FileErr { req_id, err } => {
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.fetch_abort(req_id, err);
                }
            }
            // ── part3c-2 片5 上传：Put*/MkDir*（按角色路由）─────────────────
            RemoteFrame::PutBegin {
                req_id,
                dir,
                name,
                total_len,
                overwrite,
            } => {
                // 仅被控端：建临时文件 + 撞名探测 + 子树断言（写盘服务）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_put_begin(req_id, dir, name, total_len, overwrite);
                }
            }
            RemoteFrame::PutReady {
                req_id,
                conflict,
                err,
            } => {
                // 仅控制端：决策开始发送 / 重发 Force / 暂停等覆盖。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.put_ready(req_id, conflict, err);
                }
            }
            RemoteFrame::PutChunk { req_id, seq, data } => {
                // 仅被控端：顺序写临时文件 + 回 PutChunkAck（背压）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_put_chunk(req_id, seq, &data);
                }
            }
            RemoteFrame::PutChunkAck { req_id, .. } => {
                // 仅控制端：放行 Put 源 worker 下一块（滑动窗口背压）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.put_src_ack(req_id);
                }
            }
            RemoteFrame::PutEnd { req_id } => {
                // 仅被控端：flush + 原子 rename 临时文件到目标。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_put_end(req_id);
                }
            }
            RemoteFrame::PutResult {
                req_id,
                status,
                err,
            } => {
                // 仅控制端：计统计 + 续推上传队列。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.put_result(req_id, status, err);
                }
            }
            RemoteFrame::MkDir { req_id, dir, name } => {
                // 仅被控端：建目录（幂等）+ 子树断言。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_mkdir(req_id, dir, name);
                }
            }
            RemoteFrame::MkDirResult { req_id, path, err } => {
                // 仅控制端：菜单「新建文件夹」的 MkDir（在 inflight_remote_fsop）→ 刷新目录；
                // 否则是上传遍历的 MkDir → 用返回路径递归上传其子项。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    if self.inflight_remote_fsop.contains_key(&req_id) {
                        self.finish_remote_fsop(req_id, err);
                    } else {
                        self.upload_mkdir_result(req_id, path, err);
                    }
                }
            }
            RemoteFrame::MkFile { req_id, dir, name } => {
                // 仅被控端：建空文件 + 子树断言。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_mkfile(req_id, dir, name);
                }
            }
            RemoteFrame::MkFileResult { req_id, err, .. } => {
                // 仅控制端：菜单「新建文件」完成 → 刷新目录。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.finish_remote_fsop(req_id, err);
                }
            }
            RemoteFrame::Delete {
                req_id,
                path,
                is_dir,
            } => {
                // 仅被控端：删文件 / 递归删目录。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.handle_delete(req_id, path, is_dir);
                }
            }
            RemoteFrame::DeleteResult { req_id, err } => {
                // 仅控制端：删除完成 → 刷新父目录（被删项消失）。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.finish_remote_fsop(req_id, err);
                }
            }
            // ── part3d 多会话 × 多窗格镜像（Phase 1 MVP：列表 + 订阅，单 mirror 只读）──────
            RemoteFrame::TabListSnapshot { tabs } => {
                // 仅控制端：整组替换远程会话列表（被控端按 K6 去重后才发，低频）。订阅会话若已不在
                // 新列表（被关）→ 按位置回退到邻位（被控端走 Snapshot-on-change，关会话经此路径生效）。
                if self.is_controlling() {
                    let old_idx = self
                        .subscribed_tab
                        .and_then(|s| self.remote_tabs.iter().position(|t| t.id == s));
                    self.remote_tabs = tabs;
                    self.fallback_subscription(old_idx);
                }
            }
            RemoteFrame::TabCreated { tab } => {
                // 仅控制端：插入 / 更新单个会话（MVP 被控端走 Snapshot-on-change，此为健壮兜底）。
                if self.is_controlling() {
                    match self.remote_tabs.iter_mut().find(|t| t.id == tab.id) {
                        Some(slot) => *slot = tab,
                        None => self.remote_tabs.push(tab),
                    }
                }
            }
            RemoteFrame::TabUpdated { tab } => {
                // 仅控制端：更新单个会话概览（不存在则插入，等价 TabCreated）。
                if self.is_controlling() {
                    match self.remote_tabs.iter_mut().find(|t| t.id == tab.id) {
                        Some(slot) => *slot = tab,
                        None => self.remote_tabs.push(tab),
                    }
                }
            }
            RemoteFrame::TabClosed { tab_id } => {
                // 仅控制端：移除会话；正订阅它则按位置回退到邻近会话（D6）。
                if self.is_controlling() {
                    let old_idx = self
                        .subscribed_tab
                        .and_then(|s| self.remote_tabs.iter().position(|t| t.id == s));
                    self.remote_tabs.retain(|t| t.id != tab_id);
                    self.fallback_subscription(old_idx);
                }
            }
            RemoteFrame::SubscribeSession { tab_id } => {
                // 仅被控端：记下订阅目标 + 置脏（main 复位 mirror_src 强制重发快照）。换订阅目标须复位
                // 比例同步基线——否则残留旧会话基线会让换订阅后被控端首次拖分隔条在新旧比例恰好近似时
                // 漏发 SubLayout（审计 BUG-1）。复位后据新会话当前比例重建双向同步。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    if self.sub_target != Some(tab_id) {
                        self.reset_sub_layout_baseline();
                    }
                    self.sub_target = Some(tab_id);
                    self.sub_dirty = true;
                    // 复位 RootChanged 去重基线：控制端订阅（含**重订同一会话**，如 Phase 3 切换后
                    // resubscribe_after_switch / 重点同一 tab）时会清空其文件树（根置 None →「等待 shell
                    // 上报路径」），被控端必须重推 RootChanged。否则同 cwd 被 remote_root_sent 去重跳过，
                    // 控制端文件树永久卡在等待、要切到别的 tab（cwd 变）才刷新（海风哥实测踩坑）。
                    self.remote_root_sent = None;
                }
            }
            RemoteFrame::SubscriptionStarted {
                tab_id,
                focused,
                panes,
                row_weights,
                col_weights,
                maximized,
            } => {
                // 仅控制端 且 与当前订阅目标一致（忽略换订阅在途的陈旧应答）。快照内联、先于任何
                // OutputWithId（D3 保序）。**混合**：1 窗格走单 mirror（保回看/选区，1+2 已验收）；
                // >1 窗格走 per-pane 镜像 Terminal + 布局（Phase 3c 多窗格渲染）。
                if self.is_controlling() && self.subscribed_tab == Some(tab_id) {
                    let mk_term = |p: &PaneSnapshot| {
                        let mut term = Terminal::new(
                            usize::from(p.rows).max(1),
                            usize::from(p.cols).max(1),
                            MIRROR_SCROLLBACK,
                        );
                        term.advance(&p.snapshot);
                        let _ = term.take_responses(); // 镜像无 PTY，排空应答。
                        term
                    };
                    // **统一**：1+ 窗格一律走 per-pane 路径（1:1 清晰 + per-pane 回看/选区/焦点），
                    // 消除单窗格「离屏按被控端网格像素 → shell 拉伸铺满 → 缩放锯齿」特例（海风哥反馈①）。
                    // 单 mirror 仅 0 窗格（空 tab）兜底——清空，不显示。
                    if panes.is_empty() {
                        self.mirror = None;
                        self.mirror_focus_sid = None;
                        self.mirror_panes.clear();
                        self.mirror_layout = None;
                        self.mirror_active_pane = None;
                        self.reset_history();
                    } else {
                        // 多窗格：清单 mirror + 回看态，建 per-pane 镜像 + 布局。
                        self.mirror = None;
                        self.mirror_focus_sid = None;
                        self.reset_history();
                        // 结构是否同上一份（窗格 id 顺序一致）——在重建 mirror_panes 之前比对旧的。
                        let same_structure = self
                            .mirror_panes
                            .iter()
                            .map(|mp| mp.session_id)
                            .eq(panes.iter().map(|p| p.session_id));
                        // 保留同 id 窗格的现有选区（纯尺寸/内容重发不丢选区）；新窗格选区为 None。
                        let mut old_sel: std::collections::HashMap<SessionId, Selection> = self
                            .mirror_panes
                            .drain(..)
                            .filter_map(|mp| mp.selection.map(|s| (mp.session_id, s)))
                            .collect();
                        self.mirror_panes = panes
                            .iter()
                            .map(|p| MirrorPane {
                                session_id: p.session_id,
                                term: mk_term(p),
                                selection: old_sel.remove(&p.session_id),
                                hist_base: p.base,
                                hist_screen_top: p.screen_top,
                            })
                            .collect();
                        // 比例布局：**结构变（增删窗格）或首帧** → 据被控端权重重建布局并复位同步基线
                        // （SubLayout 下一帧据此重建双向同步）；**结构同**（纯尺寸/内容重发）→ 保留控制端
                        // 当前镜像比例（增量比例同步走 SubLayout，不在此读 SubscriptionStarted 权重，
                        // 否则连续拖动会被自己的回送快照打架）。仅同步最大化结构。
                        if !same_structure || self.mirror_layout.is_none() {
                            let n = panes.len();
                            let layout = PaneLayout::from_weights(n, &row_weights, &col_weights)
                                .unwrap_or_else(|| PaneLayout::uniform(n));
                            self.mirror_layout = Some(MirrorLayout { layout, maximized });
                            self.sub_layout_baseline = None; // 据新结构当前比例重新建立 SubLayout 同步。
                        } else if let Some(l) = self.mirror_layout.as_mut() {
                            l.maximized = maximized; // 比例保留，仅同步最大化结构。
                        }
                        // 控制端焦点窗格（Phase 4）：现存焦点仍在新窗格集里则保留（结构同的纯尺寸重发
                        // 不夺焦点），否则初始化为被控端焦点窗格（panes[focused]，合理默认；之后控制端
                        // 点击改、不回报被控端）。回看边界取**焦点窗格**的 (base, screen_top) 重建。
                        let keep = self
                            .mirror_active_pane
                            .filter(|sid| panes.iter().any(|p| p.session_id == *sid));
                        let active = keep.or_else(|| {
                            panes
                                .get(focused as usize)
                                .or_else(|| panes.first())
                                .map(|p| p.session_id)
                        });
                        self.mirror_active_pane = active;
                        if let Some(ap) = active.and_then(|sid| panes.iter().find(|p| p.session_id == sid)) {
                            self.hist_bounds = Some((ap.base, ap.screen_top));
                        }
                    }
                }
            }
            RemoteFrame::OutputWithId {
                tab_id,
                session_id,
                data,
            } => {
                // 仅控制端 且 属当前订阅会话。多窗格按 session_id 路由到对应窗格镜像；单窗格只认
                // 焦点窗格那一路（被控端 tee 全部窗格，避免非焦点输出串入单 mirror）。
                if self.is_controlling() && self.subscribed_tab == Some(tab_id) {
                    if let Some(mp) =
                        self.mirror_panes.iter_mut().find(|mp| mp.session_id == session_id)
                    {
                        mp.term.advance(&data);
                        let _ = mp.term.take_responses();
                    } else if self.mirror_focus_sid == Some(session_id) {
                        if let Some(mirror) = self.mirror.as_mut() {
                            mirror.advance(&data);
                            let _ = mirror.take_responses();
                        }
                    }
                }
            }
            RemoteFrame::ResizeWithId {
                tab_id,
                session_id,
                rows,
                cols,
            } => {
                // 仅控制端 且 属当前订阅会话：多窗格路由 resize 对应窗格；单窗格焦点那一路 + 复位回看。
                // （被控端实际走 mirror_src 几何签名变 → 整屏 SubscriptionStarted 重发，此臂多为休眠兜底。）
                if self.is_controlling() && self.subscribed_tab == Some(tab_id) {
                    if let Some(mp) =
                        self.mirror_panes.iter_mut().find(|mp| mp.session_id == session_id)
                    {
                        mp.term
                            .resize(usize::from(rows).max(1), usize::from(cols).max(1));
                    } else if self.mirror_focus_sid == Some(session_id) {
                        if let Some(mirror) = self.mirror.as_mut() {
                            mirror.resize(usize::from(rows).max(1), usize::from(cols).max(1));
                        }
                        self.reset_history();
                    }
                }
            }
            // ── part3d Phase 2 远程增删会话（需求 d）──────────────────────────────────
            RemoteFrame::NewTab { req_id } => {
                // 仅被控端：入队，main spawn 新 tab（不夺被控端焦点）后回 NewTabResult。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_new_tab.push(req_id);
                }
            }
            RemoteFrame::CloseTab { req_id, tab_id } => {
                // 仅被控端：入队，main 关 tab（拒绝关最后一个）后回 CloseTabResult。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_close_tab.push((req_id, tab_id));
                }
            }
            RemoteFrame::SubViewport { tab_id, panes } => {
                // 仅被控端：记下控制端请求的各窗格目标尺寸；main 在该会话为后台 tab 时 resize 其窗格
                // （Phase 3 尺寸同步；前台由被控端窗口接管）。只留最新一份。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controlled)) {
                    self.pending_sub_viewport = Some((
                        tab_id,
                        panes
                            .into_iter()
                            .map(|p| (p.session_id, p.rows, p.cols))
                            .collect(),
                    ));
                }
            }
            RemoteFrame::SubLayout {
                tab_id,
                row_weights,
                col_weights,
            } => {
                // 任一端（会话中）：记下对端比例；main 取走后应用到本端布局并更新基线免回声。只留最新一份。
                if self.session.is_some() {
                    self.pending_sub_layout = Some((tab_id, row_weights, col_weights));
                }
            }
            RemoteFrame::NewTabResult {
                req_id: _,
                tab_id,
                err,
            } => {
                // 仅控制端：成功则自动订阅查看新会话；失败弹 toast（如已达上限）。
                if self.is_controlling() {
                    match (tab_id, err) {
                        (Some(id), None) => self.subscribe_tab(id),
                        (_, Some(e)) => {
                            log::warn!("远程新建会话失败: {e:?}");
                            self.notices.push(Notice::RemoteNewTabFailed(e));
                        }
                        (None, None) => log::warn!("远程新建会话返回空（既无 id 也无错误）"),
                    }
                }
            }
            RemoteFrame::CloseTabResult {
                req_id: _,
                tab_id,
                err,
            } => {
                // 仅控制端：失败弹 toast；成功则列表 / 订阅回退由后续 TabListSnapshot 驱动。
                if self.is_controlling() {
                    if let Some(e) = err {
                        log::warn!("远程关闭会话 {tab_id} 失败: {e:?}");
                        self.notices.push(Notice::RemoteCloseTabFailed(e));
                    }
                }
            }
            RemoteFrame::Echo(_) => {}
            // M6 P2P 直连信令：解析 payload（候选 + 证书 + nonce）后转给 P2P 打洞状态机
            // （交换候选/证书、协商建立 QUIC 直连或回退中继）。未起引擎 / payload 非法则丢弃。
            RemoteFrame::P2pSignal { kind, payload } => {
                if let Some(p2p) = &self.p2p {
                    match serde_json::from_str::<SignalPayload>(&payload) {
                        Ok(sp) => p2p.peer_signal(kind, sp),
                        Err(e) => log::debug!("P2pSignal payload 解析失败，丢弃: {e}"),
                    }
                }
            }
            // P2P 直连数据面首帧（仅解除对端 accept_bi 阻塞 + 标记流就绪）：收端 no-op。
            RemoteFrame::P2pStreamHello => {}
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
                self.reset_multi_session();
                self.clear_remote_filetree();
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
            RemoteS2C::Welcome {
                protocol_version,
                min_supported_version,
                ..
            } => {
                // K2 版本门：part3d 双 id 数据面与旧 v1 不兼容。本端协议版本低于服务端要求的
                // 最低版本 → 本端过旧，远程镜像会因帧不识别而空白，须升级（两端须同为 ≥2）。
                if lumen_protocol::PROTOCOL_VERSION < min_supported_version {
                    log::warn!(
                        "本端协议版本 {} 低于服务端要求的最低版本 {}，远程多会话镜像可能不可用，请升级",
                        lumen_protocol::PROTOCOL_VERSION,
                        min_supported_version
                    );
                } else if protocol_version < lumen_protocol::MIN_SUPPORTED_VERSION {
                    log::warn!(
                        "服务端协议版本 {} 低于本端要求的最低版本 {}，远程功能可能不可用",
                        protocol_version,
                        lumen_protocol::MIN_SUPPORTED_VERSION
                    );
                }
            }
            RemoteS2C::Pong => {}
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
                self.clear_remote_filetree();
                self.mirror = (role == Role::Controller)
                    .then(|| Terminal::new(MIRROR_INIT_ROWS, MIRROR_INIT_COLS, MIRROR_SCROLLBACK));
                self.session = Some(ActiveSession { peer_name, role });
                // M6：会话建立 → 启动 P2P 打洞引擎（控制端发 Offer，被控端等 Offer 回 Answer）。
                // 中继不受影响：直连是叠加加速层，打洞失败/未切前一切走中继。传入主线程唤醒句柄——
                // P2P 收到数据帧后须 nudge 主线程重绘（漏传则按需重绘 UI 回显延迟数秒）。
                self.p2p = Some(P2pEngine::start(
                    role,
                    stun_host_from_server(),
                    self.ctx.clone(),
                    self.proxy.clone(),
                    self.wake_pending.clone(),
                ));
                self.notices.push(Notice::SessionStarted { role, peer });
            }
            RemoteS2C::SessionEnded { reason } => {
                self.session = None;
                self.mirror = None;
                self.pending_input.clear();
                self.pending_viewport = None;
                self.pending_history.clear();
                self.reset_history();
                self.reset_multi_session();
                self.clear_remote_filetree();
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

/// 后台线程主体：连接 → 跑读写循环 → 断线退避重连，直到 `stop`。每次（重）连读共享 token 的
/// **当前值**——心跳 worker 自动续期后写回同一句柄，重连即用新 token（免 7 天到期后 WS 401 连不上）。
fn worker(
    token: &Arc<RwLock<String>>,
    cmd_rx: &Receiver<RemoteC2S>,
    evt_tx: &Sender<WsEvent>,
    stop: &Arc<AtomicBool>,
    ctx: &egui::Context,
    proxy: &EventLoopProxy<PtyWake>,
    wake_pending: &Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        let tok = token.read().map(|g| g.clone()).unwrap_or_default();
        match connect_ws(&tok) {
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

/// 从 server 基址推导 STUN 反射端地址（同主机、UDP 8788）：`http://host:8787` → `host:8788`。
/// 用于 P2P 端点发现（自建 STUN 反射，见 `server/lumen-server/src/stun.rs`，默认端口 8788；
/// 若 server 经 `LUMEN_STUN_BIND_ADDR` 改端口，此处需同步——Phase 2 先用默认端口）。
fn stun_host_from_server() -> String {
    let url = server_url();
    let after_scheme = url.split("://").nth(1).unwrap_or(url.as_str());
    let host = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = host.split(':').next().unwrap_or(host);
    format!("{host}:8788")
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
    fn 读目录队列满_回err不空挂() {
        // review MED-1 有界队列：满即回 FsErr，控制端清 is_pending、显式失败，而非永久空挂。
        let mut ws = RemoteWs::default();
        // 容量 1、无 worker 消费：第 1 个填满，第 2 个必被拒。`_job_rx` 须存活（否则通道断）。
        let (job_tx, _job_rx) = std::sync::mpsc::sync_channel::<DirJob>(1);
        let (svc_tx, svc_rx) = std::sync::mpsc::channel();
        ws.dir_job_tx = Some(job_tx);
        ws.svc_tx = Some(svc_tx);
        ws.spawn_list_dir(1, "/q".into(), false); // 入队（占满）
        ws.spawn_list_dir(2, "/q".into(), false); // 队满 → 回 err
        let replies: Vec<_> = svc_rx.try_iter().collect();
        assert_eq!(replies.len(), 1, "仅被拒的第 2 个回 err");
        match &replies[0] {
            SvcReply::ListDir { req_id, err, entries, .. } => {
                assert_eq!(*req_id, 2);
                assert!(err.is_some(), "满队列回 FsErr");
                assert!(entries.is_empty());
            }
            _ => panic!("应为 ListDir err"),
        }
    }

    #[test]
    fn build_list_dir_reply_可读目录与缺失路径() {
        let base = std::env::temp_dir().join(format!("lumen_dirsvc_t_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let _ = std::fs::write(base.join("probe.txt"), b"x");
        // 可读目录：无 err、能列到文件、req_id 透传。
        let reply = build_list_dir_reply(11, base.to_string_lossy().into_owned(), false);
        match reply {
            SvcReply::ListDir { req_id, entries, err, .. } => {
                assert_eq!(req_id, 11);
                assert!(err.is_none(), "可读目录不应 err");
                assert!(entries.iter().any(|e| e.name == "probe.txt"), "应列到 probe.txt");
            }
            _ => panic!("应为 ListDir"),
        }
        let missing = base.join("nope_sub");
        let _ = std::fs::remove_dir_all(&base);
        // 缺失路径：err。
        let reply = build_list_dir_reply(12, missing.to_string_lossy().into_owned(), false);
        match reply {
            SvcReply::ListDir { req_id, entries, err, .. } => {
                assert_eq!(req_id, 12);
                assert!(err.is_some(), "缺失路径应 err");
                assert!(entries.is_empty());
            }
            _ => panic!("应为 ListDir"),
        }
    }

    #[test]
    fn enforce_dir_byte_cap_超额删最旧_未超不动() {
        let dir = std::env::temp_dir().join(format!("lumen_open_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试夹");
        let count = |d: &std::path::Path| {
            std::fs::read_dir(d)
                .map(|rd| rd.flatten().filter(|e| e.path().is_file()).count())
                .unwrap_or(0)
        };
        let total_bytes = |d: &std::path::Path| {
            std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.metadata().ok())
                        .filter(|m| m.is_file())
                        .map(|m| m.len())
                        .sum::<u64>()
                })
                .unwrap_or(0)
        };
        // 3 个各 100 字节 = 300 总。写入顺序即 mtime 近似顺序（最旧 f0）。
        for i in 0..3u8 {
            std::fs::write(dir.join(format!("f{i}.bin")), vec![0u8; 100]).expect("写文件");
        }
        // 上限充裕：不动。
        enforce_dir_byte_cap(&dir, 1000);
        assert_eq!(count(&dir), 3, "未超上限不应删");
        // 上限 250：300>250 → 删到 ≤250（删 1 个 → 200）。
        enforce_dir_byte_cap(&dir, 250);
        assert!(total_bytes(&dir) <= 250, "应淘汰到 ≤ 上限");
        assert_eq!(count(&dir), 2, "300→250 上限应恰删最旧 1 个");
        // 上限 0：全删（best-effort，无占用故应清空）。
        enforce_dir_byte_cap(&dir, 0);
        assert_eq!(count(&dir), 0, "上限 0 应清空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transfer_status_聚合下载字节_排除剪贴板() {
        let mk = |kind, name: &str, written, total| FetchJob {
            kind,
            src_path: "/x".into(),
            name: name.into(),
            dest: None,
            file: None,
            next_seq: 0,
            written,
            total,
            last_at: Instant::now(),
            clip_stream: None,
        };
        let mut ws = RemoteWs::default();
        assert!(ws.transfer_status().is_none(), "无传输应 None");
        ws.inflight_fetch
            .insert(1, mk(FetchKind::Download, "a.bin", 50, 100));
        ws.inflight_fetch
            .insert(2, mk(FetchKind::Open, "b.bin", 30, 200));
        // 剪贴板流：不计入下载、字节、名字轮换。
        ws.inflight_fetch
            .insert(3, mk(FetchKind::Clipboard, "c.bin", 999, 999));
        let ts = ws.transfer_status().expect("有传输");
        assert_eq!(ts.downloads, 2, "剪贴板不计入下载数");
        assert_eq!(ts.uploads, 0);
        assert_eq!(ts.down_done, 80, "50+30，剪贴板 999 不计");
        assert_eq!(ts.down_total, 300, "100+200");
        assert_eq!(ts.downloads + ts.uploads, 2);
        let r = ts.down_ratio().expect("有总字节");
        assert!((r - 80.0 / 300.0).abs() < 1e-6, "聚合比 = 80/300");
        assert!(
            ts.names.contains(&"a.bin".to_string()) && ts.names.contains(&"b.bin".to_string()),
            "下载名入轮换"
        );
        assert!(!ts.names.contains(&"c.bin".to_string()), "剪贴板名不入轮换");
        // 总字节未知（FileBegin 未到，total=0）→ 进度条不定态（ratio None）。
        let mut ws2 = RemoteWs::default();
        ws2.inflight_fetch
            .insert(9, mk(FetchKind::Download, "d.bin", 10, 0));
        assert!(
            ws2.transfer_status().expect("有传输").down_ratio().is_none(),
            "total=0 应不定态"
        );
    }

    #[test]
    fn p2p_link_state_门控引擎存在_按active分直连中继() {
        let mut ws = RemoteWs::default();
        assert!(ws.p2p_link_state().is_none(), "无 P2P 引擎应 None");
        // 门控在引擎存在：无引擎即便 active 也 None。
        ws.p2p_data_active = true;
        assert!(ws.p2p_link_state().is_none(), "无引擎即便 active 也 None");
        // 有引擎：active → Direct，否则 Relay。
        ws.p2p = Some(P2pEngine::start(Role::Controller, String::new(), None, None, None));
        ws.p2p_data_active = true;
        assert_eq!(ws.p2p_link_state(), Some(P2pLink::Direct));
        ws.p2p_data_active = false;
        assert_eq!(ws.p2p_link_state(), Some(P2pLink::Relay));
        // drop ws → P2pEngine Drop 停后台线程。
    }

    #[test]
    fn build_list_dir_recursive_reply_缺失路径回err() {
        let missing = std::env::temp_dir().join("lumen_dirsvc_nonexistent_xyz_42");
        let _ = std::fs::remove_dir_all(&missing); // 保证不存在
        let reply = build_list_dir_recursive_reply(13, missing.to_string_lossy().into_owned(), false);
        match reply {
            SvcReply::ListDirRecursive { req_id, entries, truncated, err, .. } => {
                assert_eq!(req_id, 13);
                assert!(err.is_some(), "缺失路径 read_dir 失败应回 err");
                assert!(entries.is_empty() && !truncated);
            }
            _ => panic!("应为 ListDirRecursive"),
        }
    }

    #[test]
    fn 远程树_换根_按需listdir_双键去重() {
        let mut ft = RemoteFileTree::default();
        assert!(ft.visible_rows().is_empty(), "无根无行");
        // 换根：根 id 0、默认展开、整体重置；同根不重置。
        assert!(ft.set_root("/r".into()));
        assert!(!ft.set_root("/r".into()), "同根不重置");
        let rows = ft.visible_rows();
        assert_eq!(rows.len(), 2, "根 + 临时加载占位（开但未缓存非 pending）");
        assert!(matches!(rows[0].kind, RemoteRowKind::Dir { open: true }));
        assert_eq!(rows[0].name, "r");
        // 标记根在途 → 可见行为 根 + Loading。
        ft.mark_pending(0, 7);
        assert!(matches!(ft.visible_rows()[1].kind, RemoteRowKind::Loading));
        // 陈旧 req_id 应答被双键丢弃（仍 pending）。
        ft.apply_dir_entries(
            999,
            "/r",
            vec![DirEntry {
                path: "/r/a".into(),
                name: "a".into(),
                is_dir: false,
                size: 100,
            }],
            0,
            None,
        );
        assert!(ft.is_pending(0), "req_id 不匹配 → 仍 pending");
        // 正确 req_id：填充子项（目录在前 + 文件 + 溢出占位）。
        ft.apply_dir_entries(
            7,
            "/r",
            vec![
                DirEntry {
                    path: "/r/sub".into(),
                    name: "sub".into(),
                    is_dir: true,
                    size: 0,
                },
                DirEntry {
                    path: "/r/a.txt".into(),
                    name: "a.txt".into(),
                    is_dir: false,
                    size: 50,
                },
            ],
            3,
            None,
        );
        assert!(!ft.is_pending(0));
        let rows = ft.visible_rows();
        assert_eq!(rows.len(), 4, "根 + sub + a.txt + 溢出");
        assert!(matches!(rows[1].kind, RemoteRowKind::Dir { open: false }));
        assert_eq!(rows[1].name, "sub");
        assert!(matches!(rows[2].kind, RemoteRowKind::File));
        assert!(matches!(rows[3].kind, RemoteRowKind::Overflow(3)));
        // 展开 sub（DFS find 可达）：填其 listing，x 在 depth 2 可见。
        let sub_id = rows[1].id;
        ft.set_open(sub_id, true);
        ft.mark_pending(sub_id, 8);
        ft.apply_dir_entries(
            8,
            "/r/sub",
            vec![DirEntry {
                path: "/r/sub/x".into(),
                name: "x".into(),
                is_dir: false,
                size: 10,
            }],
            0,
            None,
        );
        let rows = ft.visible_rows();
        assert_eq!(rows.len(), 5, "根 + sub(开) + x + a.txt + 溢出");
        assert_eq!(rows[2].name, "x");
        assert_eq!(rows[2].depth, 2);
        // 折叠 sub（纯本地）：x 不再可见。
        ft.set_open(sub_id, false);
        assert_eq!(ft.visible_rows().len(), 4);
    }

    #[test]
    fn 远程树_读失败占位_显示隐藏重列() {
        let mut ft = RemoteFileTree::default();
        ft.set_root("/r".into());
        ft.mark_pending(0, 1);
        ft.apply_dir_entries(1, "/r", Vec::new(), 0, Some(FsErr::PermissionDenied));
        assert!(matches!(
            ft.visible_rows()[1].kind,
            RemoteRowKind::Unreadable
        ));
        // 切「显示隐藏项」：重列（清缓存、回根），返回 true；同值不重列。
        assert!(ft.set_show_hidden(true));
        assert!(ft.show_hidden());
        assert!(!ft.has_listing(0), "重列清缓存");
        assert!(ft.is_open(0), "重列后根仍默认展开");
        assert!(!ft.set_show_hidden(true), "同值不重列");
    }

    #[test]
    fn fetch_basename_清洗() {
        assert_eq!(sanitize_basename("a.txt"), "a.txt");
        assert_eq!(sanitize_basename("report-v2.md"), "report-v2.md");
        // 非 [alnum._-] 一律换 _（路径分隔符 / 通配符 / CJK）。
        assert_eq!(sanitize_basename("x/y\\z?.exe"), "x_y_z_.exe");
        assert_eq!(sanitize_basename("中文.txt"), "__.txt");
        assert_eq!(sanitize_basename(""), "file");
    }

    #[test]
    fn fetch_io错误映射() {
        use std::io::{Error, ErrorKind};
        assert_eq!(io_err_to_fs(&Error::from(ErrorKind::NotFound)), FsErr::NotFound);
        assert_eq!(
            io_err_to_fs(&Error::from(ErrorKind::PermissionDenied)),
            FsErr::PermissionDenied
        );
        assert_eq!(io_err_to_fs(&Error::from(ErrorKind::Other)), FsErr::Io);
    }

    #[test]
    fn fetch_控制端写临时文件_乱序中止() {
        // 直接驱动控制端接收态（不经线程 / WS）：start → begin → 按序块 → 乱序块中止。
        let mut ws = RemoteWs::default();
        ws.start_fetch_open("/some/dir/a.txt".into()); // send_frame 无 cmd_tx → no-op
        let req = *ws.inflight_fetch.keys().next().expect("有在途 Fetch");
        ws.fetch_begin(req, 0);
        let tmp = ws
            .inflight_fetch
            .get(&req)
            .and_then(|j| j.dest.clone())
            .expect("FileBegin 建了临时文件");
        assert!(tmp.exists(), "临时文件已创建");
        // 按序两块顺序写入。
        ws.fetch_chunk(req, 0, b"hello ");
        ws.fetch_chunk(req, 1, b"world");
        assert_eq!(ws.inflight_fetch.get(&req).map(|j| j.next_seq), Some(2));
        // 乱序块（seq=3，期望 2）→ 中止：移除在途 + 删半成品 + FetchFailed 通知。
        ws.fetch_chunk(req, 3, b"X");
        assert!(!ws.inflight_fetch.contains_key(&req), "中止后移除在途");
        assert!(!tmp.exists(), "中止删半成品临时文件");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::FetchFailed(_))),
            "应弹 FetchFailed"
        );
    }

    #[test]
    fn 下载_单文件_落地与完成汇总() {
        // start_download 守卫 is_controlling：测试需置控制中会话。
        let mut ws = RemoteWs {
            session: Some(ActiveSession {
                peer_name: "peer".into(),
                role: Role::Controller,
            }),
            ..Default::default()
        };
        let base = std::env::temp_dir().join(format!("lumen_dl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建测试目录");
        // 下载一个不冲突的文件（覆盖模式）：入队 + pump 起一个 Download fetch。
        ws.start_download(
            vec![ClipItem {
                path: "/remote/a.txt".into(),
                name: "a.txt".into(),
                is_dir: false,
            }],
            base.display().to_string(),
            true,
        );
        let req = *ws.inflight_fetch.keys().next().expect("有下载 fetch");
        // 模拟传输：begin（建目标文件）→ chunk → end（落地 + 计完成）。
        ws.fetch_begin(req, 0);
        ws.fetch_chunk(req, 0, b"hello");
        ws.fetch_end(req);
        let target = base.join("a.txt");
        let content = std::fs::read(&target).expect("目标文件应落地");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(content, b"hello", "下载内容正确");
        assert!(ws.download.is_none(), "完成后清空 download");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::DownloadDone { done: 1, errors: 0, .. })),
            "应弹 DownloadDone(done=1)"
        );
    }

    #[test]
    fn 下载_不覆盖时跳过已存在() {
        let mut ws = RemoteWs {
            session: Some(ActiveSession {
                peer_name: "peer".into(),
                role: Role::Controller,
            }),
            ..Default::default()
        };
        let base = std::env::temp_dir().join(format!("lumen_dlskip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建测试目录");
        std::fs::write(base.join("exist.txt"), b"old").expect("写已存在文件");
        // 不覆盖 + 目标已存在 → 跳过，不起 fetch，立即完成。
        ws.start_download(
            vec![ClipItem {
                path: "/remote/exist.txt".into(),
                name: "exist.txt".into(),
                is_dir: false,
            }],
            base.display().to_string(),
            false,
        );
        assert!(ws.inflight_fetch.is_empty(), "跳过不起 fetch");
        assert!(ws.download.is_none(), "立即完成");
        let kept = std::fs::read(base.join("exist.txt")).expect("旧文件仍在");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(kept, b"old", "跳过未动旧文件");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::DownloadDone { skipped: 1, .. })),
            "应弹 DownloadDone(skipped=1)"
        );
    }

    #[test]
    fn 剪贴板_远程侧会话结束清空_本地侧保留() {
        let mut ws = RemoteWs::default();
        ws.set_file_clipboard(
            ClipSide::Remote,
            vec![ClipItem {
                path: "/r/x".into(),
                name: "x".into(),
                is_dir: false,
            }],
        );
        assert!(ws.file_clipboard().is_some());
        ws.clear_remote_filetree();
        assert!(ws.file_clipboard().is_none(), "Remote 侧会话结束清空");
        ws.set_file_clipboard(
            ClipSide::Local,
            vec![ClipItem {
                path: "/l/y".into(),
                name: "y".into(),
                is_dir: false,
            }],
        );
        ws.clear_remote_filetree();
        assert!(ws.file_clipboard().is_some(), "Local 侧保留");
    }

    #[test]
    fn 上传_被控端写盘_原子rename落地() {
        // 直接驱动被控端 Put 接收态（不经线程 / WS）：PutBegin(Force) → chunk → end，验证目标
        // 文件内容（send_frame 无 cmd_tx → 静默 no-op）。
        let mut ws = RemoteWs::default();
        let dir = std::env::temp_dir().join(format!("lumen_put_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let req = 42u64;
        // Force：建临时文件 + 存 inflight_put。
        ws.handle_put_begin(req, dir.display().to_string(), "f.txt".into(), 11, PutOverwrite::Force);
        let tmp = ws
            .inflight_put
            .get(&req)
            .map(|j| j.tmp_path.clone())
            .expect("PutBegin(Force) 建了临时文件");
        assert!(tmp.exists(), "临时文件已创建");
        // 顺序两块写入。
        ws.handle_put_chunk(req, 0, b"hello ");
        ws.handle_put_chunk(req, 1, b"world");
        assert_eq!(ws.inflight_put.get(&req).map(|j| j.next_seq), Some(2));
        // PutEnd：flush + 原子 rename 到目标。
        ws.handle_put_end(req);
        assert!(!ws.inflight_put.contains_key(&req), "end 后移除在途");
        assert!(!tmp.exists(), "rename 后临时文件不在");
        let target = dir.join("f.txt");
        let content = std::fs::read(&target).expect("目标文件应落地");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(content, b"hello world", "上传内容正确落地");
    }

    #[test]
    fn 上传_被控端_路径穿越被拒() {
        let mut ws = RemoteWs::default();
        let dir = std::env::temp_dir().join(format!("lumen_put_trav_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        // name=".." → validate_entry_name 拒 → 不建临时文件（PutReady{err} 经 send_frame no-op）。
        ws.handle_put_begin(1, dir.display().to_string(), "..".into(), 0, PutOverwrite::Force);
        assert!(ws.inflight_put.is_empty(), "穿越名（..）被拒，不建临时");
        // name 含分隔符 → 同样被拒。
        ws.handle_put_begin(2, dir.display().to_string(), "a/b".into(), 0, PutOverwrite::Force);
        assert!(ws.inflight_put.is_empty(), "穿越名（a/b）被拒，不建临时");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 上传_被控端_probe撞名与skip不写() {
        let mut ws = RemoteWs::default();
        let dir = std::env::temp_dir().join(format!("lumen_put_conf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        std::fs::write(dir.join("exist.txt"), b"old").expect("写已存在文件");
        // Probe 命中已存在 → 回 conflict、不建临时、不写。
        ws.handle_put_begin(1, dir.display().to_string(), "exist.txt".into(), 3, PutOverwrite::Probe);
        assert!(ws.inflight_put.is_empty(), "Probe 撞名不建临时文件");
        assert_eq!(
            std::fs::read(dir.join("exist.txt")).expect("旧文件仍在"),
            b"old",
            "Probe 不动旧文件"
        );
        // Skip → 直接 PutResult{Skipped}、不写。
        ws.handle_put_begin(2, dir.display().to_string(), "exist.txt".into(), 3, PutOverwrite::Skip);
        assert!(ws.inflight_put.is_empty(), "Skip 不建临时文件");
        assert_eq!(
            std::fs::read(dir.join("exist.txt")).expect("旧文件仍在"),
            b"old",
            "Skip 不动旧文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 上传_被控端_mkdir幂等并回带路径() {
        let mut ws = RemoteWs::default();
        let dir = std::env::temp_dir().join(format!("lumen_mkdir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        // 首建成功。
        ws.handle_mkdir(1, dir.display().to_string(), "sub".into());
        let created = dir.join("sub");
        assert!(created.is_dir(), "create_dir 落地");
        // 再建同名 → AlreadyExists 视为成功（幂等，不 panic、目录仍在）。
        ws.handle_mkdir(2, dir.display().to_string(), "sub".into());
        assert!(created.is_dir(), "幂等：已存在仍视为成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 构造一个「控制中」会话的 `RemoteWs`（上传编排守卫 is_controlling）。
    fn controlling_ws() -> RemoteWs {
        RemoteWs {
            session: Some(ActiveSession {
                peer_name: "peer".into(),
                role: Role::Controller,
            }),
            ..RemoteWs::default()
        }
    }

    #[test]
    fn 复制目录_descriptor带根目录名前缀和顶层项() {
        let mut ws = controlling_ws();
        ws.start_clip_dir("/remote/myfolder".into(), "myfolder".into());
        let req_id = ws.inflight_clip_dir.as_ref().expect("在途枚举").0;
        ws.apply_list_dir_recursive(
            req_id,
            "/remote/myfolder".into(),
            vec![
                RecursiveDirEntry {
                    rel_path: "a.txt".into(),
                    path: "/remote/myfolder/a.txt".into(),
                    is_dir: false,
                    size: 3,
                },
                RecursiveDirEntry {
                    rel_path: "sub".into(),
                    path: "/remote/myfolder/sub".into(),
                    is_dir: true,
                    size: 0,
                },
                RecursiveDirEntry {
                    rel_path: "sub/b.txt".into(),
                    path: "/remote/myfolder/sub/b.txt".into(),
                    is_dir: false,
                    size: 5,
                },
            ],
            false,
            None,
        );
        let ready = ws.take_clip_dir_ready().expect("应有就绪子树");
        let rels: Vec<&str> = ready.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(
            rels,
            vec!["myfolder", "myfolder/a.txt", "myfolder/sub", "myfolder/sub/b.txt"],
            "顶层项 + 全部子项套根目录名前缀（含顶层文件夹本身）"
        );
        assert!(ready[0].is_dir, "顶层项是目录");
        assert_eq!(ready[0].path, "/remote/myfolder", "顶层项 abs = 根路径");
        assert_eq!(ready[1].path, "/remote/myfolder/a.txt", "文件项 fetch key 不变");
    }

    #[test]
    fn 复制空目录_仍粘出顶层空文件夹() {
        let mut ws = controlling_ws();
        ws.start_clip_dir("/remote/empty".into(), "empty".into());
        let req_id = ws.inflight_clip_dir.as_ref().expect("在途").0;
        ws.apply_list_dir_recursive(req_id, "/remote/empty".into(), vec![], false, None);
        let ready = ws.take_clip_dir_ready().expect("空目录也应有顶层项");
        assert_eq!(ready.len(), 1, "仅顶层目录项");
        assert_eq!(ready[0].rel_path, "empty");
        assert!(ready[0].is_dir);
    }

    #[test]
    fn 复制目录_陈旧应答被丢弃() {
        let mut ws = controlling_ws();
        ws.start_clip_dir("/r/d".into(), "d".into());
        // 不匹配 req_id 的应答 → 忽略，不产生就绪子树、在途枚举仍保留。
        ws.apply_list_dir_recursive(99999, "/r/d".into(), vec![], false, None);
        assert!(ws.take_clip_dir_ready().is_none(), "陈旧 req_id 应答不产生就绪子树");
        assert!(ws.inflight_clip_dir.is_some(), "在途枚举仍保留");
    }

    #[test]
    fn 复制目录_枚举失败弹failed() {
        let mut ws = controlling_ws();
        ws.start_clip_dir("/r/d".into(), "d".into());
        let req_id = ws.inflight_clip_dir.as_ref().expect("在途").0;
        ws.apply_list_dir_recursive(
            req_id,
            "/r/d".into(),
            vec![],
            false,
            Some(FsErr::PermissionDenied),
        );
        assert!(ws.take_clip_dir_ready().is_none(), "失败不产生就绪子树");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::ClipDirFailed)),
            "应弹 ClipDirFailed"
        );
    }

    #[test]
    fn 上传_控制端_单文件起编排() {
        let mut ws = controlling_ws();
        // 起单文件上传：入 file_queue → pump 起一个 PutBegin（Probe），记 inflight_put_meta。
        ws.start_upload(
            vec![ClipItem {
                path: "C:\\local\\a.txt".into(),
                name: "a.txt".into(),
                is_dir: false,
            }],
            "/remote/dst".into(),
        );
        assert!(ws.upload.is_some(), "上传编排已建");
        assert_eq!(ws.inflight_put_meta.len(), 1, "发了一个 PutBegin、记一项元信息");
        let meta = ws.inflight_put_meta.values().next().expect("有元信息");
        assert_eq!(meta.name, "a.txt");
        assert_eq!(meta.remote_dir, "/remote/dst");
        assert_eq!(ws.upload.as_ref().map(|u| u.active_puts), Some(1), "active_puts=1");
        // 应弹 UploadStarted。
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::UploadStarted)),
            "应弹 UploadStarted"
        );
    }

    #[test]
    fn 上传_控制端_撞名暂停等覆盖() {
        let mut ws = controlling_ws();
        ws.start_upload(
            vec![ClipItem {
                path: "C:\\local\\a.txt".into(),
                name: "a.txt".into(),
                is_dir: false,
            }],
            "/remote/dst".into(),
        );
        let req = *ws.inflight_put_meta.keys().next().expect("有在途 Put");
        // 被控端探得撞名（conflict）且 policy=None → 暂停，等覆盖模态。
        ws.put_ready(req, Some(PutConflict { is_dir: false, existing_len: 9 }), None);
        assert_eq!(ws.upload_conflict_count(), Some(1), "暂停 → 弹覆盖模态");
        // 用户选跳过 → policy=Some(false)、计跳过、清待决，编排完成。
        ws.resolve_upload_conflict(crate::shell::OverwriteChoice::Skip);
        assert!(ws.upload.is_none(), "跳过后无其它文件 → 编排完成");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::UploadDone { skipped: 1, done: 0, .. })),
            "应弹 UploadDone(skipped=1)"
        );
    }

    #[test]
    fn 上传_多文件同时撞名_不互相覆盖() {
        // H1 回归：policy=None 时多个 PutReady{conflict} 并发回来必须都进队列、互不覆盖，
        // 否则被覆盖的 req 永占 active_puts → 上传永久挂死。
        let mut ws = controlling_ws();
        ws.start_upload(
            vec![
                ClipItem {
                    path: "C:\\local\\a.txt".into(),
                    name: "a.txt".into(),
                    is_dir: false,
                },
                ClipItem {
                    path: "C:\\local\\b.txt".into(),
                    name: "b.txt".into(),
                    is_dir: false,
                },
            ],
            "/remote/dst".into(),
        );
        let reqs: Vec<u64> = ws.inflight_put_meta.keys().copied().collect();
        assert_eq!(reqs.len(), 2, "两文件各发一 PutBegin");
        // 两者都探到撞名（policy=None）→ 都 park 进 conflict_queue（不互相覆盖）。
        for &req in &reqs {
            ws.put_ready(
                req,
                Some(PutConflict {
                    is_dir: false,
                    existing_len: 1,
                }),
                None,
            );
        }
        assert_eq!(ws.upload_conflict_count(), Some(2), "两冲突都在队列（H1：未覆盖）");
        // 跳过全部 → 两者都计跳过、队列清空、编排完成（不挂死）。
        ws.resolve_upload_conflict(crate::shell::OverwriteChoice::Skip);
        assert!(ws.upload.is_none(), "跳过全部后编排完成、未挂死");
        assert!(
            ws.take_notices()
                .iter()
                .any(|n| matches!(n, Notice::UploadDone { skipped: 2, .. })),
            "应弹 UploadDone(skipped=2)"
        );
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
    fn weights_approx_eq_容差与形状() {
        let a = (1u64, vec![0.5, 0.5], vec![vec![1.0]]);
        // 微抖动（< 1e-4）视为相等：免归一化浮点抖动刷帧。
        assert!(weights_approx_eq(&a, &(1, vec![0.50001, 0.49999], vec![vec![1.0]])));
        // 超容差不等。
        assert!(!weights_approx_eq(&a, &(1, vec![0.3, 0.7], vec![vec![1.0]])));
        // tab_id 不同 / 形状不同 → 不等。
        assert!(!weights_approx_eq(&a, &(2, vec![0.5, 0.5], vec![vec![1.0]])));
        assert!(!weights_approx_eq(&a, &(1, vec![1.0], vec![vec![1.0]])));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // 测试需直接挂 cmd_tx 捕获出站帧（无公开构造器）。
    fn sublayout_双向比例_回声免疫与本地变更检测() {
        // 核心正确性：应用对端比例后，本端「变更检测」不把它当本地改动回发（回声免疫）；
        // 仅本地真改才发 SubLayout。用 cmd_tx channel 捕获实际出站帧验证。
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ws = RemoteWs::default();
        ws.cmd_tx = Some(tx);
        起会话(&mut ws, Role::Controlled);
        let has_sublayout = |rx: &Receiver<RemoteC2S>| -> bool {
            let mut found = false;
            while let Ok(msg) = rx.try_recv() {
                if let RemoteC2S::Relay(v) = msg {
                    if matches!(RemoteFrame::from_value(&v), Ok(RemoteFrame::SubLayout { .. })) {
                        found = true;
                    }
                }
            }
            found
        };
        let _ = has_sublayout(&rx); // 清起会话期间杂帧。
        let rw = vec![0.3, 0.7];
        let cw = vec![vec![1.0], vec![1.0]];
        // 被控端收到控制端比例 → main 流程：取走 + 应用 + 记基线。
        relay(
            &mut ws,
            &RemoteFrame::SubLayout {
                tab_id: 9,
                row_weights: rw.clone(),
                col_weights: cw.clone(),
            },
        );
        let got = ws.take_sub_layout().expect("有待应用比例");
        assert_eq!(got.0, 9);
        ws.note_sub_layout_baseline(got.0, got.1, got.2);
        let _ = has_sublayout(&rx);
        // 回声免疫：本端当前比例 == 刚应用的对端比例 → 不回发。
        ws.send_sub_layout_if_changed(9, rw.clone(), cw.clone());
        assert!(!has_sublayout(&rx), "应用对端比例后同值不回发（回声免疫）");
        // 本地真改（被控端用户拖了分隔条）→ 比例变 → 回发一帧。
        ws.send_sub_layout_if_changed(9, vec![0.5, 0.5], cw);
        assert!(has_sublayout(&rx), "本地比例真变应发 SubLayout");
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
    fn 历史短应答清缺口在途() {
        // 字节夹紧：请求 [10,20) 在途，被控端只返 [10,13)（字节预算截断）。返回的 3 行入缓存销账，
        // 未返的 [13,20) 缺口须经 clear_inflight_gap 移出 inflight，否则永久卡住、回看空白。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        for a in 10..20 {
            ws.hist_inflight.insert(a); // 模拟请求 [10,20) 在途
        }
        relay(
            &mut ws,
            &RemoteFrame::HistoryRows {
                top: 10,
                base: 0,
                screen_top: 100,
                lines: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], // 仅返 [10,13)
            },
        );
        assert_eq!(ws.hist_cache.len(), 3, "返回的 3 行入缓存");
        assert!(
            ws.hist_inflight.is_empty(),
            "未返的缺口 [13,20) 应被清出 inflight 以便续请求（否则永久空白）"
        );
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
    #[allow(clippy::field_reassign_with_default)]
    fn part4_输入按焦点窗格路由转发() {
        // Phase 4：未订阅→send_input 不发不动回看态（杜绝旧 part4a 落到被控端激活会话缺陷）；订阅后
        // 按 (subscribed_tab, 焦点窗格 session_id) 发 InputWithId，且回看态转发即 snap 回跟随（看回显）。
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ws = RemoteWs::default();
        ws.cmd_tx = Some(tx);
        起会话(&mut ws, Role::Controller);
        let take_input = |rx: &Receiver<RemoteC2S>| -> Option<(TabId, SessionId, Vec<u8>)> {
            while let Ok(msg) = rx.try_recv() {
                if let RemoteC2S::Relay(v) = msg {
                    if let Ok(RemoteFrame::InputWithId {
                        tab_id,
                        session_id,
                        data,
                    }) = RemoteFrame::from_value(&v)
                    {
                        return Some((tab_id, session_id, data));
                    }
                }
            }
            None
        };
        // 未订阅：不发、回看态不变。
        relay(&mut ws, &RemoteFrame::HistoryBounds { base: 0, screen_top: 100 });
        ws.scroll_mirror(5);
        assert_eq!(ws.hist_top, Some(95), "已进回看态");
        ws.send_input(b"x");
        assert_eq!(ws.hist_top, Some(95), "未订阅：send_input no-op，回看态不变");
        assert!(take_input(&rx).is_none(), "未订阅不发输入");
        // 订阅单窗格会话：SubscriptionStarted 设 mirror_focus_sid（42）。
        ws.subscribe_tab(7);
        relay(
            &mut ws,
            &RemoteFrame::SubscriptionStarted {
                tab_id: 7,
                focused: 0,
                panes: vec![PaneSnapshot {
                    session_id: 42,
                    rows: 24,
                    cols: 80,
                    snapshot: vec![],
                    base: 0,
                    screen_top: 24,
                    custom_title: None,
                }],
                row_weights: vec![],
                col_weights: vec![],
                maximized: None,
            },
        );
        ws.scroll_mirror(3); // 再进回看态
        assert!(ws.hist_top.is_some(), "订阅后再进回看态");
        ws.send_input(b"ls\r");
        assert_eq!(ws.hist_top, None, "转发输入 snap 回跟随底部");
        assert_eq!(
            take_input(&rx),
            Some((7, 42, b"ls\r".to_vec())),
            "按 (tab=7, 焦点窗格=42) 路由发 InputWithId"
        );
    }

    /// 构造一个 part3d PaneSnapshot（测试用，内容空、给定 id 与边界）。
    fn pane_snap(session_id: SessionId, base: u64, screen_top: u64) -> PaneSnapshot {
        PaneSnapshot {
            session_id,
            rows: 24,
            cols: 80,
            snapshot: vec![],
            base,
            screen_top,
            custom_title: None,
        }
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn part4_多窗格焦点切换与输入路由() {
        // 多窗格订阅 → 默认焦点 = 被控端 focused 窗格；点击切焦点 → 输入路由到新焦点窗格、
        // 回看边界按新窗格快照复位（与被控端焦点解耦）。
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ws = RemoteWs::default();
        ws.cmd_tx = Some(tx);
        起会话(&mut ws, Role::Controller);
        let take_input = |rx: &Receiver<RemoteC2S>| -> Option<(TabId, SessionId, Vec<u8>)> {
            while let Ok(msg) = rx.try_recv() {
                if let RemoteC2S::Relay(v) = msg {
                    if let Ok(RemoteFrame::InputWithId {
                        tab_id,
                        session_id,
                        data,
                    }) = RemoteFrame::from_value(&v)
                    {
                        return Some((tab_id, session_id, data));
                    }
                }
            }
            None
        };
        ws.subscribe_tab(5);
        relay(
            &mut ws,
            &RemoteFrame::SubscriptionStarted {
                tab_id: 5,
                focused: 1, // 被控端焦点 = panes[1] = sid 20
                panes: vec![pane_snap(10, 0, 30), pane_snap(20, 5, 40)],
                row_weights: vec![0.5, 0.5],
                col_weights: vec![vec![1.0], vec![1.0]],
                maximized: None,
            },
        );
        // 默认焦点采纳被控端 focused（sid 20）；输入路由到它。
        assert_eq!(ws.mirror_active_pane, Some(20));
        ws.send_input(b"a");
        assert_eq!(take_input(&rx), Some((5, 20, b"a".to_vec())));
        // 切焦点到 sid 10：输入改路由到 10，回看边界复位为该窗格快照 (0, 30)。
        assert!(ws.set_mirror_active_pane(10));
        assert_eq!(ws.mirror_active_pane, Some(10));
        assert_eq!(ws.hist_bounds, Some((0, 30)));
        ws.send_input(b"b");
        assert_eq!(take_input(&rx), Some((5, 10, b"b".to_vec())));
        // 不存在的窗格 id 切焦点被忽略。
        assert!(!ws.set_mirror_active_pane(999));
        assert_eq!(ws.mirror_active_pane, Some(10));
    }

    #[test]
    fn part4_paneop_仅被控端入队() {
        use lumen_protocol::remote::PaneOpKind;
        // 被控端收 PaneOp → 入队（按 tab/session/op）；控制端角色不入队。
        let mut ctl = RemoteWs::default();
        起会话(&mut ctl, Role::Controller);
        relay(
            &mut ctl,
            &RemoteFrame::PaneOp {
                tab_id: 1,
                session_id: 2,
                op: PaneOpKind::Close,
            },
        );
        assert!(ctl.take_pane_ops().is_empty(), "控制端不入队 PaneOp");
        let mut sub = RemoteWs::default();
        起会话(&mut sub, Role::Controlled);
        relay(
            &mut sub,
            &RemoteFrame::PaneOp {
                tab_id: 1,
                session_id: 2,
                op: PaneOpKind::SwapWith { other: 9 },
            },
        );
        assert_eq!(
            sub.take_pane_ops(),
            vec![(1, 2, PaneOpKind::SwapWith { other: 9 })]
        );
        assert!(sub.take_pane_ops().is_empty(), "取走后清空");
    }

    #[test]
    fn part4_per_pane选区起选互斥() {
        // 在某窗格起选 → 仅该窗格有选区、进拖选态；切到另一窗格起选 → 旧窗格选区清（一时刻一个源）。
        let mut ws = RemoteWs::default();
        起会话(&mut ws, Role::Controller);
        ws.subscribe_tab(5);
        relay(
            &mut ws,
            &RemoteFrame::SubscriptionStarted {
                tab_id: 5,
                focused: 0,
                panes: vec![pane_snap(10, 0, 30), pane_snap(20, 0, 30)],
                row_weights: vec![0.5, 0.5],
                col_weights: vec![vec![1.0], vec![1.0]],
                maximized: None,
            },
        );
        ws.mirror_pane_sel_start(10, 0, 0);
        assert!(ws.mirror_pane_selecting());
        assert_eq!(ws.mirror_pane_selecting_sid(), Some(10));
        assert!(ws.mirror_panes.iter().find(|p| p.session_id == 10).unwrap().selection.is_some());
        // 切到窗格 20 起选 → 窗格 10 选区清。
        ws.mirror_pane_sel_start(20, 0, 0);
        assert!(ws.mirror_panes.iter().find(|p| p.session_id == 10).unwrap().selection.is_none());
        assert!(ws.mirror_panes.iter().find(|p| p.session_id == 20).unwrap().selection.is_some());
        assert_eq!(ws.mirror_pane_selecting_sid(), Some(20));
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
        // 单窗格 HistoryReq → 目标 None（焦点窗格）。
        assert_eq!(ws.take_history_reqs(), vec![(None, 5, 3)]);
        // 取走后清空。
        assert!(ws.take_history_reqs().is_empty());
        // 多窗格 HistoryReqForPane → 目标 Some(sid)。
        relay(
            &mut ws,
            &RemoteFrame::HistoryReqForPane {
                tab_id: 1,
                session_id: 9,
                top: 20,
                count: 4,
            },
        );
        assert_eq!(ws.take_history_reqs(), vec![(Some(9), 20, 4)]);
        // 控制端角色不应入队历史请求（HistoryReq* 仅被控端处理）。
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
