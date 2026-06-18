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
use std::path::PathBuf;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lumen_protocol::remote::{
    DenyReason, DirEntry, EndReason, FsErr, PairingFailReason, RemoteC2S, RemoteFrame, RemoteS2C,
    Role, FETCH_MAX_LEN, FETCH_WINDOW, FILE_CHUNK,
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
    /// 被控端：文件服务后台线程 → 主线程的回包发送端（[`Self::start`] 建，worker 克隆）。
    svc_tx: Option<Sender<SvcReply>>,
    /// 被控端：文件服务回包接收端（main 在 `pump_remote` 经 [`Self::drain_service`] 排空发回）。
    svc_rx: Option<Receiver<SvcReply>>,
    /// 控制端：在途 Fetch（接收方）：req_id → 落本地文件的任务（#5 打开 / #7 下载）。
    inflight_fetch: HashMap<u64, FetchJob>,
    /// 被控端：在途 Fetch 源（发送方）：req_id → 给 worker 发「可再发一块」许可的句柄
    /// （每收一个 [`RemoteFrame::FileChunkAck`] +1，滑动窗口背压）。
    inflight_fetch_src: HashMap<u64, FetchSrcJob>,
    /// 跨本地 / 远程的文件剪贴板（复制记引用、粘贴触发传输）。会话结束清 `Remote` 侧。
    file_clipboard: Option<FileClipboard>,
    /// 控制端：进行中的 #7 下载编排（远程 → 本地递归传输）；同一时刻一个。
    download: Option<DownloadWalk>,
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
}

/// 远程树一个真实节点（目录 / 文件；占位行不入表，渲染时合成）。
struct RemoteNode {
    /// 被控端不透明路径（往返键 + 显示）。
    path: String,
    /// 显示名（被控端 `display_name` 算好）。
    name: String,
    /// 是否目录。
    is_dir: bool,
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
        if let Some(root) = &self.root {
            let name = last_path_segment(root);
            self.nodes.push(RemoteNode {
                path: root.clone(),
                name,
                is_dir: true,
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
    fn node_path(&self, id: usize) -> Option<&str> {
        self.nodes.get(id).map(|n| n.path.as_str())
    }
    fn node_is_dir(&self, id: usize) -> bool {
        self.nodes.get(id).is_some_and(|n| n.is_dir)
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
}

/// 控制端在途 Fetch 的用途：决定落地位置与收完动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchKind {
    /// #5 双击打开：落临时文件，`FileEnd` 后用本地默认程序打开。
    Open,
    /// #7 下载：落到目标本地路径，`FileEnd` 后只关句柄（不打开），并推进下载编排。
    Download,
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
    /// 上次收到块的时刻（停滞超时清理；`FileBegin` 与每块刷新）。
    last_at: Instant,
}

/// 跨「本地 / 远程」两侧的文件剪贴板（复制只记引用、零传输；粘贴才触发字节流）。
pub struct FileClipboard {
    /// 复制来源侧。
    pub side: ClipSide,
    /// 复制的项（path 不透明、name 显示名、is_dir 决定递归）。
    pub items: Vec<ClipItem>,
}

/// 剪贴板来源 / 粘贴目标侧。
#[derive(Clone, Copy, PartialEq, Eq)]
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

/// 被控端在途 Fetch 源（发送方）：仅持给 worker 发许可的句柄，worker 经 `cmd_tx` 直发帧。
struct FetchSrcJob {
    /// 每收一个 [`RemoteFrame::FileChunkAck`] 发一个许可，worker 领许可才读发下一块（背压）。
    permit_tx: Sender<()>,
}

/// 取路径末段作显示名（`C:\Users\hf` → `hf`；盘符根 `C:\` 等无末段时返回整串）。
fn last_path_segment(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(path)
        .to_owned()
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
        self.svc_tx = Some(svc_tx);
        self.svc_rx = Some(svc_rx);
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
        // 先 clear（其内排空 svc_rx 丢弃在途回包）再断 svc 通道——否则 svc_rx 已 None
        // 时排空成死代码（与 end_session/Disconnected 路径行为不一致）。
        self.clear_remote_filetree();
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
        self.clear_remote_filetree();
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

    /// 被控端：后台线程读目录（绝不在主循环同步 IO——慢速网络盘会冻结整个应用），
    /// 结果经 `svc_tx` 回主线程，由 [`Self::drain_service`] 发回控制端。
    ///
    /// TODO(片3 文件服务统一)：当前每请求起一条 OS 线程、无并发上限（review MED-1）。正常
    /// 负载下控制端的 `has_listing`/`is_pending` 闸使每目录至多请求一次、量极小；但慢速网络
    /// 盘上大量展开会堆线程。片3 引入 Fetch/Put 大文件传输时，把读目录 / 读文件 / 写文件统
    /// 一收敛为「常驻 service 线程 + 有界任务队列 + inflight 上限」，与历史服务的同步有界精神
    /// 一致。威胁模型上控制端已是配对鉴权方（本可在被控端跑任意命令），不构成提权，故片2 不阻断。
    pub fn spawn_list_dir(&self, req_id: u64, path: String, show_hidden: bool) {
        let Some(tx) = self.svc_tx.clone() else {
            return;
        };
        thread::spawn(move || {
            let reply = match crate::shell::filetree::list_dir_entries(
                std::path::Path::new(&path),
                show_hidden,
            ) {
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
            };
            // UI 先退出时通道已关：发送失败静默忽略。
            let _ = tx.send(reply);
        });
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
                SvcReply::FetchSrcDone { req_id } => {
                    self.inflight_fetch_src.remove(&req_id);
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
                last_at: Instant::now(),
            },
        );
        self.notices.push(Notice::FetchStarted);
        self.send_frame(&RemoteFrame::FetchReq { req_id, path });
    }

    /// 控制端：收 `FileBegin` → 建落地文件（Open=临时目录 `{req_id}-名`；Download=目标路径，
    /// 先 `create_dir_all` 父目录）。
    fn fetch_begin(&mut self, req_id: u64) {
        let (kind, name, dest_opt) = {
            let Some(job) = self.inflight_fetch.get(&req_id) else {
                return;
            };
            (job.kind, job.name.clone(), job.dest.clone())
        };
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
        };
        match std::fs::File::create(&target) {
            Ok(f) => {
                if let Some(job) = self.inflight_fetch.get_mut(&req_id) {
                    job.file = Some(f);
                    job.dest = Some(target);
                    job.next_seq = 0; // 重置连续性计数（防异常重发 FileBegin 后误判乱序）。
                    job.written = 0;
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

    /// 控制端：收 `FileChunk` → 连续性校验 + 累计字节硬上限后顺序写临时文件，回
    /// `FileChunkAck`（背压）。`Some(err)` = 中止原因（乱序 / 写失败 = `Io`，超上限 = `TooLarge`）。
    fn fetch_chunk(&mut self, req_id: u64, seq: u32, data: &[u8]) {
        let abort: Option<FsErr> = {
            let Some(job) = self.inflight_fetch.get_mut(&req_id) else {
                return;
            };
            let Some(file) = job.file.as_mut() else {
                return; // FileBegin 失败/未到：忽略（清理已在别处）。
            };
            if seq != job.next_seq {
                log::warn!("Fetch 块乱序 req={req_id} seq={seq} 期望={}", job.next_seq);
                Some(FsErr::Io)
            } else if let Err(e) = file.write_all(data) {
                log::error!("写临时文件失败: {e}");
                Some(FsErr::Io)
            } else {
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
        };
        if let Some(err) = abort {
            self.fetch_abort(req_id, err);
        } else {
            self.send_frame(&RemoteFrame::FileChunkAck { req_id, seq });
        }
    }

    /// 控制端：收 `FileEnd` → flush + 关句柄。Open → 用系统默认程序打开（#5）；
    /// Download → 计完成 + 推进下载队列（#7）。
    fn fetch_end(&mut self, req_id: u64) {
        let Some(mut job) = self.inflight_fetch.remove(&req_id) else {
            return;
        };
        let kind = job.kind;
        let dest = job.dest.take();
        if let Some(mut file) = job.file.take() {
            let _ = file.flush();
            drop(file); // 关句柄后再交系统程序（Windows 打开前须释放写锁）。
            match kind {
                FetchKind::Open => {
                    if let Some(d) = dest {
                        crate::shell::filetree::open_with_default(&d);
                    }
                }
                FetchKind::Download => self.download_file_done(false),
            }
        } else {
            // FileBegin 未成功就 End（异常）：清理半成品 / 下载计错。
            if let Some(d) = dest {
                let _ = std::fs::remove_file(d);
            }
            if matches!(kind, FetchKind::Download) {
                self.download_file_done(true);
            }
        }
    }

    /// 控制端：中止一个在途 Fetch（乱序 / 写失败 / 超上限 / 对端 `FileErr` / 停滞超时 / 建文件
    /// 失败）：关句柄、删半成品文件，发 `FetchCancel` 让被控端即时回收源 worker。Open 弹失败
    /// 提示；Download 计错 + 推进队列（不逐文件弹 toast，结束汇总）。
    fn fetch_abort(&mut self, req_id: u64, err: FsErr) {
        let mut was_download = false;
        if let Some(mut job) = self.inflight_fetch.remove(&req_id) {
            was_download = matches!(job.kind, FetchKind::Download);
            job.file = None; // 关句柄（Windows 删前须释放）。
            if let Some(d) = job.dest.take() {
                let _ = std::fs::remove_file(&d);
            }
        }
        self.send_frame(&RemoteFrame::FetchCancel { req_id });
        if was_download {
            self.download_file_done(true);
        } else {
            self.notices.push(Notice::FetchFailed(err));
        }
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

    /// 控制端：开始把剪贴板里的远程项下载到本地 `dest_dir`（递归）。`overwrite`：撞名覆盖
    /// （否则跳过已存在），由粘贴时的覆盖弹窗一次性决定、套用整次递归。
    ///
    /// 守卫：非控制中（会话已结束）不起（防 H2 死会话复活下载态）；已有下载在途则忽略
    /// （防 M1 并发粘贴污染状态机——同一时刻仅一个下载）。
    pub fn start_download(&mut self, items: Vec<ClipItem>, dest_dir: String, overwrite: bool) {
        if items.is_empty() || !self.is_controlling() {
            return;
        }
        if self.download.is_some() {
            log::debug!("下载进行中，忽略新的粘贴下载请求");
            return;
        }
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
                    name: String::new(),
                    dest: Some(dest),
                    file: None,
                    next_seq: 0,
                    written: 0,
                    last_at: Instant::now(),
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

    /// 清空文件树同步态（会话起止 / 断线；**不**在终端 Resize 时清——resize 不动文件树）。
    fn clear_remote_filetree(&mut self) {
        self.remote_filetree = None;
        self.remote_root_sent = None;
        self.pending_listdir.clear();
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
            RemoteFrame::FileBegin { req_id, .. } => {
                // 仅控制端：建临时落地文件。total_len 仅供进度，finalize 以 FileEnd 为准。
                if matches!(self.session.as_ref().map(|s| s.role), Some(Role::Controller)) {
                    self.fetch_begin(req_id);
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
            // 片4/5：Put*/MkDir*。当前未实现，忽略。
            RemoteFrame::PutBegin { .. }
            | RemoteFrame::PutReady { .. }
            | RemoteFrame::PutChunk { .. }
            | RemoteFrame::PutChunkAck { .. }
            | RemoteFrame::PutEnd { .. }
            | RemoteFrame::PutResult { .. }
            | RemoteFrame::MkDir { .. }
            | RemoteFrame::MkDirResult { .. } => {}
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
                self.clear_remote_filetree();
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
                },
                DirEntry {
                    path: "/r/a.txt".into(),
                    name: "a.txt".into(),
                    is_dir: false,
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
        ws.fetch_begin(req);
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
        let mut ws = RemoteWs::default();
        // start_download 守卫 is_controlling：测试需置控制中会话。
        ws.session = Some(ActiveSession {
            peer_name: "peer".into(),
            role: Role::Controller,
        });
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
        ws.fetch_begin(req);
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
        let mut ws = RemoteWs::default();
        ws.session = Some(ActiveSession {
            peer_name: "peer".into(),
            role: Role::Controller,
        });
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
