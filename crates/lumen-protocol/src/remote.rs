//! M5.3 终端远程：WebSocket 长连接经 `lumen-server` 中继的「控制面」协议。
//!
//! 与 M5.1/M5.2 的 REST DTO 不同，本模块是**双向消息流**：客户端与服务端
//! 各自维护一条 WebSocket（路径 [`crate::routes::WS`]），消息以 JSON 文本帧
//! 收发。两类角色：
//!
//! - **控制端**（controller）：选中在线设备发起 [`RemoteC2S::RequestControl`]，
//!   输入被控端展示的 9 位配对码 [`RemoteC2S::SubmitPairing`]，配对成功后镜像
//!   并操控对端。
//! - **被控端**（controlled）：收到 [`RemoteS2C::ControlRequested`] 后醒目展示
//!   配对码，可 [`RemoteC2S::DeclineControl`] 拒绝；被控期间把状态增量经
//!   [`RemoteC2S::Relay`] 推给控制端，并执行控制端发来的操作指令。
//!
//! # 控制面 vs 数据面（可扩展性铁律）
//! 服务端**只解释控制面消息**（请求 / 配对 / 独占仲裁 / 会话收尾）；数据面
//! [`RemoteC2S::Relay`] / [`RemoteS2C::Relay`] 的载荷是**不透明 [`serde_json::Value`]**，
//! 服务端原样转发给会话对端、绝不反序列化其内部结构。客户端两端共享
//! [`RemoteFrame`] 类型做强类型收发（[`RemoteFrame::to_value`] /
//! [`RemoteFrame::from_value`]）。如此 M5.3 part2/3 给 [`RemoteFrame`] 增加
//! 状态增量（grid delta / block / 布局）与操作指令（Action）变体时，**服务端
//! 中继代码零改动**——这是分期推进不返工的关键。
//!
//! # 安全要点（part1 已落实，详见 `docs/M5远程控制设计.md` §6）
//! - WS 升级走 `Authorization: Bearer` 头鉴权（复用 REST 的 JWT），不走 query
//!   参数，避免反代日志泄漏 token。
//! - 配对码仅下发给**被控端**展示；控制端凭人工转述输入，服务端校验。
//! - [`RemoteC2S::SubmitPairing`] 由服务端校验「提交者 == 发起请求的 device_id」
//!   （取自连接的鉴权身份，非消息自报），杜绝抢答 / 重放 / 跨用户接管。

use serde::{Deserialize, Serialize};

/// 会话中本端扮演的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// 控制端（镜像并操控对端）。
    Controller,
    /// 被控端（被镜像、执行远程指令）。
    Controlled,
}

/// 控制请求被拒 / 取消的机器可读原因（客户端按此本地化提示，不靠字符串匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// 目标设备当前不在线（无活跃 WS 连接）。
    Offline,
    /// 目标设备已被其他控制端独占。
    AlreadyControlled,
    /// 控制端自己已在控制另一台设备（控制端同一时刻只控一台）。
    ControllerBusy,
    /// 目标设备正在与他人配对中（已有未决配对请求）。
    TargetPairing,
    /// 目标设备属于其它账户（禁止跨账户控制）。
    CrossUser,
    /// 不能控制自己。
    SelfControl,
    /// 被控端用户主动拒绝。
    RejectedByUser,
    /// 控制端在配对完成前断线 / 撤销。
    ControllerLeft,
    /// 配对超时（被控端未在有效期内完成）。
    Expired,
    /// 配对码连续输错次数超限。
    TooManyAttempts,
}

/// 配对码校验失败的机器可读原因（[`RemoteS2C::PairingResult`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingFailReason {
    /// 配对码错误（还有剩余尝试次数）。
    InvalidCode,
    /// 配对已超时失效。
    Expired,
    /// 无对应的未决配对（可能已被取消 / 完成 / 从未发起）。
    NoPending,
    /// 错误次数超限，配对作废。
    TooManyAttempts,
}

/// 会话结束的机器可读原因（[`RemoteS2C::SessionEnded`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndReason {
    /// 对端主动结束会话（[`RemoteC2S::EndSession`]）。
    PeerLeft,
    /// 对端断线。
    PeerDisconnected,
    /// 本设备在别处重新登录，当前连接被新连接取代。
    Replaced,
}

/// 会话(tab)唯一标识——镜像 `lumen-app` 侧 `session::TabId`（自增 `u64`、关闭后不复用）。
/// 协议 crate 不依赖 app，故在此独立别名；两端同为 `u64`，边界零转换。**part3d 起数据面按
/// `(TabId, SessionId)` 双 id 路由**（见 [`RemoteFrame::OutputWithId`]），不再用无 id 的
/// [`RemoteFrame::Output`]。
pub type TabId = u64;

/// 窗格(pane)唯一标识——镜像 app 侧 `session::SessionId`（每个分屏窗格一个稳定 id、关窗格后
/// `Vec<Session>` 下标会重排但 id 不变不复用）。**绝不用窗格下标(渲染序)做路由 / 缓存键**
/// （part3d D1：关窗格令下标重排，用下标会静默错位内容）；下标仅作渲染顺序。
pub type SessionId = u64;

/// 数据面帧（控制端↔被控端，经服务端**盲转**；服务端不解释内容）。
///
/// part1 仅含 [`RemoteFrame::Echo`] 占位；**part3a 终端镜像**采用「VT 字节流转发」
/// 方案：被控端把焦点窗格 PTY 输出（含初始整屏快照重放）转发给控制端，控制端喂入
/// 一个无 PTY 的镜像 `Terminal::advance` 复现整状态（颜色/光标/标题/cwd/命令块全在
/// 字节流的 SGR/OSC 里，自动还原）。故数据面**只传字节**，无需把结构化 Cell 上线，
/// `lumen-protocol` 保持零依赖。后续 part3c（多窗格/布局）再加变体，**服务端中继零改**。
///
/// **part3d 历史按需分页**：会话只传当前可见屏，被控端的 scrollback 历史不预传。
/// 控制端上滚回看时按视口窗口发 [`RemoteFrame::HistoryReq`]，被控端按绝对行号
/// （`Grid::line_by_abs`）序列化对应行回 [`RemoteFrame::HistoryRows`]——「滚到哪屏拉哪屏」，
/// 断线重连亦按需重拉，不再因镜像会话内 scrollback 丢失而看不到历史。
///
/// **part3c-2 Option B 目录树 + 双向文件传输**（替代 part3c-1 的 Option A 快照推送）：
/// 控制端按需浏览被控端文件系统（[`ListDir`](RemoteFrame::ListDir) / [`RootChanged`](RemoteFrame::RootChanged)），
/// 自持展开态（不同步）；文件读取 [`FetchReq`](RemoteFrame::FetchReq)（分块 + ACK 背压，打开 / 下载）；
/// 文件写入 [`PutBegin`](RemoteFrame::PutBegin)（两阶段 + 撞名决议 + 背压，上传）。被控端只服务无
/// 状态读/写原语，所有递归 / 复制粘贴 / 打开由控制端编排。路径全程不透明字符串往返、被控端解释。
///
/// part3d Phase 3 起 [`SubscriptionStarted`](RemoteFrame::SubscriptionStarted) 携多窗格布局权重
/// （`f32`），故本枚举**不再 `derive(Eq)`**（`f32` 无 `Eq`）；`PartialEq` 仍在（往返测试用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteFrame {
    /// 回环测试帧：原样转发给对端，用于 part1 验证中继通路连通。
    Echo(String),
    /// 被控端 → 控制端：焦点窗格 PTY 输出字节（会话起始的整屏快照重放 + 实时增量，
    /// 同一通道）。控制端逐帧 `mirror.advance(&bytes)`。
    Output(Vec<u8>),
    /// 被控端 → 控制端：镜像终端尺寸（行/列）。会话起始与被控端窗格 resize 时发；
    /// 控制端据此 `mirror.resize(rows, cols)`，**必须在该尺寸的 `Output` 之前到达**。
    Resize {
        /// 行数。
        rows: u16,
        /// 列数。
        cols: u16,
    },
    /// 控制端 → 被控端：用户输入的 VT 编码字节（按键 / 中断等）。被控端写入焦点
    /// 窗格 PTY（受「本地输入优先」仲裁：被控端本地用户刚输入过则丢弃，part4）。
    /// **part3d Phase 1–3 休眠**（无 id 落到被控端激活会话、违背只读）；Phase 4 起用带双 id 的
    /// [`Self::InputWithId`] 路由到订阅窗格，此变体保留收处理臂、由版本门挡住的 v1 对端休眠无害。
    Input(Vec<u8>),
    /// 控制端 → 被控端（part3d Phase 4）：把输入 VT 字节写到**指定**会话的**指定窗格**（控制端自选
    /// 焦点窗格、与被控端焦点解耦，需求 e）。被控端按 `(tab_id, session_id)` 查窗格 PTY 写入，受
    /// 「本地输入优先」**per-pane** 仲裁（仅当该窗格正被被控端本地用户输入时丢弃）。`data` 走 base64
    /// （见 [`b64`]，与 [`OutputWithId`](RemoteFrame::OutputWithId) 一致，免 JSON 数字数组膨胀）。
    InputWithId {
        /// 目标会话 id。
        tab_id: TabId,
        /// 目标窗格 id（路由键，**非下标**，D1）。
        session_id: SessionId,
        /// 用户输入的 VT 编码字节。
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// 控制端 → 被控端：请求被控端焦点窗格 resize 到此行列（SSH 式：远端跟随控制
    /// 端视图尺寸，被控端 shell/程序按此重排，控制端满屏渲染、零 letterbox）。被控
    /// 期间覆盖被控端自身窗口尺寸；断开后恢复（part3 视口协商）。
    ViewportResize {
        /// 行数。
        rows: u16,
        /// 列数。
        cols: u16,
    },
    /// 控制端 → 被控端（part3d 历史按需分页）：请求被控端焦点窗格历史行
    /// `[top, top + count)`（**绝对行号**，跨滚动稳定）。控制端镜像上滚回看时按当前
    /// 视口窗口缺哪段拉哪段——会话起始只发当前可见屏，历史不预传，断线重连亦按需重拉。
    HistoryReq {
        /// 起始绝对行号。
        top: u64,
        /// 请求行数。
        count: u16,
    },
    /// 控制端 → 被控端（part3d Phase 4 per-pane 回看）：请求**指定窗格**的历史行 `[top, top+count)`
    /// （绝对行号）。多窗格镜像上滚回看用——控制端一时刻只对**焦点窗格**回看（`mirror_focused_sid`）。
    /// 被控端按 `(tab_id, session_id)` 查窗格 grid 序列化对应行回 [`HistoryRowsForPane`](RemoteFrame::HistoryRowsForPane)。
    HistoryReqForPane {
        /// 目标会话 id。
        tab_id: TabId,
        /// 目标窗格 id（路由键，非下标）。
        session_id: SessionId,
        /// 起始绝对行号。
        top: u64,
        /// 请求行数。
        count: u16,
    },
    /// 被控端 → 控制端：应答 [`Self::HistoryReq`]。`lines[i]` 是绝对行 `top + i` 的
    /// VT 序列化（每行一段绝对 SGR 字节，空 `Vec` = 该行空白 / 越界）。同时回带当前
    /// 历史边界，使控制端夹紧回看范围并随实时输出推进。
    HistoryRows {
        /// 起始绝对行号（与请求对齐，`lines[i]` 对应 `top + i`）。
        top: u64,
        /// 被控端当前最旧保留行的绝对行号（更旧的已被 scrollback 淘汰）。
        base: u64,
        /// 被控端当前可视区首行（screen row 0）的绝对行号。
        screen_top: u64,
        /// 逐行 VT 字节。
        lines: Vec<Vec<u8>>,
    },
    /// 被控端 → 控制端（part3d Phase 4）：应答 [`HistoryReqForPane`](RemoteFrame::HistoryReqForPane)。
    /// 携 `session_id` 供控制端校验「是否仍是当前焦点窗格的回看」——切焦点窗格后到达的陈旧应答按
    /// `session_id != mirror_focused_sid` 丢弃，避免串台（绝对行号体系按**该窗格**独立 `base/screen_top`）。
    HistoryRowsForPane {
        /// 所属窗格 id（控制端校验键）。
        session_id: SessionId,
        /// 起始绝对行号（与请求对齐）。
        top: u64,
        /// 该窗格当前最旧保留行绝对行号。
        base: u64,
        /// 该窗格当前可视区首行绝对行号。
        screen_top: u64,
        /// 逐行 VT 字节。
        lines: Vec<Vec<u8>>,
    },
    /// 被控端 → 控制端：历史边界（最旧保留行 + 可视区首行的绝对行号）。会话起始
    /// 随整屏快照发一次，使控制端在首次回看前即知可滚范围。
    HistoryBounds {
        /// 最旧保留行绝对行号。
        base: u64,
        /// 可视区首行绝对行号。
        screen_top: u64,
    },
    // ── part3c-2 Option B 目录树：控制端按需浏览被控端文件系统 + 双向文件传输 ──────
    //    替代已删的 Option A `FileTreeSnapshot` / `FileTreeOp`：被控端只服务无状态读/写
    //    原语，所有展开态 / 递归 / 复制粘贴 / 打开由控制端编排，被控端保持简单、不持远程态。
    /// 被控端 → 控制端：焦点窗格 cwd 变化即推（会话起始也推一次）。控制端据此重置树根
    /// （清空缓存 + 代次 +1）。修 #4：cwd 变即换根，不等快照重发慢链。
    RootChanged {
        /// 被控端焦点窗格 cwd 的不透明展示字符串（控制端只展示 + 原样回传，不解析）。
        path: String,
    },
    /// 控制端 → 被控端：列目录。`req_id` 关联应答（多目录可并发在途）。
    ListDir {
        /// 控制端单调递增请求号（0 保留为哨兵无效）。
        req_id: u64,
        /// 被控端不透明目录路径（取自 [`RootChanged`](RemoteFrame::RootChanged) 或 [`DirEntry`]）。
        path: String,
        /// 是否包含隐藏项（远程「文件管理」语义：控制端可选显示 `.env` 等）。
        show_hidden: bool,
    },
    /// 被控端 → 控制端：[`ListDir`](RemoteFrame::ListDir) 应答（`err=Some` 时 `entries` 为空）。
    ListDirResult {
        /// 与请求对齐。
        req_id: u64,
        /// 原样回带请求目录路径：控制端按 path 走 `find_dir_by_path` + `pending` 双键校验。
        path: String,
        /// 被控端读出的子项（目录在前 + 不分大小写排序 + 单层封顶）。
        entries: Vec<DirEntry>,
        /// 单层超上限截断后未显示的项数（`>0` 时控制端画「溢出」占位）。
        overflow: u32,
        /// 读失败原因；`Some` 时 `entries` 空、`overflow=0`。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：递归列目录整棵子树（一次扫完，DFS 平铺）。用于「复制远程目录 → 资源管理器
    /// 粘贴」：控制端需先拿到整棵子树清单才能构造虚拟文件 descriptor。受最大深度 / 项数上限约束，
    /// 超限截断。`req_id` 关联应答（多目录可并发在途，同 [`ListDir`](RemoteFrame::ListDir)）。
    ListDirRecursive {
        /// 控制端单调递增请求号（0 保留为哨兵无效）。
        req_id: u64,
        /// 被控端不透明根目录路径（取自 [`RootChanged`](RemoteFrame::RootChanged) 或 [`DirEntry`]）。
        path: String,
        /// 是否包含隐藏项（与单层 [`ListDir`](RemoteFrame::ListDir) 取同一 `show_hidden`，保持一致）。
        show_hidden: bool,
        /// 最大递归深度（根目录为深度 1；`0` 视作无穷大）。建议传 [`LIST_DIR_RECURSIVE_MAX_DEPTH`]。
        max_depth: u32,
        /// 最多返回的总项数（目录项 + 文件项都计；`0` 视作无穷大）。建议传
        /// [`LIST_DIR_RECURSIVE_MAX_ENTRIES`]。
        max_entries: u32,
    },
    /// 被控端 → 控制端：[`ListDirRecursive`](RemoteFrame::ListDirRecursive) 应答（`err=Some` 时
    /// `entries` 为空、`truncated=false`）。
    ListDirRecursiveResult {
        /// 与请求对齐。
        req_id: u64,
        /// 原样回带请求根目录路径。
        path: String,
        /// 平铺子树。**DFS 遍历序，且此顺序即虚拟文件 descriptor 的 `fgd` 数组顺序、即资源管理器
        /// 请求 `FILECONTENTS` 的 `lindex` 索引**——全链路严禁任何一层重排，否则 lindex 错位、
        /// 粘贴出错位文件内容（数据静默损坏）。父目录项必在其所有子项之前（DFS 天然保证，空目录
        /// 也借此建出）。
        entries: Vec<RecursiveDirEntry>,
        /// 是否因超上限（深度 / 项数）而截断。
        truncated: bool,
        /// 读失败原因；`Some` 时 `entries` 空、`truncated=false`。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：请求读取整个文件字节（被控端按 [`FILE_CHUNK`] 分块、受 ACK 窗口节流）。
    FetchReq {
        /// 关联请求号。
        req_id: u64,
        /// 被控端不透明文件路径。
        path: String,
    },
    /// 控制端 → 被控端：中止一个在途 Fetch（接收方乱序 / 写失败 / 停滞超时 / 建临时文件失败）。
    /// 被控端据此移除源任务（drop 许可通道 → 源 worker 领许可失败自退、即时释放文件句柄 /
    /// 线程，无需等会话结束）。对端已自然结束（worker 已退）时为幂等无操作。
    FetchCancel {
        /// 关联请求号。
        req_id: u64,
    },
    /// 被控端 → 控制端：文件首帧（先于任何 [`FileChunk`](RemoteFrame::FileChunk)；`total_len`
    /// 仅供进度，finalize 以 [`FileEnd`](RemoteFrame::FileEnd) 为准，防读盘期文件被追加的 TOCTOU）。
    FileBegin {
        /// 关联请求号。
        req_id: u64,
        /// 文件总字节数（进度用）。
        total_len: u64,
    },
    /// 被控端 → 控制端：一块文件字节（`seq` 自 0 连续递增，控制端校验连续性）。
    FileChunk {
        /// 关联请求号。
        req_id: u64,
        /// 块序号（自 0 连续）。
        seq: u32,
        /// 原始字节（≤ [`FILE_CHUNK`]）。JSON 里走 base64（见 [`b64`]），免数字数组 ~4x 膨胀。
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// 控制端 → 被控端：确认已收 `seq`，被控端方可发 `seq + FETCH_WINDOW` 之后的块（滑动窗口背压）。
    FileChunkAck {
        /// 关联请求号。
        req_id: u64,
        /// 已确认收到的块序号。
        seq: u32,
    },
    /// 被控端 → 控制端：文件传完（控制端据此 finalize / 打开 / 写盘）。
    FileEnd {
        /// 关联请求号。
        req_id: u64,
    },
    /// 被控端 → 控制端：读文件任意阶段出错，终止该 `req_id`。
    FileErr {
        /// 关联请求号。
        req_id: u64,
        /// 错误原因。
        err: FsErr,
    },
    /// 控制端 → 被控端：开始写一个文件（上传）。`dir`+`name` 由被控端 `Path::join` 自拼
    /// （控制端不拼分隔符，杜绝跨平台分隔符歧义）。
    PutBegin {
        /// 关联请求号。
        req_id: u64,
        /// 目标目录（不透明，取自远程树节点 path）。
        dir: String,
        /// 文件名（控制端本地 basename；被控端 `validate_entry_name` 校验 + 子树断言）。
        name: String,
        /// 文件总字节数。
        total_len: u64,
        /// 撞名策略。
        overwrite: PutOverwrite,
    },
    /// 被控端 → 控制端：能否继续。`conflict=Some` → 控制端弹覆盖提示后重发
    /// `PutBegin(Force/Skip)`；`err=Some` → 致命错误（不可写 / 名字非法 / 路径穿越被拒）。
    PutReady {
        /// 关联请求号。
        req_id: u64,
        /// 撞名详情（`Probe` 命中已存在目标时）。
        conflict: Option<PutConflict>,
        /// 致命错误。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：一块上传字节（`seq` 自 0 连续）。被控端顺序写临时文件。
    PutChunk {
        /// 关联请求号。
        req_id: u64,
        /// 块序号（自 0 连续）。
        seq: u32,
        /// 原始字节（≤ [`FILE_CHUNK`]）。JSON 里走 base64（见 [`b64`]），免数字数组 ~4x 膨胀。
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// 被控端 → 控制端：确认已落盘 `seq`，控制端方可发后续窗口块
    /// （背压，对称于 [`FileChunkAck`](RemoteFrame::FileChunkAck)）。
    PutChunkAck {
        /// 关联请求号。
        req_id: u64,
        /// 已落盘的块序号。
        seq: u32,
    },
    /// 控制端 → 被控端：上传结束 → 被控端 flush + 原子 rename 临时文件到目标。
    PutEnd {
        /// 关联请求号。
        req_id: u64,
    },
    /// 被控端 → 控制端：写入最终结果。
    ///
    /// **`err` 优先于 `status`**：`err=Some` 时该次写入失败，`status` 无意义（实现可能填任意值，
    /// 如中止路径回 `Written`）——消费方必须先判 `err`，再看 `status`。
    PutResult {
        /// 关联请求号。
        req_id: u64,
        /// 写入 / 跳过（仅 `err=None` 时有意义）。
        status: PutStatus,
        /// 失败原因（失败时 `Some`；非 `None` 时盖过 `status`）。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：在 `dir` 下创建子目录 `name`（递归上传建目录结构；幂等）。
    MkDir {
        /// 关联请求号。
        req_id: u64,
        /// 父目录（不透明）。
        dir: String,
        /// 子目录名（被控端校验 + 子树断言）。
        name: String,
    },
    /// 被控端 → 控制端：[`MkDir`](RemoteFrame::MkDir) 结果（已存在视为成功，`err=None`）。
    MkDirResult {
        /// 关联请求号。
        req_id: u64,
        /// 创建成功的子目录被控端完整路径（控制端递归上传时作为其子项的 `dir`；`err=Some` 时为空串）。
        path: String,
        /// 失败原因。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：在 `dir` 下新建**空文件** `name`（远程菜单「新建文件」；被控端校验名 + 子树
    /// 断言，`create_new` 已存在则失败、不覆盖）。
    MkFile {
        /// 关联请求号。
        req_id: u64,
        /// 父目录（不透明）。
        dir: String,
        /// 新文件名（被控端校验）。
        name: String,
    },
    /// 被控端 → 控制端：[`MkFile`](RemoteFrame::MkFile) 结果。
    MkFileResult {
        /// 关联请求号。
        req_id: u64,
        /// 新建文件被控端完整路径（`err=Some` 时空串）。
        path: String,
        /// 失败原因。
        err: Option<FsErr>,
    },
    /// 控制端 → 被控端：删除 `path`（远程菜单「删除」）。`is_dir` 决定递归删目录 / 删单文件。
    Delete {
        /// 关联请求号。
        req_id: u64,
        /// 被删项被控端完整路径（不透明，先前 ListDir 枚举所得）。
        path: String,
        /// 是否目录（true 递归删，false 删文件）。
        is_dir: bool,
    },
    /// 被控端 → 控制端：[`Delete`](RemoteFrame::Delete) 结果（成功 `err=None`）。
    DeleteResult {
        /// 关联请求号。
        req_id: u64,
        /// 失败原因。
        err: Option<FsErr>,
    },
    // ── part3d 多会话 × 多窗格镜像 ───────────────────────────────────────────────
    //    控制端镜像被控端「所有会话(tab) + 每会话多窗格(pane)」：会话列表/状态帧（被控端推、
    //    随增删实时更新）+ 订阅切换（控制端选看某会话，被控端焦点不动）+ `(TabId,SessionId)`
    //    双 id 路由的内容帧 + 远程增删会话。**`pane_index` 仅渲染序、绝不做路由/缓存键**（D1）。
    //    旧无 id 的 [`Output`](RemoteFrame::Output) / [`Resize`](RemoteFrame::Resize) 于 Phase 1
    //    一次性切到带 id 帧（K2：`Welcome.min_supported_version` 版本门，不双发灰度）。
    /// 被控端 → 控制端：**全部**会话(tab)列表 + 概览状态（名/路径/忙/未读/窗格数）。控制端进入
    /// 远程视图 / (重)连后由被控端推一次做基线，之后增量走 [`TabCreated`](RemoteFrame::TabCreated)
    /// / [`TabClosed`](RemoteFrame::TabClosed) / [`TabUpdated`](RemoteFrame::TabUpdated)。`tabs`
    /// 顺序即被控端侧栏顺序（控制端按此渲染、不另排序；D6 重连按排序而非 HashMap 迭代序）。
    TabListSnapshot {
        /// 会话列表（被控端侧栏顺序）。
        tabs: Vec<TabState>,
    },
    /// 被控端 → 控制端：新建了一个会话(tab)（本地新建 / 远程 [`NewTab`](RemoteFrame::NewTab) 均推）。
    /// 控制端按 `tab.id` 插入列表（已存在则等价一次 [`TabUpdated`](RemoteFrame::TabUpdated)）。
    TabCreated {
        /// 新会话概览。
        tab: TabState,
    },
    /// 被控端 → 控制端：一个会话(tab)已关闭。控制端从列表移除；若正订阅它则按排序回退到邻近
    /// 会话（part3d Phase 2）。
    TabClosed {
        /// 被关会话 id。
        tab_id: TabId,
    },
    /// 被控端 → 控制端：一个会话(tab)概览状态变化（重命名 / cwd 变 / 忙闲翻转 / 未读 / 窗格增删）。
    /// 携全量 [`TabState`]；被控端按 K6 去重后才发——去重键**排除布局浮点、归一化 spinner 标题**，
    /// 高频抖动字段不刷链路。
    TabUpdated {
        /// 变化后的会话概览。
        tab: TabState,
    },
    /// 控制端 → 被控端：订阅某会话(tab)的内容（点击列表项切换查看）。被控端据此把该会话各窗格
    /// 输出经 [`OutputWithId`](RemoteFrame::OutputWithId) 推来，**被控端自身焦点不动**（需求 c/e）。
    /// K5：同一时刻只订阅 1 个——新订阅隐式取代旧的，被控端停推旧会话。被控端先回
    /// [`SubscriptionStarted`](RemoteFrame::SubscriptionStarted) 带初始整屏快照，再发增量（D3 保序）。
    SubscribeSession {
        /// 目标会话 id。
        tab_id: TabId,
    },
    /// 被控端 → 控制端：[`SubscribeSession`](RemoteFrame::SubscribeSession) 应答 + 订阅会话各窗格的
    /// **初始整屏快照**。`panes[i]` 渲染顺序即下标 `i`（复刻被控端布局：先上排自左向右、再下排），
    /// 路由键是 [`PaneSnapshot::session_id`]、**非下标**（D1）；`focused` 为焦点窗格在 `panes` 的下标。
    /// **此帧必先于该会话任何 [`OutputWithId`](RemoteFrame::OutputWithId) 到达**（D3：快照保序，否则
    /// 增量喂到空镜像错乱）。Phase 1 只回焦点窗格 1 个；**Phase 3 起回全部窗格 + 布局**，控制端据
    /// `row_weights`/`col_weights`/`maximized` 复刻被控端的 `pane_rects` 几何（网格结构由窗格数推导，
    /// 两端同一套规则：1=满屏、2=左右、3=左中右、4=上2下2、5=上3下2、6=上3下3）。
    SubscriptionStarted {
        /// 被订阅会话 id。
        tab_id: TabId,
        /// 焦点窗格在 `panes` 中的下标（渲染序；控制端高亮 / 单窗格镜像取此窗格）。
        focused: u32,
        /// 各窗格初始快照（渲染顺序 = 下标：先上排自左向右、再下排）。
        panes: Vec<PaneSnapshot>,
        /// 每排高度权重（长度 = 排数；归一化和为 1）。复刻被控端 `PaneLayout::row_weights`。
        /// `#[serde(default)]`：兼容 Phase 1/2 旧端（无此字段）发来的帧——缺省空，控制端按
        /// 单窗格降级（取 `panes[focused]`），免「两端必须同一构建」的脆弱性。
        #[serde(default)]
        row_weights: Vec<f32>,
        /// 每排内各列宽度权重（外层 = 排数，内层 = 该排列数）。复刻 `PaneLayout::col_weights`。
        #[serde(default)]
        col_weights: Vec<Vec<f32>>,
        /// 最大化窗格在 `panes` 中的下标（`Some` 时该窗格独占、其余隐藏；复刻被控端 `Tab::maximized`）。
        #[serde(default)]
        maximized: Option<u32>,
    },
    /// 被控端 → 控制端：带 `(tab_id, session_id)` 双 id 的窗格 PTY 输出（替代无 id 的
    /// [`Output`](RemoteFrame::Output)）。控制端按 `session_id` 路由到对应镜像 `Terminal::advance`
    /// （**绝不用窗格下标**，D1）。`data` 走 base64（见 [`b64`]）省 ~3x JSON 膨胀。仅订阅会话的窗格
    /// 会推（被控端双路 tee：本地焦点 + 订阅会话各窗格 + 输出节流，Phase 3 / D7）。
    OutputWithId {
        /// 所属会话 id。
        tab_id: TabId,
        /// 所属窗格 id（路由键）。
        session_id: SessionId,
        /// 窗格 PTY 输出字节。
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// 被控端 → 控制端：带 `(tab_id, session_id)` 双 id 的窗格尺寸（替代无 id 的
    /// [`Resize`](RemoteFrame::Resize)）。**必先于该窗格对应尺寸的 [`OutputWithId`] 到达**，
    /// 控制端据此 `mirror.resize(rows, cols)`。
    ResizeWithId {
        /// 所属会话 id。
        tab_id: TabId,
        /// 所属窗格 id（路由键）。
        session_id: SessionId,
        /// 行数。
        rows: u16,
        /// 列数。
        cols: u16,
    },
    /// 控制端 → 被控端：远程新建一个会话(tab)（需求 d）。被控端新建后回
    /// [`NewTabResult`](RemoteFrame::NewTabResult)，并向控制端推 [`TabCreated`](RemoteFrame::TabCreated)；
    /// 会话数已达 [`REMOTE_MAX_SESSIONS`] 时拒绝（`err=Some(LimitReached)`）。
    NewTab {
        /// 控制端单调递增请求号（关联应答；0 保留为哨兵无效）。
        req_id: u64,
    },
    /// 被控端 → 控制端：[`NewTab`](RemoteFrame::NewTab) 结果。**`err` 优先于 `tab_id`**：`err=Some`
    /// 时新建失败、`tab_id` 无意义（应填 `None`）；成功时 `tab_id=Some(新 id)`、`err=None`（控制端
    /// 据此可自动订阅新会话）。
    NewTabResult {
        /// 与请求对齐。
        req_id: u64,
        /// 新建成功的会话 id（失败为 `None`）。
        tab_id: Option<TabId>,
        /// 失败原因（成功为 `None`；非 `None` 时盖过 `tab_id`）。
        err: Option<RemoteOpErr>,
    },
    /// 控制端 → 被控端：远程关闭指定会话(tab)（需求 d）。被控端关闭后回
    /// [`CloseTabResult`](RemoteFrame::CloseTabResult) 并推 [`TabClosed`](RemoteFrame::TabClosed)。
    CloseTab {
        /// 控制端单调递增请求号。
        req_id: u64,
        /// 目标会话 id。
        tab_id: TabId,
    },
    /// 被控端 → 控制端：[`CloseTab`](RemoteFrame::CloseTab) 结果（`err=None` 即已关）。
    CloseTabResult {
        /// 与请求对齐。
        req_id: u64,
        /// 目标会话 id（原样回带）。
        tab_id: TabId,
        /// 失败原因（已关成功为 `None`；如 `tab_id` 不存在回 [`RemoteOpErr::NotFound`]）。
        err: Option<RemoteOpErr>,
    },
    /// 控制端 → 被控端：订阅会话各窗格的**目标网格尺寸**（控制端按自己均分布局 + 字号算出）。
    /// 被控端据此 resize 该会话的窗格，使镜像在控制端 **1:1 无裁切**地忠实显示（part3d Phase 3
    /// 尺寸同步）。**所有权规则**：仅当该会话在被控端为**后台 tab** 时生效（控制端定尺寸）；被控端
    /// 把它切到**前台**时由被控端窗口尺寸接管（控制端那期间回到裁剪/留白的忠实显示），避免两端抢
    /// resize。控制端尺寸变化（窗口 resize / 切订阅）才发，去重。
    SubViewport {
        /// 目标会话 id。
        tab_id: TabId,
        /// 各窗格目标尺寸（按 `session_id` 路由到被控端对应窗格）。
        panes: Vec<PaneViewport>,
    },
    /// **双向**（控制端↔被控端）：同步订阅会话窗格布局的**相对比例**（行/列权重）。任一端拖分隔条
    /// 改比例即发；对端把权重应用到该会话的 `PaneLayout`（控制端→镜像布局；被控端→其 tab 布局，
    /// **前台后台均应用**——proportions 不抢绝对网格，故对被控端前台无侵扰，各端按自己窗口×权重出格）。
    ///
    /// 与 [`SubViewport`](RemoteFrame::SubViewport) **互补且正交**：`SubViewport` 同步**绝对网格**
    /// （仅后台 tab，为控制端 1:1 无裁切）；`SubLayout` 同步**相对比例**（双向、不分前后台，解决「控制端
    /// 拖了前台 tab 不生效」与「两端比例不一致」）。前者改 grid、后者改 weights，二者不冲突。
    ///
    /// **回声免疫**：收发两端各自维护「已发/已应用基线」（`RemoteWs::sub_layout_baseline`），收到即把
    /// 基线更新为该权重——故「应用对端的比例」不会再被本端变更检测当成本地改动回发，连续拖动无回声打架。
    SubLayout {
        /// 目标会话 id。
        tab_id: TabId,
        /// 每排高度权重（归一化；长度 = 排数）。复刻 `PaneLayout::row_weights`。
        row_weights: Vec<f32>,
        /// 每排内各列宽度权重（外层 = 排数，内层 = 该排列数）。复刻 `PaneLayout::col_weights`。
        col_weights: Vec<Vec<f32>>,
    },
    /// 控制端 → 被控端（part3d Phase 4 需求②）：对订阅会话的某窗格做**远程操作**（关闭 / 最大化切换 /
    /// 与另一窗格换位）。被控端在该 tab 上执行后，布局/窗格集变化经 [`SubscriptionStarted`](RemoteFrame::SubscriptionStarted)
    /// 重发同步回控制端（两端一致）。**fire-and-forget**：无独立结果帧，以后续 `SubscriptionStarted`/
    /// `TabClosed` 为确认。按 `(tab_id, session_id)` 路由（非下标，D1）。
    PaneOp {
        /// 目标会话 id。
        tab_id: TabId,
        /// 目标窗格 id（路由键）。
        session_id: SessionId,
        /// 操作。
        op: PaneOpKind,
    },
    /// **双向**（控制端↔被控端，M6 P2P 直连）：P2P 打洞信令——交换公网端点候选 / 自签证书指纹，协商
    /// 建立 QUIC 直连或宣告回退中继。走现有 [`RemoteC2S::Relay`] 通路盲转，**服务器零改动**（前向兼容
    /// 铁律）。信令通道本身已过 JWT + 配对鉴权，可作直连证书指纹的信任锚（防 MITM）。
    ///
    /// `payload` 为该阶段数据的 **JSON 字符串**（候选端点 / SPKI 指纹 / nonce 等）。**内层结构 Phase 2
    /// 定**——Phase 0 只立信令信封，避免过早冻结候选/指纹格式（见 `docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md`）。
    P2pSignal {
        /// 信令阶段。
        kind: P2pSignalKind,
        /// 阶段数据（JSON 字符串；内层结构 Phase 2 定）。
        payload: String,
    },
    /// **P2P 直连数据面首帧**（M6 Phase 3）：QUIC 双向流建立后由发起方（Controller）`open_bi` 后
    /// **立即写出**的第一帧——quinn 的 `open_bi` 是惰性的，发起方不写首字节，对端 `accept_bi` 会永久
    /// 阻塞、流无法建立。本帧仅用于解除对端 `accept_bi` 阻塞 + 标记数据面流就绪，**收端 no-op**。
    /// 走 QUIC 直连流（非中继），与其它数据面帧同 length-prefix 分帧。
    P2pStreamHello,
}

/// part3d Phase 4 [`RemoteFrame::PaneOp`] 的操作种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneOpKind {
    /// 在该会话**新建一个窗格**（远程 split；`session_id` 字段被忽略——目标是 `tab_id` 整个会话）。
    /// 被控端在该 tab 加一格（不抢被控端自身焦点，需求 c/e），满窗格数上限则忽略（fire-and-forget）。
    New,
    /// 关闭该窗格（被控端关其 shell/PTY；若是该 tab 最后一格则关整个 tab）。
    Close,
    /// 切换该窗格最大化 / 还原（被控端 `toggle_maximize`）。
    ToggleMaximize,
    /// 与窗格 `other` 换位（被控端交换两窗格在 `panes` 中的下标 = 渲染序）。
    SwapWith {
        /// 换位目标窗格 id。
        other: SessionId,
    },
}

/// M6 [`RemoteFrame::P2pSignal`] 的打洞信令阶段（QUIC 打洞 + 中继回退握手机）。
///
/// 流程：`Offer`(发起方公布候选+指纹+nonce) → `Answer`(应答方公布候选+指纹) → 双方同时打洞 + QUIC
/// 握手 → `Ready`(某候选握手成功，确认直连) | `Fallback`(超时/失败，宣告继续全中继)。中继 WS 全程
/// 在线，P2P 仅作叠加加速层（见 `docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md` §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum P2pSignalKind {
    /// 发起方 → 应答方：公布本端候选端点（LAN + STUN 反射的公网映射）+ 自签证书 SPKI 指纹 + nonce。
    Offer,
    /// 应答方 → 发起方：回带本端候选端点 + 自签证书 SPKI 指纹。
    Answer,
    /// 任一端：某条候选 QUIC(TLS1.3) 握手成功且指纹校验通过，确认选定该直连。
    Ready,
    /// 任一端：打洞超时 / 全部候选失败（对称 NAT 等），宣告继续全中继（对端停止打洞尝试）。
    Fallback,
}

/// part3c-2 Option B 目录条目（被控端 `read_dir_worker` 产物，控制端只展示 + 原样回传）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// 被控端不透明完整路径（往返键：控制端不解析、不 basename、不 join）。
    pub path: String,
    /// 显示名（被控端 `display_name` 算好，控制端直接画）。
    pub name: String,
    /// 是否目录（控制端据此画三角 / 决定可下钻；不让控制端自己 stat）。
    pub is_dir: bool,
    /// 文件字节数（目录为 0）。片6 虚拟文件剪贴板的 descriptor 据此填 `FD_FILESIZE`，让资源管理器
    /// 知道文件大小（否则当 0 字节空文件、不取内容）。
    pub size: u64,
}

/// part3c-2 片8 递归目录树平铺项（[`ListDirRecursiveResult`](RemoteFrame::ListDirRecursiveResult)
/// 的元素）。被控端 DFS 遍历产出，控制端据此构造虚拟文件 descriptor。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDirEntry {
    /// 相对请求 `root_path` 的相对路径。**跨平台约定：一律用正斜杠 `/` 分隔**（被控端无论
    /// Linux / Windows 都规范化为 `/`），控制端构造 Windows descriptor 的 `cFileName` 时再转 `\`。
    /// 例：`sub/deep/file.txt`。根目录本身不出现（只列其内容）。
    pub rel_path: String,
    /// 被控端不透明完整绝对路径（后续 [`FetchReq`](RemoteFrame::FetchReq) 的 key；任何一端都不拆、
    /// 不 join、不 basename，同 [`DirEntry::path`]）。
    pub path: String,
    /// 是否目录（目录项在 descriptor 里打 `FILE_ATTRIBUTE_DIRECTORY`、不被请求 `FILECONTENTS`）。
    pub is_dir: bool,
    /// 文件字节数（目录为 0）。填 descriptor 的 `FD_FILESIZE`（缺则 0KB bug）。
    pub size: u64,
}

/// part3d 会话(tab)概览状态（[`TabListSnapshot`](RemoteFrame::TabListSnapshot) /
/// [`TabCreated`](RemoteFrame::TabCreated) / [`TabUpdated`](RemoteFrame::TabUpdated) 元素）。
/// 镜像 app 侧 `shell::TabItem` 的**可上线字段**（egui 图标纹理无法上线、不含）。控制端只展示，
/// 路由 / 订阅用 [`Self::id`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabState {
    /// 会话 id（路由 / 订阅键）。
    pub id: TabId,
    /// 名称行（自定义名 > 焦点窗格 cwd 尾目录名 > OSC 标题 > 「会话 N」，恒非空）。
    pub name: String,
    /// 路径行（焦点窗格 cwd 完整路径，OSC 9;9 上报）；cwd 未知（首个提示符前）为 `None`。
    pub path: Option<String>,
    /// 是否忙（焦点窗格 OSC 标题含 Braille spinner，claude 等 TUI 工作中）。被控端去重时
    /// 归一化此布尔判定，不让每帧抖动的 spinner 字形刷链路（K6）。
    pub busy: bool,
    /// 后台期间任一窗格有未读输出（控制端列表项小圆点）。
    pub unseen: bool,
    /// tab 内窗格数（>1 时控制端标「N 格」）。
    pub pane_count: u32,
    /// 焦点窗格前台程序 exe 图标（top-down RGBA8 位图）。被控端抽取上线、控制端
    /// 贴图；`None` = 无图标 / 抽取失败（控制端回退自绘终端字形）。旧端不带此字段
    /// 按 `None` 解析（`#[serde(default)]` 后向兼容；服务端盲转 JSON、不受影响）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconBitmap>,
}

/// part3d 会话图标位图（前台程序 exe 图标）：被控端 `proc_icon` 抽取的平台无关
/// top-down RGBA8（`rgba.len() == w*h*4`），控制端解码贴成 egui 纹理。`TabState::icon`
/// 的载体；`rgba` 走 base64 上线（同 [`PaneSnapshot::snapshot`]，免 JSON 数字数组 4x 膨胀）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IconBitmap {
    /// 宽（像素）。
    pub w: u16,
    /// 高（像素）。
    pub h: u16,
    /// top-down RGBA8 像素（base64，见 [`b64`]）。
    #[serde(with = "b64")]
    pub rgba: Vec<u8>,
}

/// part3d 窗格(pane)初始快照（[`SubscriptionStarted`](RemoteFrame::SubscriptionStarted) 元素）。
/// **无 `pane_index` 字段**——渲染顺序即所在 `panes` 数组下标（D1：下标仅渲染序、绝不做路由/
/// 缓存键）；路由键是 [`Self::session_id`]。`snapshot` 是整屏等效 VT 字节（控制端喂全新镜像
/// `Terminal::advance` 即复现该屏），随后续 [`OutputWithId`](RemoteFrame::OutputWithId) 增量推进。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    /// 窗格 id（路由键，跨增删稳定）。
    pub session_id: SessionId,
    /// 行数。
    pub rows: u16,
    /// 列数。
    pub cols: u16,
    /// 整屏等效 VT 字节（`remote_mirror::screen_snapshot_vt` 产物；base64，见 [`b64`]）。
    #[serde(with = "b64")]
    pub snapshot: Vec<u8>,
    /// 最旧保留行绝对行号（历史回看下界；对齐 [`HistoryBounds`](RemoteFrame::HistoryBounds)）。
    pub base: u64,
    /// 可视区首行绝对行号（历史回看锚点）。
    pub screen_top: u64,
    /// 窗格自定义名（用户重命名；app 级、不在 VT 流里，故显式带；`None` 走默认标题）。
    pub custom_title: Option<String>,
}

/// part3d Phase 3 尺寸同步：[`SubViewport`](RemoteFrame::SubViewport) 的单窗格目标尺寸。
/// 控制端按自己均分布局 + 字号算出每格能容纳的行列，被控端按 `session_id` 路由 resize 对应窗格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneViewport {
    /// 目标窗格 id（被控端路由键）。
    pub session_id: SessionId,
    /// 目标行数。
    pub rows: u16,
    /// 目标列数。
    pub cols: u16,
}

/// part3d 远程会话增删操作（[`NewTab`](RemoteFrame::NewTab) / [`CloseTab`](RemoteFrame::CloseTab)）
/// 的失败原因（机器可读，控制端本地化提示；风格同 [`FsErr`] / [`DenyReason`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteOpErr {
    /// 会话数已达 [`REMOTE_MAX_SESSIONS`]，拒绝新建（[`NewTab`](RemoteFrame::NewTab)）。
    LimitReached,
    /// 目标会话 id 不存在（[`CloseTab`](RemoteFrame::CloseTab) 目标已关 / 从未存在）。
    NotFound,
    /// 其它失败（spawn shell 失败等，粗粒度兜底）。
    Io,
}

/// 文件系统操作错误（机器可读，控制端本地化提示；风格同 [`DenyReason`] / [`EndReason`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsErr {
    /// 路径不存在。
    NotFound,
    /// 权限不足。
    PermissionDenied,
    /// 期望目录但实为文件。
    NotADirectory,
    /// 期望文件但实为目录。
    IsADirectory,
    /// 文件超 [`FETCH_MAX_LEN`]（Fetch）/ 名字非法 / 路径穿越被拒（Put）等拒绝类错误。
    TooLarge,
    /// 其它 IO 错误（粗粒度兜底）。
    Io,
}

/// Put 撞名策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PutOverwrite {
    /// 首次探测：被控端只回 [`PutReady`](RemoteFrame::PutReady)`{conflict}`，不写。
    Probe,
    /// 用户确认覆盖：被控端写并原子替换。
    Force,
    /// 用户选不覆盖：被控端直接回 [`PutResult`](RemoteFrame::PutResult)`{Skipped}`。
    Skip,
}

/// Put 撞名详情（`Probe` 命中已存在目标时回带）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutConflict {
    /// 已存在目标是否为目录（目录 vs 文件覆盖语义不同，控制端据此措辞）。
    pub is_dir: bool,
    /// 已存在文件字节数（控制端提示「覆盖 N 字节」）。
    pub existing_len: u64,
}

/// Put 最终写入结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PutStatus {
    /// 已写入（覆盖或新建）。
    Written,
    /// 按用户选择跳过。
    Skipped,
}

/// 文件传输单块原始字节上限。`data` 在 JSON 里走 base64（膨胀 ~1.33x：256 KiB → ~341 KiB
/// << 4 MiB 帧上限）。块设为 256 KiB（旧 64 KiB 的 4 倍）：等量数据帧数 / ACK 往返 / JSON
/// 序列化次数降到 1/4。
pub const FILE_CHUNK: usize = 256 * 1024;
/// 在途未 ACK 块窗口（背压：发送端最多领先对端 `FETCH_WINDOW` 块）。8 块 = 2 MiB 在途，
/// 填满局域网 / 中等延迟链路的带宽时延积。
pub const FETCH_WINDOW: u32 = 8;
/// 单文件 Fetch 字节上限（防控制端临时盘塞满；超出回 [`FsErr::TooLarge`]）。
pub const FETCH_MAX_LEN: u64 = 2 * 1024 * 1024 * 1024;

/// 递归列目录（[`RemoteFrame::ListDirRecursive`]）最大总项数（目录项 + 文件项），超限截断。
/// 与 4 MiB 单帧上限自洽：10000 项 × ~140 B JSON ≈ 2 MB << 4 MiB。也封顶虚拟文件 descriptor 的
/// `fgd` 数组规模，防资源管理器粘贴时内存 / 并发流爆炸。
pub const LIST_DIR_RECURSIVE_MAX_ENTRIES: u32 = 10_000;
/// 递归列目录最大深度（根目录深度 1；`0` 视作无穷大）。防符号链接环 / 病态深树；常见「复制项目树」
/// 远不及此。深层相对路径还会撞 Windows `MAX_PATH=260`，descriptor 端对超 259 的项另行剔除。
pub const LIST_DIR_RECURSIVE_MAX_DEPTH: u32 = 20;

/// part3d 远程可创建会话(tab)总数上限（[`RemoteFrame::NewTab`] 超限回
/// [`RemoteOpErr::LimitReached`]）：防控制端反复 [`NewTab`](RemoteFrame::NewTab) 在被控端 fork
/// 出无界 shell（资源耗尽）。32 对真实多会话使用绰绰有余；满载 [`TabListSnapshot`]
/// （32 × ~600 B）≈ 19 KB << 4 MiB 单帧上限。
pub const REMOTE_MAX_SESSIONS: u32 = 32;

/// 文件块字节在 JSON 里的 base64 编解码（`#[serde(with = "b64")]`）。原始 `Vec<u8>` 经 serde
/// 会序列化成 JSON 数字数组（每字节形如 `123,` ≈ 4 字符）、膨胀约 4 倍；base64 仅膨胀 ~1.33x，
/// 等量数据传输量降到约 1/3、接近原始二进制。app / server 对此无感知（字段仍是 `Vec<u8>`，
/// 服务端继续盲转 [`serde_json::Value`]）。
mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    /// `Vec<u8>` → base64 字符串。
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    /// base64 字符串 → `Vec<u8>`（非法 base64 回 serde 自定义错误）。
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

impl RemoteFrame {
    /// 序列化为不透明 JSON 值，包进 [`RemoteC2S::Relay`] / [`RemoteS2C::Relay`]。
    ///
    /// # Errors
    /// 序列化失败（理论上不会发生，除非自定义 `Serialize` 出错）时返回 serde 错误。
    pub fn to_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// 从 [`RemoteS2C::Relay`] 收到的不透明 JSON 值还原强类型帧。
    ///
    /// # Errors
    /// 当 JSON 不是本版本认识的 [`RemoteFrame`] 变体时返回 serde 错误——调用方
    /// 应 `warn` 后丢弃该帧（前向兼容：新版本对端可能发来本端未知的变体）。
    pub fn from_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
    }
}

/// 客户端 → 服务端的远程控制消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteC2S {
    /// 控制端发起：请求控制 `target` 设备（同账户、在线、未被占用）。
    RequestControl {
        /// 目标（被控端）设备 id。
        target: String,
    },
    /// 控制端提交被控端展示的配对码。
    SubmitPairing {
        /// 目标设备 id（确定提交针对哪个未决配对）。
        target: String,
        /// 用户输入的 9 位配对码。
        code: String,
    },
    /// 被控端拒绝当前未决的控制请求（dismiss 配对码并通知控制端）。
    DeclineControl,
    /// 任一端主动结束当前活跃会话。
    EndSession,
    /// 数据面：把不透明帧盲转给会话对端（载荷由 [`RemoteFrame`] 序列化而来）。
    Relay(serde_json::Value),
    /// 应用层心跳：保活 + 刷新在线状态，服务端回 [`RemoteS2C::Pong`]。
    Ping,
}

/// 服务端 → 客户端的远程控制消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteS2C {
    /// 连接建立后立即下发：协议版本协商 + 确认本设备 id。
    Welcome {
        /// 服务端当前协议版本。
        protocol_version: u32,
        /// 服务端仍兼容的最低客户端协议版本（低于此应提示用户升级）。
        min_supported_version: u32,
        /// 服务端据 JWT 确认的本设备 id。
        device_id: String,
    },
    /// 发给**被控端**：有控制端请求控制你，展示配对码供其转述输入。
    ControlRequested {
        /// 控制端设备 id。
        controller_device_id: String,
        /// 控制端设备显示名。
        controller_name: String,
        /// 9 位配对码（被控端醒目展示）。
        pairing_code: String,
        /// 配对有效期（秒），UI 可倒计时。
        expires_in_secs: u32,
    },
    /// 发给**控制端**：请求已送达被控端，请输入其展示的配对码。
    PairingNeeded {
        /// 目标（被控端）设备 id。
        target_device_id: String,
        /// 目标设备显示名。
        target_name: String,
        /// 配对有效期（秒）。
        expires_in_secs: u32,
    },
    /// 发给**控制端**：配对码校验结果（仅在失败时下发；成功走 [`Self::SessionStarted`]）。
    PairingResult {
        /// 失败原因。
        reason: PairingFailReason,
        /// 剩余尝试次数（0 表示配对已作废）。
        attempts_left: u32,
    },
    /// 发给**控制端**：控制请求被拒（同步失败 / 被控端拒绝 / 取消）。
    ControlDenied {
        /// 目标设备 id。
        target_device_id: String,
        /// 机器可读原因。
        reason: DenyReason,
    },
    /// 发给**被控端**：未决配对被取消（控制端撤销 / 超时），dismiss 配对码。
    PairingCancelled {
        /// 机器可读原因。
        reason: DenyReason,
    },
    /// 会话已建立（双方各收一份，含对端信息与本端角色）。
    SessionStarted {
        /// 对端设备 id。
        peer_device_id: String,
        /// 对端设备显示名。
        peer_name: String,
        /// 本端角色。
        role: Role,
    },
    /// 会话已结束（双方各收一份）。
    SessionEnded {
        /// 机器可读原因。
        reason: EndReason,
    },
    /// 数据面：来自会话对端的不透明帧（用 [`RemoteFrame::from_value`] 还原）。
    Relay(serde_json::Value),
    /// 心跳应答。
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c2s_往返() {
        let msg = RemoteC2S::SubmitPairing {
            target: "dev-2".into(),
            code: "123456789".into(),
        };
        let json = serde_json::to_string(&msg).expect("序列化");
        let back: RemoteC2S = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, msg);
    }

    #[test]
    fn s2c_往返带枚举原因() {
        let msg = RemoteS2C::ControlDenied {
            target_device_id: "dev-2".into(),
            reason: DenyReason::AlreadyControlled,
        };
        let json = serde_json::to_string(&msg).expect("序列化");
        let back: RemoteS2C = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, msg);
    }

    #[test]
    fn remoteframe_经value往返() {
        // 数据面：RemoteFrame → Value → 包进 Relay → 解包 → 还原 RemoteFrame。
        let frame = RemoteFrame::Echo("hello".into());
        let value = frame.to_value().expect("转 value");
        let c2s = RemoteC2S::Relay(value.clone());
        // 模拟服务端盲转：原样取出 value，无需认识 RemoteFrame。
        let RemoteC2S::Relay(relayed) = c2s else {
            panic!("应为 Relay");
        };
        let back = RemoteFrame::from_value(&relayed).expect("还原");
        assert_eq!(back, frame);
    }

    #[test]
    fn part3d_尺寸与布局帧经value往返() {
        // SubViewport（绝对网格）+ SubLayout（相对比例，双向）的 value 往返。
        for frame in [
            RemoteFrame::SubViewport {
                tab_id: 7,
                panes: vec![
                    PaneViewport { session_id: 3, rows: 40, cols: 100 },
                    PaneViewport { session_id: 5, rows: 40, cols: 60 },
                ],
            },
            RemoteFrame::SubLayout {
                tab_id: 7,
                row_weights: vec![0.3, 0.7],
                col_weights: vec![vec![0.5, 0.5], vec![0.4, 0.6]],
            },
        ] {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn part4_输入与per_pane回看帧经value往返() {
        for frame in [
            RemoteFrame::InputWithId {
                tab_id: 3,
                session_id: 7,
                data: vec![0x03, b'l', b's', b'\r'],
            },
            RemoteFrame::HistoryReqForPane {
                tab_id: 3,
                session_id: 7,
                top: 1200,
                count: 40,
            },
            RemoteFrame::HistoryRowsForPane {
                session_id: 7,
                top: 1200,
                base: 1000,
                screen_top: 1400,
                lines: vec![vec![b'a'], vec![]],
            },
            RemoteFrame::PaneOp {
                tab_id: 3,
                session_id: 7,
                op: PaneOpKind::New,
            },
            RemoteFrame::PaneOp {
                tab_id: 3,
                session_id: 7,
                op: PaneOpKind::Close,
            },
            RemoteFrame::PaneOp {
                tab_id: 3,
                session_id: 7,
                op: PaneOpKind::ToggleMaximize,
            },
            RemoteFrame::PaneOp {
                tab_id: 3,
                session_id: 7,
                op: PaneOpKind::SwapWith { other: 9 },
            },
        ] {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn m6_p2p信令帧经value往返() {
        // M6 Phase 0：P2pSignal 四阶段经 Relay 盲转往返（服务端零改动、前向兼容）。
        // payload 内层结构 Phase 2 才定，此处用占位 JSON 字符串验证信封序列化。
        for kind in [
            P2pSignalKind::Offer,
            P2pSignalKind::Answer,
            P2pSignalKind::Ready,
            P2pSignalKind::Fallback,
        ] {
            let frame = RemoteFrame::P2pSignal {
                kind,
                payload: r#"{"candidates":["192.168.1.85:51820"],"fp":"sha256:deadbeef"}"#.into(),
            };
            // 模拟服务端盲转：RemoteFrame → Value → 包进 Relay → 原样取出 → 还原。
            let value = frame.to_value().expect("to_value");
            let RemoteC2S::Relay(relayed) = RemoteC2S::Relay(value) else {
                unreachable!()
            };
            let back = RemoteFrame::from_value(&relayed).expect("from_value");
            assert_eq!(back, frame);
        }
        // Phase 3 数据面首帧（走 QUIC 直连流，仍经 to_value/from_value 分帧）。
        let hello = RemoteFrame::P2pStreamHello;
        let v = hello.to_value().expect("to_value");
        assert_eq!(RemoteFrame::from_value(&v).expect("from_value"), hello);
    }

    #[test]
    fn 镜像帧经value往返() {
        for frame in [
            RemoteFrame::Output(vec![0x1b, b'[', b'2', b'J']),
            RemoteFrame::Resize { rows: 40, cols: 120 },
        ] {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn option_b_帧经value往返() {
        // part3c-2 Option B 浏览 + 双向传输全部新变体的 value 往返（含字节块 / 撞名 / 错误）。
        let frames = vec![
            RemoteFrame::RootChanged {
                path: "C:\\Users\\hf".into(),
            },
            RemoteFrame::ListDir {
                req_id: 1,
                path: "C:\\Users\\hf".into(),
                show_hidden: true,
            },
            RemoteFrame::ListDirResult {
                req_id: 1,
                path: "C:\\Users\\hf".into(),
                entries: vec![
                    DirEntry {
                        path: "C:\\Users\\hf\\sub".into(),
                        name: "sub".into(),
                        is_dir: true,
                        size: 0,
                    },
                    DirEntry {
                        path: "C:\\Users\\hf\\a.txt".into(),
                        name: "a.txt".into(),
                        is_dir: false,
                        size: 1234,
                    },
                ],
                overflow: 3,
                err: None,
            },
            RemoteFrame::ListDirResult {
                req_id: 2,
                path: "C:\\x".into(),
                entries: vec![],
                overflow: 0,
                err: Some(FsErr::PermissionDenied),
            },
            RemoteFrame::FetchReq {
                req_id: 5,
                path: "C:\\a.bin".into(),
            },
            RemoteFrame::FetchCancel { req_id: 5 },
            RemoteFrame::FileBegin {
                req_id: 5,
                total_len: 1234,
            },
            RemoteFrame::FileChunk {
                req_id: 5,
                seq: 0,
                data: vec![0, 255, 13, 10],
            },
            RemoteFrame::FileChunkAck { req_id: 5, seq: 0 },
            RemoteFrame::FileEnd { req_id: 5 },
            RemoteFrame::FileErr {
                req_id: 5,
                err: FsErr::TooLarge,
            },
            RemoteFrame::PutBegin {
                req_id: 9,
                dir: "C:\\dst".into(),
                name: "f.txt".into(),
                total_len: 7,
                overwrite: PutOverwrite::Probe,
            },
            RemoteFrame::PutReady {
                req_id: 9,
                conflict: Some(PutConflict {
                    is_dir: false,
                    existing_len: 42,
                }),
                err: None,
            },
            RemoteFrame::PutReady {
                req_id: 9,
                conflict: None,
                err: Some(FsErr::Io),
            },
            RemoteFrame::PutChunk {
                req_id: 9,
                seq: 0,
                data: vec![1, 2, 3],
            },
            RemoteFrame::PutChunkAck { req_id: 9, seq: 0 },
            RemoteFrame::PutEnd { req_id: 9 },
            RemoteFrame::PutResult {
                req_id: 9,
                status: PutStatus::Written,
                err: None,
            },
            RemoteFrame::PutResult {
                req_id: 9,
                status: PutStatus::Skipped,
                err: None,
            },
            RemoteFrame::MkDir {
                req_id: 10,
                dir: "C:\\dst".into(),
                name: "sub".into(),
            },
            RemoteFrame::MkDirResult {
                req_id: 10,
                path: "C:\\dst\\sub".into(),
                err: None,
            },
            RemoteFrame::MkDirResult {
                req_id: 11,
                path: String::new(),
                err: Some(FsErr::PermissionDenied),
            },
            RemoteFrame::MkFile {
                req_id: 12,
                dir: "C:\\dst".into(),
                name: "new.txt".into(),
            },
            RemoteFrame::MkFileResult {
                req_id: 12,
                path: "C:\\dst\\new.txt".into(),
                err: None,
            },
            RemoteFrame::Delete {
                req_id: 13,
                path: "C:\\dst\\old.txt".into(),
                is_dir: false,
            },
            RemoteFrame::DeleteResult {
                req_id: 13,
                err: None,
            },
            RemoteFrame::DeleteResult {
                req_id: 14,
                err: Some(FsErr::NotFound),
            },
        ];
        for frame in frames {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn list_dir_recursive_帧经value往返() {
        // 片8 目录递归：请求 + 成功（DFS 嵌套序：父目录项在子项前 + 空目录单独成项）+
        // 截断 + 错误 四场景的 value 往返。
        let frames = vec![
            RemoteFrame::ListDirRecursive {
                req_id: 7,
                path: "C:\\proj".into(),
                show_hidden: false,
                max_depth: LIST_DIR_RECURSIVE_MAX_DEPTH,
                max_entries: LIST_DIR_RECURSIVE_MAX_ENTRIES,
            },
            RemoteFrame::ListDirRecursiveResult {
                req_id: 7,
                path: "C:\\proj".into(),
                // DFS 序：sub/ 目录项在其子项 sub/a.txt 之前；empty/ 空目录单独成项。
                entries: vec![
                    RecursiveDirEntry {
                        rel_path: "sub".into(),
                        path: "C:\\proj\\sub".into(),
                        is_dir: true,
                        size: 0,
                    },
                    RecursiveDirEntry {
                        rel_path: "sub/a.txt".into(),
                        path: "C:\\proj\\sub\\a.txt".into(),
                        is_dir: false,
                        size: 1234,
                    },
                    RecursiveDirEntry {
                        rel_path: "empty".into(),
                        path: "C:\\proj\\empty".into(),
                        is_dir: true,
                        size: 0,
                    },
                    RecursiveDirEntry {
                        rel_path: "root.txt".into(),
                        path: "C:\\proj\\root.txt".into(),
                        is_dir: false,
                        size: 9,
                    },
                ],
                truncated: false,
                err: None,
            },
            RemoteFrame::ListDirRecursiveResult {
                req_id: 8,
                path: "C:\\big".into(),
                entries: vec![RecursiveDirEntry {
                    rel_path: "x.bin".into(),
                    path: "C:\\big\\x.bin".into(),
                    is_dir: false,
                    size: 1,
                }],
                truncated: true,
                err: None,
            },
            RemoteFrame::ListDirRecursiveResult {
                req_id: 9,
                path: "C:\\nope".into(),
                entries: vec![],
                truncated: false,
                err: Some(FsErr::PermissionDenied),
            },
        ];
        for frame in frames {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn list_dir_recursive_满载单帧不超上限() {
        // 上限自洽：MAX_ENTRIES 项的应答序列化后应远小于 server 4 MiB 单帧上限。
        let entries: Vec<RecursiveDirEntry> = (0..LIST_DIR_RECURSIVE_MAX_ENTRIES)
            .map(|i| RecursiveDirEntry {
                rel_path: format!("dir{}/sub{}/file{i}.dat", i % 64, i % 8),
                path: format!("C:\\root\\dir{}\\sub{}\\file{i}.dat", i % 64, i % 8),
                is_dir: false,
                size: u64::from(i),
            })
            .collect();
        let frame = RemoteFrame::ListDirRecursiveResult {
            req_id: 1,
            path: "C:\\root".into(),
            entries,
            truncated: true,
            err: None,
        };
        let v = frame.to_value().expect("to_value");
        let json = serde_json::to_string(&v).expect("序列化");
        // 4 MiB 单帧上限（server ws.rs MAX_WS_MESSAGE）。留足余量。
        assert!(
            json.len() < 4 * 1024 * 1024,
            "满载 {} 项 JSON {} 字节，超 4 MiB 帧上限",
            LIST_DIR_RECURSIVE_MAX_ENTRIES,
            json.len()
        );
    }

    #[test]
    fn 历史分页帧经value往返() {
        for frame in [
            RemoteFrame::HistoryReq { top: 1000, count: 40 },
            RemoteFrame::HistoryRows {
                top: 1000,
                base: 0,
                screen_top: 1234,
                lines: vec![vec![b'h', b'i'], Vec::new(), vec![0x1b, b'[', b'0', b'm']],
            },
            RemoteFrame::HistoryBounds { base: 0, screen_top: 1234 },
        ] {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn 服务端盲转无需识别帧内部() {
        // 关键不变量：构造一个本版本「未知」的帧 JSON（模拟未来 part2/3 的变体），
        // 服务端只需搬运 Value、不反序列化即可转发；客户端旧版本 from_value 才报错。
        let future = serde_json::json!({ "FutureVariant": { "x": 1 } });
        let c2s = RemoteC2S::Relay(future.clone());
        let json = serde_json::to_string(&c2s).expect("序列化");
        // 服务端能完整解析外层 RemoteC2S（不因内层未知而失败）。
        let parsed: RemoteC2S = serde_json::from_str(&json).expect("外层可解析");
        let RemoteC2S::Relay(v) = parsed else {
            panic!("应为 Relay");
        };
        assert_eq!(v, future);
        // 旧版本客户端尝试强类型还原才会失败（应 warn 后丢弃）。
        assert!(RemoteFrame::from_value(&v).is_err());

        // part3c-2：含原始字节块的真实帧同样被服务端原样搬运（无需识别内层即可中继）。
        let chunk = RemoteFrame::FileChunk {
            req_id: 1,
            seq: 0,
            data: vec![0, 255, 1, 2],
        };
        let cv = chunk.to_value().expect("to_value");
        let RemoteC2S::Relay(inner) = RemoteC2S::Relay(cv.clone()) else {
            panic!("应为 Relay");
        };
        assert_eq!(inner, cv);
        assert_eq!(RemoteFrame::from_value(&inner).expect("还原"), chunk);
    }

    #[test]
    fn part3d_多会话多窗格帧经value往返() {
        // part3d 全部新变体的 value 往返（会话列表/状态 + 订阅 + 双 id 内容 + 增删 + 结果）。
        let frames = vec![
            RemoteFrame::TabListSnapshot {
                tabs: vec![
                    TabState {
                        id: 0,
                        name: "会话 1".into(),
                        path: Some("C:\\proj".into()),
                        busy: false,
                        unseen: true,
                        pane_count: 1,
                        // 带图标：验证 Some(IconBitmap) 经 base64 往返。
                        icon: Some(IconBitmap {
                            w: 2,
                            h: 2,
                            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                        }),
                    },
                    TabState {
                        id: 3,
                        name: "build".into(),
                        path: None,
                        busy: true,
                        unseen: false,
                        pane_count: 4,
                        icon: None,
                    },
                ],
            },
            RemoteFrame::TabCreated {
                tab: TabState {
                    id: 5,
                    name: "新会话".into(),
                    path: None,
                    busy: false,
                    unseen: false,
                    pane_count: 1,
                    icon: None,
                },
            },
            RemoteFrame::TabClosed { tab_id: 3 },
            RemoteFrame::TabUpdated {
                tab: TabState {
                    id: 0,
                    name: "renamed".into(),
                    path: Some("C:\\proj\\sub".into()),
                    busy: true,
                    unseen: false,
                    pane_count: 2,
                    icon: None,
                },
            },
            RemoteFrame::SubscribeSession { tab_id: 0 },
            RemoteFrame::SubscriptionStarted {
                tab_id: 0,
                focused: 1,
                panes: vec![
                    PaneSnapshot {
                        session_id: 10,
                        rows: 40,
                        cols: 120,
                        snapshot: vec![0x1b, b'[', b'2', b'J'],
                        base: 0,
                        screen_top: 40,
                        custom_title: None,
                    },
                    PaneSnapshot {
                        session_id: 11,
                        rows: 40,
                        cols: 120,
                        snapshot: vec![0x1b, b'[', b'H', b'h', b'i'],
                        base: 5,
                        screen_top: 100,
                        custom_title: Some("日志".into()),
                    },
                ],
                // 2 窗格 → 网格 [2]（一排两列）；用精确 f32（0.5/1.0）确保 PartialEq 往返。
                row_weights: vec![1.0],
                col_weights: vec![vec![0.5, 0.5]],
                maximized: None,
            },
            RemoteFrame::OutputWithId {
                tab_id: 0,
                session_id: 11,
                data: vec![0, 255, 13, 10, 0x1b],
            },
            RemoteFrame::ResizeWithId {
                tab_id: 0,
                session_id: 11,
                rows: 50,
                cols: 200,
            },
            RemoteFrame::NewTab { req_id: 1 },
            RemoteFrame::NewTabResult {
                req_id: 1,
                tab_id: Some(7),
                err: None,
            },
            RemoteFrame::NewTabResult {
                req_id: 2,
                tab_id: None,
                err: Some(RemoteOpErr::LimitReached),
            },
            RemoteFrame::CloseTab { req_id: 3, tab_id: 7 },
            RemoteFrame::CloseTabResult {
                req_id: 3,
                tab_id: 7,
                err: None,
            },
            RemoteFrame::CloseTabResult {
                req_id: 4,
                tab_id: 99,
                err: Some(RemoteOpErr::NotFound),
            },
            RemoteFrame::SubViewport {
                tab_id: 0,
                panes: vec![
                    PaneViewport {
                        session_id: 10,
                        rows: 40,
                        cols: 120,
                    },
                    PaneViewport {
                        session_id: 11,
                        rows: 20,
                        cols: 80,
                    },
                ],
            },
        ];
        for frame in frames {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn part3d_订阅快照满载单帧不超上限() {
        // 最坏：一个会话 MAX_PANES(=6，镜像 app session::MAX_PANES) 个窗格，每格整屏快照 256 KiB
        // （远超真实满彩屏 ~80 KB）。base64 ~1.33x，6 格 ~2 MiB，应 < server 4 MiB 单帧上限。
        const MAX_PANES: usize = 6;
        const PANE_SNAP: usize = 256 * 1024;
        let panes: Vec<PaneSnapshot> = (0..MAX_PANES)
            .map(|i| PaneSnapshot {
                session_id: i as u64,
                rows: 60,
                cols: 200,
                snapshot: vec![b'x'; PANE_SNAP],
                base: 0,
                screen_top: 60,
                custom_title: Some(format!("pane{i}")),
            })
            .collect();
        let frame = RemoteFrame::SubscriptionStarted {
            tab_id: 0,
            focused: 0,
            panes,
            // 6 窗格 → 网格 [3,3]（上下两排各三列）。
            row_weights: vec![0.5, 0.5],
            col_weights: vec![vec![1.0 / 3.0; 3], vec![1.0 / 3.0; 3]],
            maximized: None,
        };
        let v = frame.to_value().expect("to_value");
        let json = serde_json::to_string(&v).expect("序列化");
        assert!(
            json.len() < 4 * 1024 * 1024,
            "满载 {MAX_PANES} 窗格 × {PANE_SNAP} B 快照 → JSON {} 字节，超 4 MiB 帧上限",
            json.len()
        );
    }

    #[test]
    fn part3d_会话列表满载单帧不超上限() {
        // 上限自洽：REMOTE_MAX_SESSIONS 个会话（名 / 路径取长串）序列化后远小于 4 MiB 单帧上限。
        let tabs: Vec<TabState> = (0..REMOTE_MAX_SESSIONS)
            .map(|i| TabState {
                id: u64::from(i),
                name: format!("会话名字够长够长够长够长够长够长 {i}"),
                path: Some(format!(
                    "C:\\some\\deeply\\nested\\working\\directory\\path\\number\\{i}"
                )),
                busy: i % 2 == 0,
                unseen: i % 3 == 0,
                pane_count: (i % 6) + 1,
                // 满载每会话都带 32×32 图标，验证「带图标满载」仍远小于 4 MiB 单帧上限。
                icon: Some(IconBitmap {
                    w: 32,
                    h: 32,
                    rgba: vec![0u8; 32 * 32 * 4],
                }),
            })
            .collect();
        let frame = RemoteFrame::TabListSnapshot { tabs };
        let v = frame.to_value().expect("to_value");
        let json = serde_json::to_string(&v).expect("序列化");
        assert!(
            json.len() < 4 * 1024 * 1024,
            "满载 {REMOTE_MAX_SESSIONS} 会话 JSON {} 字节，超 4 MiB 帧上限",
            json.len()
        );
    }
}
