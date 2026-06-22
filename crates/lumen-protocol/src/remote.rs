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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Input(Vec<u8>),
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
}
