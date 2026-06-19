//! 文件树栏（M3.3，M3.6 增强）：跟随激活会话 cwd 的目录树。
//!
//! `egui_ltreeview` 的全部用法收敛在本模块（设计文档风险 #6：该 crate
//! 单一维护者，集中收敛便于日后整体替换为自绘实现）。
//!
//! 行为规格（docs/M3应用外壳设计.md §3.5 / §4 第②行）：
//! - 树根 = 激活会话上报的 cwd（OSC 9;9）；未上报时显示等待占位；
//!   切 tab / cd 后树根跟随，根变化时重置展开状态。
//! - 懒加载：目录首次展开才读子项；目录在前文件在后、各按名排序；
//!   隐藏项（Windows Hidden 属性或点开头）默认不显示。
//! - 读盘在后台线程进行（M3 审查项：UI 线程同步 read_dir 会被慢速
//!   网络盘/超大目录冻结整个应用）：首次展开先画「加载中…」占位并
//!   派发后台读取，回包经 mpsc 通道送回 UI 线程替换占位；换根/刷新
//!   按代次号丢弃旧回包，防旧根结果污染新树。
//! - 单层条目上限 1000，超出折叠成「…还有 N 项未显示」占位行
//!   （ltreeview 无虚拟化，这是万级目录不卡的保障）；枚举本身另设
//!   硬上限，十万级目录的枚举成本也封顶。
//! - 读目录失败（权限/网络盘）显示灰色「无法读取」占位行，不 panic。
//! - 激活（双击/回车）目录 → shell 空闲时请求注入 cd，忙时仅提示；
//!   激活文件 → 系统默认程序打开；单击仅选中，开合走 closer 三角。
//!
//! M3.6 文件树增强（需求池 P3）：
//! - **拖动到终端插入路径**：节点拖出树、在终端区释放 →
//!   [`path_insert_text`] 生成转义后的路径文本写 PTY（不带回车）。
//!   drop 落点判定在 shell/mod.rs（要等 CentralPanel 布局出终端矩形）。
//! - **右键菜单**：新建文件/文件夹（小弹窗输入名字）、删除（确认后
//!   移入回收站，trash crate）、在文件管理器中打开、复制绝对/相对
//!   路径。新建/删除在后台线程执行，结果经 toast 反馈；成功后只刷新
//!   所在目录（按路径恢复后代目录的展开状态，回包仍按代次+请求号
//!   作废）。
//! - **搜索**：工具条 🔍 展开输入框，≥2 字符防抖后派发后台递归扫描
//!   （深度/结果数/枚举量三重封顶），搜索态用扁平相对路径列表替代
//!   树；双击沿用节点语义（目录=cd、文件=系统打开），Esc/再点 🔍
//!   收起并恢复树视图。
//!
//! M3.6b（需求池 P7/P8）：
//! - 目录右键菜单增加「进入文件夹」（与双击目录同链路，树根也有）。
//! - 鼠标双击激活在 egui_ltreeview 0.7.0 存在上游 bug（Activate 不可
//!   达，P8「双击文件不打开」的根因），以 [`merge_double_click_activation`]
//!   合成规避，详见该函数文档。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use egui_ltreeview::{Action, NodeBuilder, TreeView, TreeViewBuilder, TreeViewState};

use super::theme;
use super::toast::ToastKind;

/// 单层最多展示的条目数。
const MAX_ENTRIES_PER_DIR: usize = 1000;
/// 目录枚举的硬上限（含被展示的部分）：超大目录（十万级）只枚举到
/// 这里即提前终止，枚举成本封顶；此时溢出计数是「至少还有 N 项」的
/// 下界（截断计数），展示文案不变。
const ENUM_HARD_CAP: usize = MAX_ENTRIES_PER_DIR * 10;
/// 「shell 忙」轻提示的展示时长。
const HINT_DURATION: Duration = Duration::from_secs(3);
/// 后台读目录/文件操作/搜索在途时的回包轮询间隔（本应用事件循环只
/// 消费 egui 的 repaint_delay，worker 线程无法直接唤醒重绘，靠它驱动
/// 下一帧收包）。
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 搜索触发的最少字符数（不足时维持树视图）。
const SEARCH_MIN_CHARS: usize = 2;
/// 搜索输入的防抖窗口：停止输入这么久才派发扫描，避免每个按键都
/// 起一轮全树递归。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// 搜索结果上限（扁平列表无虚拟化，同时也是给用户的「该收敛关键词
/// 了」信号；触顶视为截断）。
const SEARCH_MAX_RESULTS: usize = 500;
/// 搜索递归深度上限（树根为第 0 层，最深扫到第 8 层的条目）。
const SEARCH_MAX_DEPTH: usize = 8;
/// 搜索枚举的硬上限（沿用 [`ENUM_HARD_CAP`] 思路）：全树合计访问的
/// 条目数封顶，巨型仓库的扫描成本可控；触顶视为截断。
const SEARCH_ENUM_CAP: usize = ENUM_HARD_CAP * 10;

/// 节点种类（Overflow/Unreadable/Loading 是不可交互的占位行）。
#[derive(Clone, Copy)]
enum NodeKind {
    Dir,
    File,
    /// 该层还有 N 项因单层上限未显示。
    Overflow(usize),
    /// 目录读取失败（权限/网络盘断连）。
    Unreadable,
    /// 后台读取中的「加载中…」占位。
    Loading,
}

/// 一个树节点：id 即它在 `FileTreeState::nodes` 中的下标。
struct NodeInfo {
    path: PathBuf,
    kind: NodeKind,
}

/// 已读目录的子节点 id 列表（含占位行；懒加载产物）。
struct DirListing {
    children: Vec<usize>,
}

/// 后台读目录的回包（worker 线程 → UI 线程）。
struct LoadReply {
    /// 派发时刻的代次号：换根/刷新后旧代回包直接丢弃。
    epoch: u64,
    /// 派发时刻的请求号：同一目录节点可因「目录级刷新」被重复派发，
    /// 仅最新请求的回包被接受（同代次内新旧回包到达顺序无保证）。
    seq: u64,
    /// 被读取的目录节点 id。
    dir_id: usize,
    /// `Ok((已排序截断的 (路径, 是否目录) 列表, 溢出条数))`；
    /// `Err(())` 表示读取失败（权限/网络盘断连）。
    result: Result<(Vec<(PathBuf, bool)>, usize), ()>,
}

/// 后台文件操作结果枚举（F6：后台线程不持有翻译文本；UI 侧展示时查 i18n 表组装）。
///
/// 注意：`Reveal`（在文件管理器中打开）在 UI 线程同步执行，失败时直接推
/// toast，不走此枚举；枚举只覆盖真正在后台线程完成的新建/删除操作。
enum OpResult {
    /// 创建成功，name = 创建的文件/目录名
    Created(String),
    /// 创建失败：目标已存在，name = 冲突项名
    CreateExists(String),
    /// 创建失败：其他错误，msg = 系统错误文本
    CreateFailed(String),
    /// 移入回收站成功，name = 被删项名
    Trashed(String),
    /// 删除失败，msg = 系统错误文本
    DeleteFailed(String),
}

impl OpResult {
    /// 取当前语言的展示文案和 toast 级别。
    fn to_toast(&self) -> (super::toast::ToastKind, String) {
        use super::toast::ToastKind;
        let s = crate::i18n::strings();
        match self {
            Self::Created(name) => (
                ToastKind::Info,
                crate::i18n::fmt1(s.filetree_created_fmt, name),
            ),
            Self::CreateExists(name) => (
                ToastKind::Warn,
                crate::i18n::fmt1(s.filetree_create_exists_fmt, name),
            ),
            Self::CreateFailed(msg) => (
                ToastKind::Error,
                crate::i18n::fmt1(s.filetree_create_failed_fmt, msg),
            ),
            Self::Trashed(name) => (
                ToastKind::Info,
                crate::i18n::fmt1(s.filetree_trashed_fmt, name),
            ),
            Self::DeleteFailed(msg) => (
                ToastKind::Error,
                crate::i18n::fmt1(s.filetree_delete_failed_fmt, msg),
            ),
        }
    }
}

/// 后台文件操作（新建/删除）的回包。
///
/// 不带代次号：操作已真实发生，结果 toast 无论树状态如何都要展示；
/// 仅「刷新所在目录」按路径在当前树里查找，根已切走时自然落空。
struct OpReply {
    /// 操作成功后需要刷新的目录（失败不刷新）。
    refresh: Option<PathBuf>,
    /// 操作结果（F6 枚举化，UI 侧取当前语言文案）。
    result: OpResult,
}

/// 后台搜索扫描的回包。
struct SearchReply {
    /// 派发时刻的搜索代次：输入变化/清空后旧代回包直接丢弃。
    epoch: u64,
    /// 匹配的 (路径, 是否目录) 列表（BFS 序：浅层优先）。
    items: Vec<(PathBuf, bool)>,
    /// 结果触顶/枚举触顶/扫描被作废提前退出，结果不完整。
    truncated: bool,
}

/// 当前展示的搜索结果。
struct SearchResults {
    items: Vec<(PathBuf, bool)>,
    truncated: bool,
}

/// 进行中的对话框（模态小弹窗；打开期间 main 把键盘焦点交给 egui）。
enum Dialog {
    /// 新建文件/文件夹：在 `dir` 下输入名字。
    Create {
        dir: PathBuf,
        is_dir: bool,
        name: String,
        /// 刚打开：下一帧把焦点交给输入框。
        focus: bool,
        /// 名字校验失败的红字提示。
        error: Option<String>,
    },
    /// 删除确认（移入回收站）。
    ConfirmDelete { path: PathBuf, is_dir: bool },
}

/// 右键菜单点选的动作（菜单闭包经 RefCell 回传，树绘制结束后处理）。
enum MenuAction {
    /// 进入文件夹（cd 过去，与双击目录同一条链路：忙闸门 + 提示）。
    EnterDir(PathBuf),
    /// 在目录下新建文件/文件夹（弹输入框）。
    Create { dir: PathBuf, is_dir: bool },
    /// 删除（先弹确认，确认后移入回收站）。
    Delete { path: PathBuf, is_dir: bool },
    /// 在文件管理器中打开并选中。
    Reveal(PathBuf),
    /// 复制绝对路径。
    CopyAbs(PathBuf),
    /// 复制相对树根的路径。
    CopyRel(PathBuf),
    /// part3c-2 #7/上传：复制文件 / 文件夹到文件剪贴板（上传源；区别于 CopyAbs/Rel 复制文本）。
    CopyFiles { path: PathBuf, is_dir: bool },
    /// part3c-2 #7：把剪贴板里的远程项粘贴（下载）到此本地目录。
    PasteInto(PathBuf),
}

/// 文件树的跨帧状态。
pub struct FileTreeState {
    /// 栏是否展开（工具条按钮 / Ctrl+B 切换）。
    pub visible: bool,
    /// 当前树根（= 激活会话 cwd）。None 显示等待占位。
    root: Option<PathBuf>,
    /// ltreeview 的展开/选中状态。自持有而非存 egui memory：根变化时
    /// 直接整体重置，无需造一次性 id。
    tree: TreeViewState<usize>,
    /// 节点表（append-only，根变化/刷新时整体重建）。根节点恒为 id 0。
    nodes: Vec<NodeInfo>,
    /// 懒加载缓存：目录节点 id → 子节点列表（在途项是「加载中…」
    /// 占位，回包到达后整体替换）。
    listings: HashMap<usize, DirListing>,
    /// 在途后台读取：目录节点 id → 请求号（非空时驱动轮询重绘收包；
    /// 目录级刷新会对同一 id 重复派发，仅最新请求号的回包被接受）。
    pending: HashMap<usize, u64>,
    /// 读目录请求号分配器（单调递增）。
    load_seq: u64,
    /// 代次号：换根/刷新时 +1，旧代回包按号丢弃。
    epoch: u64,
    /// 后台读目录的回包通道（tx 克隆给 worker 线程）。
    reply_tx: mpsc::Sender<LoadReply>,
    reply_rx: mpsc::Receiver<LoadReply>,
    /// 「shell 忙」提示的过期时刻。
    hint_until: Option<Instant>,
    /// 目录级刷新后需要恢复展开状态的目录路径（回包建新节点时消费）。
    restore_open: HashSet<PathBuf>,
    /// 进行中的对话框（新建输入/删除确认）。
    dialog: Option<Dialog>,
    /// 后台文件操作（新建/删除）的回包通道。
    op_tx: mpsc::Sender<OpReply>,
    op_rx: mpsc::Receiver<OpReply>,
    /// 在途后台文件操作数（非零时驱动轮询重绘收包）。
    ops_pending: usize,
    /// 搜索框是否展开。
    search_open: bool,
    /// 搜索输入缓冲。
    search_query: String,
    /// 搜索框刚展开：下一帧把焦点交给输入框。
    search_focus: bool,
    /// 防抖：到点才真正派发扫描（每次输入变化后推）。
    search_dispatch_at: Option<Instant>,
    /// 当前有效的搜索代次（Arc 共享给 worker 用于提前退出）。
    search_epoch: Arc<AtomicU64>,
    /// 搜索回包通道。
    search_tx: mpsc::Sender<SearchReply>,
    search_rx: mpsc::Receiver<SearchReply>,
    /// 当前展示的搜索结果（None = 扫描在途或尚未派发）。
    search_results: Option<SearchResults>,
    /// 搜索扫描在途（驱动轮询重绘收包）。
    search_pending: bool,
    /// 搜索结果列表的选中下标（单击定位）。
    search_selected: Option<usize>,
}

impl Default for FileTreeState {
    fn default() -> Self {
        let (reply_tx, reply_rx) = mpsc::channel();
        let (op_tx, op_rx) = mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        Self {
            visible: true,
            root: None,
            tree: TreeViewState::default(),
            nodes: Vec::new(),
            listings: HashMap::new(),
            pending: HashMap::new(),
            load_seq: 0,
            epoch: 0,
            reply_tx,
            reply_rx,
            hint_until: None,
            restore_open: HashSet::new(),
            dialog: None,
            op_tx,
            op_rx,
            ops_pending: 0,
            search_open: false,
            search_query: String::new(),
            search_focus: false,
            search_dispatch_at: None,
            search_epoch: Arc::new(AtomicU64::new(0)),
            search_tx,
            search_rx,
            search_results: None,
            search_pending: false,
            search_selected: None,
        }
    }
}

impl FileTreeState {
    /// 是否有对话框（新建输入/删除确认）打开。main 据此把键盘/IME
    /// 焦点交给 egui（与重命名编辑同款仲裁）。
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// 搜索输入框是否展开。其 `egui::TextEdit` 聚焦时能接收 IME 合成，
    /// 故 main 的 IME 路由仲裁（`ime_should_route_to_composer`）必须把它
    /// 视作覆盖层、把 IME 焦点交给 egui，否则激进路由会把往搜索框打的
    /// 中文劫持进终端 composer（对抗审查 IME 项）。与 `dialog_open` 同款。
    pub fn search_open(&self) -> bool {
        self.search_open
    }

    /// 文件树栏当前实际占用宽度（逻辑点）：展开时为用户设置的展开宽度，
    /// 收起时返回 0.0（问题7：收起态不再画窄条，不占用任何宽度）。
    /// new_pane() 预计算新窗格尺寸时用此值估算终端工作区。
    pub fn effective_width(&self, expanded_width: f32) -> f32 {
        if self.visible {
            expanded_width
        } else {
            0.0
        }
    }

    // M5.3 part3c-2：Option A 的快照抽取 / 远程展开入口（snapshot_rows / snapshot_visit /
    // find_dir_by_path / remote_expand / remote_collapse）已删。Option B 下被控端不再持远程
    // 展开态，控制端自持的远程树状态机（含路径反查 DFS）见 remote_ws::RemoteFileTree（片2）。

    /// 树根跟随激活会话 cwd：变化（切 tab / cd / 首次上报）时整树重置
    /// ——节点表、目录缓存、展开与选中状态全部重建（规格：根变化时
    /// 重置展开状态）；搜索态一并退出（旧根的结果已无意义）。
    fn sync_root(&mut self, cwd: Option<&Path>) {
        if self.root.as_deref() == cwd {
            return;
        }
        self.root = cwd.map(Path::to_path_buf);
        self.reset_nodes();
        self.close_search();
    }

    /// 重建节点表（「刷新」按钮也走这里，代价是展开状态一并丢失——
    /// 换取 id 分配的简单与确定性）。代次号 +1：在途后台读取的回包
    /// 全部作废，防旧根/旧树的结果污染新节点表。
    fn reset_nodes(&mut self) {
        self.nodes.clear();
        self.listings.clear();
        self.pending.clear();
        self.restore_open.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.tree = TreeViewState::default();
        if let Some(root) = &self.root {
            // 根节点固定占用 id 0（add_node 以 0 起步）。
            self.nodes.push(NodeInfo {
                path: root.clone(),
                kind: NodeKind::Dir,
            });
        }
    }

    /// 收取后台读目录的回包：当前代次且请求号最新的把「加载中…」
    /// 占位替换为真实子项，旧代次/旧请求（换根、刷新、目录级刷新前
    /// 派发）的直接丢弃。
    fn drain_replies(&mut self) {
        while let Ok(reply) = self.reply_rx.try_recv() {
            if reply.epoch != self.epoch || self.pending.get(&reply.dir_id) != Some(&reply.seq) {
                continue;
            }
            self.pending.remove(&reply.dir_id);
            let dir_path = self.nodes[reply.dir_id].path.clone();
            let children = match reply.result {
                Err(()) => vec![push_node(&mut self.nodes, dir_path, NodeKind::Unreadable)],
                Ok((entries, overflow)) => {
                    let mut children = Vec::with_capacity(entries.len() + 1);
                    for (path, is_dir) in entries {
                        // 目录级刷新前展开过的子目录：新节点 id 变了，
                        // 按路径把展开状态接回去（restore_open 一次性
                        // 消费，命中即移除）。
                        let restore = is_dir && self.restore_open.remove(&path);
                        let kind = if is_dir {
                            NodeKind::Dir
                        } else {
                            NodeKind::File
                        };
                        let id = push_node(&mut self.nodes, path, kind);
                        if restore {
                            self.tree.set_openness(id, true);
                        }
                        children.push(id);
                    }
                    if overflow > 0 {
                        children.push(push_node(
                            &mut self.nodes,
                            dir_path,
                            NodeKind::Overflow(overflow),
                        ));
                    }
                    children
                }
            };
            // 直接覆盖占位 listing（占位节点留在 append-only 节点表里，
            // 不再被引用，下次重建时一并回收）。
            self.listings.insert(reply.dir_id, DirListing { children });
        }
    }

    /// 收取后台文件操作（新建/删除）的回包：成功 → toast + 刷新所在
    /// 目录；失败 → toast 错误。
    fn drain_ops(&mut self, out: &mut FileTreeOutput) {
        while let Ok(reply) = self.op_rx.try_recv() {
            self.ops_pending = self.ops_pending.saturating_sub(1);
            // 成功时刷新目录（失败不刷新）。
            let is_success = matches!(reply.result, OpResult::Created(_) | OpResult::Trashed(_));
            if is_success {
                if let Some(dir) = &reply.refresh {
                    self.refresh_dir(dir);
                }
            }
            // F6：UI 侧取当前语言文案。
            out.toasts.push(reply.result.to_toast());
        }
    }

    /// 收取后台搜索扫描的回包（仅当前代次的被接受）。
    fn drain_search(&mut self, out: &mut FileTreeOutput) {
        while let Ok(reply) = self.search_rx.try_recv() {
            if reply.epoch != self.search_epoch.load(Ordering::Relaxed) {
                continue;
            }
            self.search_pending = false;
            if reply.truncated {
                out.toasts.push((
                    ToastKind::Warn,
                    crate::i18n::strings()
                        .filetree_search_truncated_toast
                        .to_owned(),
                ));
            }
            self.search_results = Some(SearchResults {
                items: reply.items,
                truncated: reply.truncated,
            });
            self.search_selected = None;
        }
    }

    /// 目录级刷新（新建/删除后调用）：只作废该目录（按路径，含因
    /// 重建产生的孤儿同路径节点）的子项缓存与在途请求，下一帧
    /// ensure_listing 重新派发读取；后代目录的展开状态先按路径收集
    /// 进 [`Self::restore_open`]，回包建新节点时恢复——比整树
    /// reset_nodes 轻得多（不丢其余分支的展开/选中状态）。
    fn refresh_dir(&mut self, dir: &Path) {
        let ids: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Dir) && n.path == dir)
            .map(|(i, _)| i)
            .collect();
        let mut open = Vec::new();
        for &id in &ids {
            self.collect_open_dirs(id, &mut open);
        }
        self.restore_open.extend(open);
        for id in ids {
            self.listings.remove(&id);
            self.pending.remove(&id);
        }
    }

    /// 递归收集 `dir_id` 子树中被用户展开过的目录路径（父目录当前
    /// 收起也收集——ltreeview 保留其展开记忆，刷新后应一致）。
    fn collect_open_dirs(&self, dir_id: usize, acc: &mut Vec<PathBuf>) {
        let Some(listing) = self.listings.get(&dir_id) else {
            return;
        };
        for &child in &listing.children {
            let Some(info) = self.nodes.get(child) else {
                continue;
            };
            if matches!(info.kind, NodeKind::Dir) {
                if self.tree.is_open(&child) == Some(true) {
                    acc.push(info.path.clone());
                }
                self.collect_open_dirs(child, acc);
            }
        }
    }

    /// 派发后台新建（文件/文件夹）。结果回包经 [`Self::drain_ops`]
    /// 弹 toast；盘 IO 不在 UI 线程做（网络盘可能卡住）。
    fn dispatch_create(&mut self, dir: PathBuf, name: String, is_dir: bool) {
        self.ops_pending += 1;
        let tx = self.op_tx.clone();
        std::thread::spawn(move || {
            let target = dir.join(&name);
            let result = if is_dir {
                std::fs::create_dir(&target)
            } else {
                // create_new：已存在同名项时失败而非清空旧文件。
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map(|_| ())
            };
            let reply = match result {
                Ok(()) => OpReply {
                    refresh: Some(dir),
                    result: OpResult::Created(name),
                },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => OpReply {
                    refresh: None,
                    result: OpResult::CreateExists(name),
                },
                Err(e) => OpReply {
                    refresh: None,
                    result: OpResult::CreateFailed(e.to_string()),
                },
            };
            // UI 先退出时通道已关：发送失败静默忽略。
            let _ = tx.send(reply);
        });
    }

    /// 派发后台删除（移入回收站，trash crate）。Windows 下走
    /// IFileOperation，可能弹系统进度框/被占用失败，均在后台线程等待。
    fn dispatch_delete(&mut self, path: PathBuf) {
        self.ops_pending += 1;
        let tx = self.op_tx.clone();
        std::thread::spawn(move || {
            let name = display_name(&path);
            let parent = path.parent().map(Path::to_path_buf);
            let reply = match trash::delete(&path) {
                Ok(()) => OpReply {
                    refresh: parent,
                    result: OpResult::Trashed(name),
                },
                Err(e) => OpReply {
                    refresh: None,
                    result: OpResult::DeleteFailed(e.to_string()),
                },
            };
            let _ = tx.send(reply);
        });
    }

    /// 派发后台搜索扫描（防抖到点后调用）。代次 +1：在途旧扫描的
    /// 回包作废，worker 自身也按代次提前退出。
    fn dispatch_search(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let query = self.search_query.trim().to_lowercase();
        if query.chars().count() < SEARCH_MIN_CHARS {
            return;
        }
        let epoch = self.search_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.search_pending = true;
        self.search_results = None;
        let tx = self.search_tx.clone();
        let current = self.search_epoch.clone();
        std::thread::spawn(move || {
            let (items, truncated) = search_worker(&root, &query, epoch, &current);
            let _ = tx.send(SearchReply {
                epoch,
                items,
                truncated,
            });
        });
    }

    /// 收起搜索：清空输入与结果、恢复树视图，作废在途扫描。
    fn close_search(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_focus = false;
        self.search_dispatch_at = None;
        self.search_results = None;
        self.search_pending = false;
        self.search_selected = None;
        // 代次 +1：在途 worker 检测到后提前退出，回包按号丢弃。
        self.search_epoch.fetch_add(1, Ordering::Relaxed);
    }
}

/// 一帧文件树 UI 的产出（由 main.rs 执行，UI 只产出动作）。
#[derive(Default)]
pub struct FileTreeOutput {
    /// 激活了目录且 shell 空闲：请求向激活会话注入 cd。
    pub cd_dir: Option<PathBuf>,
    /// 激活了文件：用系统默认程序打开。
    pub open_file: Option<PathBuf>,
    /// 激活了目录但 shell 忙，cd 未注入（上层据此弹 toast）。
    pub busy_hint: bool,
    /// 节点被拖出树并在树外释放：(路径, 释放位置（egui 逻辑坐标）)。
    /// 是否落在终端区由 shell/mod.rs 在 CentralPanel 布局后判定。
    pub external_drop: Option<(PathBuf, egui::Pos2)>,
    /// 请求写剪贴板的文本（复制绝对/相对路径；arboard 在 main 持有）。
    pub copy_text: Option<String>,
    /// part3c-2 #7/上传：复制文件 / 文件夹到文件剪贴板（本地侧；(路径, 是否目录)）。
    pub file_copy: Option<(PathBuf, bool)>,
    /// part3c-2 #7：把远程剪贴板项粘贴（下载）到此本地目录。
    pub file_paste_dir: Option<PathBuf>,
    /// 要弹的系统提示（操作结果反馈；shell::show 转投 ToastState）。
    pub toasts: Vec<(ToastKind, String)>,
    /// 对话框本帧关闭（main 把键盘焦点交还终端）。
    pub dialog_closed: bool,
    /// 展开面板本帧的实际宽度（逻辑点；P10 持久化用）。收起窄条时
    /// 为 None——不覆盖已存的展开宽度。
    pub panel_width: Option<f32>,
    /// 面板本帧实际占用矩形（含展开态与窄条；P16 描边用）。None
    /// 仅在极端边角（面板未渲染）时出现，正常帧必有值。
    pub panel_rect: Option<egui::Rect>,
}

/// 绘制文件树栏（位于 tab 侧栏右侧、终端区左侧）。
/// 收起时画一条窄条（仅展开按钮），展开时画完整面板（可拖宽，
/// P10：`width` 为持久化的宽度，仅 egui 无面板记忆的首帧生效；
/// 实际宽度经 [`FileTreeOutput::panel_width`] 报回，松手时落盘）。
pub fn show(
    root: &mut egui::Ui,
    st: &mut FileTreeState,
    cwd: Option<&Path>,
    shell_idle: bool,
    pal: &theme::Palette,
    width: f32,
    // part3c-2 #7：是否显示「粘贴到此目录」（= 文件剪贴板里有远程项，下载目标）。
    can_paste: bool,
) -> FileTreeOutput {
    let mut out = FileTreeOutput::default();
    st.sync_root(cwd);
    // 先收各路后台回包（面板收起时也收：重新展开即见结果；操作结果
    // toast 不依赖面板可见性）。
    st.drain_replies();
    st.drain_ops(&mut out);
    st.drain_search(&mut out);
    // 搜索防抖到点：派发后台扫描。
    if st.search_dispatch_at.is_some_and(|at| Instant::now() >= at) {
        st.search_dispatch_at = None;
        st.dispatch_search();
    }

    // 问题7：收起态不再画窄条（开合由顶栏②与 Ctrl+B 管）。
    // 展开态照常画完整面板（可拖宽）；隐藏态不画任何面板，panel_rect
    // 保持 None——mod.rs 的描边与拖宽手柄逻辑对 None 已做跳过处理。
    if st.visible {
        let resp = egui::Panel::left("lumen_filetree")
            .default_size(width)
            .size_range(crate::settings::FILETREE_WIDTH_MIN..=crate::settings::FILETREE_WIDTH_MAX)
            .resizable(true)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.filetree_fill)
                    .inner_margin(egui::Margin::symmetric(6, 8)),
            )
            .show_inside(root, |ui| panel_ui(ui, st, shell_idle, pal, can_paste, &mut out));
        out.panel_width = Some(resp.response.rect.width());
        out.panel_rect = Some(resp.response.rect);
    }
    // else：st.visible == false → 不画任何面板，panel_rect = None，
    // panel_width = None（mod.rs 已处理 None 跳过描边/手柄逻辑）。

    // 对话框（新建输入/删除确认）：模态层，面板收起时也照常显示。
    dialog_ui(root.ctx(), st, pal, &mut out);

    // 仍有在途后台工作（含本帧 panel_ui 刚派发的）：安排轮询重绘，
    // 驱动下一帧继续收包——必须放在面板绘制之后，否则首个派发帧
    // 不会被唤醒，「加载中…」会卡到下一个无关事件才刷新。
    if !st.pending.is_empty() || st.ops_pending > 0 || st.search_pending {
        root.ctx().request_repaint_after(LOAD_POLL_INTERVAL);
    }
    // 搜索防抖在途：到点那一帧需要被唤醒来派发扫描。
    if let Some(at) = st.search_dispatch_at {
        root.ctx()
            .request_repaint_after(at.saturating_duration_since(Instant::now()));
    }
    out
}

/// 面板内容：工具条 + 搜索行 + 轻提示 + 树/搜索结果。
fn panel_ui(
    ui: &mut egui::Ui,
    st: &mut FileTreeState,
    shell_idle: bool,
    pal: &theme::Palette,
    can_paste: bool,
    out: &mut FileTreeOutput,
) {
    // —— 工具条：根目录名（悬停看全路径）+ 搜索/刷新 ——
    // 问题7：收起按钮已删除（开合由顶栏②与 Ctrl+B 统一管理）。
    let s = crate::i18n::strings();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // —— 刷新按钮：24×24 热区，painter 画圆弧箭头 ——
            let (refresh_rect, refresh_resp) =
                ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
            {
                let painter = ui.painter();
                if refresh_resp.hovered() {
                    painter.rect_filled(
                        refresh_rect,
                        egui::CornerRadius::same(4),
                        pal.bg_highlight,
                    );
                }
                let fg = if refresh_resp.hovered() {
                    pal.fg
                } else {
                    pal.fg_dim
                };
                let stroke = egui::Stroke::new(1.2, fg);
                let c = refresh_rect.center();
                // 圆弧箭头：用 11 段折线逼近约 270° 圆弧（从 45° 到 315°，顺时针）
                // 半径 5.0，圆心在热区中心偏上 0.5px 以视觉居中
                let r = 5.0_f32;
                let arc_cx = c.x;
                let arc_cy = c.y + 0.5;
                // 起点角度 45°（右上），终点角度 315°（右下），顺时针即角度递减
                // 逼近段数 11，覆盖约 270°
                let start_deg = 45.0_f32;
                let sweep_deg = -270.0_f32; // 顺时针 270°
                let n = 11usize;
                let mut arc_pts: Vec<egui::Pos2> = (0..=n)
                    .map(|i| {
                        let t = i as f32 / n as f32;
                        let angle = (start_deg + t * sweep_deg).to_radians();
                        egui::pos2(
                            arc_cx + r * angle.cos(),
                            arc_cy - r * angle.sin(), // egui y 轴向下，sin 取反
                        )
                    })
                    .collect();
                // 绘制折线段
                for w in arc_pts.windows(2) {
                    painter.line_segment([w[0], w[1]], stroke);
                }
                // 末端箭头（位于弧终点约 315°，朝向切线方向）
                // 切线方向：顺时针圆弧末端切线沿 225° 方向（向左下），箭头两短线
                let end_pt = arc_pts.pop().unwrap_or(c);
                let arrow_len = 3.5_f32;
                // 末端切线角度（顺时针弧末端切线 = 起始角 + 顺时针 270° - 90°）
                // 实际切线朝 (45° - 270° - 90°) = -315° ≡ 45° 向下即约 -45° + 180° = 135°
                // 更直观：末端点约在 315°（右下），顺时针切线方向向左 = 约 180°+45°=225°
                let tangent_deg: f32 = 225.0;
                let tangent_rad = tangent_deg.to_radians();
                let perp1 = (tangent_deg + 40.0).to_radians();
                let perp2 = (tangent_deg - 40.0).to_radians();
                painter.line_segment(
                    [
                        end_pt,
                        egui::pos2(
                            end_pt.x
                                + arrow_len * tangent_rad.cos()
                                + arrow_len * 0.6 * perp1.cos(),
                            end_pt.y
                                - arrow_len * tangent_rad.sin()
                                - arrow_len * 0.6 * perp1.sin(),
                        ),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        end_pt,
                        egui::pos2(
                            end_pt.x
                                + arrow_len * tangent_rad.cos()
                                + arrow_len * 0.6 * perp2.cos(),
                            end_pt.y
                                - arrow_len * tangent_rad.sin()
                                - arrow_len * 0.6 * perp2.sin(),
                        ),
                    ],
                    stroke,
                );
            }
            if refresh_resp.on_hover_text(s.filetree_refresh_tip).clicked() {
                st.reset_nodes();
            }

            // —— 搜索开关按钮：24×24 热区，painter 画放大镜 ——
            // 激活态用 accent 色；点击展开输入行，再点收起并清空（Esc 同义）。
            let (search_rect, search_resp) =
                ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
            {
                let painter = ui.painter();
                let is_active = st.search_open;
                if search_resp.hovered() {
                    painter.rect_filled(search_rect, egui::CornerRadius::same(4), pal.bg_highlight);
                }
                let fg = if is_active {
                    pal.accent
                } else if search_resp.hovered() {
                    pal.fg
                } else {
                    pal.fg_dim
                };
                let stroke = egui::Stroke::new(1.2, fg);
                let c = search_rect.center();
                // 放大镜圆（圆心偏左上，半径 4.5）
                let lens_r = 4.5_f32;
                let lens_cx = c.x - 1.5;
                let lens_cy = c.y - 1.5;
                // 用 12 段折线逼近整圆
                let n = 12usize;
                let circle_pts: Vec<egui::Pos2> = (0..=n)
                    .map(|i| {
                        let angle = (i as f32 / n as f32) * std::f32::consts::TAU;
                        egui::pos2(
                            lens_cx + lens_r * angle.cos(),
                            lens_cy + lens_r * angle.sin(),
                        )
                    })
                    .collect();
                for w in circle_pts.windows(2) {
                    painter.line_segment([w[0], w[1]], stroke);
                }
                // 柄：从圆边 45° 方向延伸约 4px
                let handle_start = egui::pos2(
                    lens_cx + lens_r * 45.0_f32.to_radians().cos(),
                    lens_cy + lens_r * 45.0_f32.to_radians().sin(),
                );
                let handle_end = egui::pos2(
                    handle_start.x + 4.0 * 45.0_f32.to_radians().cos(),
                    handle_start.y + 4.0 * 45.0_f32.to_radians().sin(),
                );
                painter.line_segment([handle_start, handle_end], stroke);
            }
            if search_resp.on_hover_text(s.filetree_search_tip).clicked() {
                if st.search_open {
                    st.close_search();
                } else {
                    st.search_open = true;
                    st.search_focus = true;
                }
            }
            let title = st
                .root
                .as_deref()
                .map_or_else(|| s.filetree_root_placeholder.to_owned(), display_name);
            let label =
                egui::Label::new(egui::RichText::new(title).size(12.0).color(pal.fg)).truncate();
            let resp = ui.add(label);
            if let Some(root) = &st.root {
                resp.on_hover_text(root.display().to_string());
            }
        });
    });

    // —— 搜索输入行（🔍 展开时显示）——
    if st.search_open {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut st.search_query)
                .hint_text(s.filetree_search_hint)
                .desired_width(f32::INFINITY),
        );
        if st.search_focus {
            resp.request_focus();
            st.search_focus = false;
        }
        // Esc：egui 让输入框失焦，借此收起并清空搜索。
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            st.close_search();
        } else if resp.changed() {
            if st.search_query.trim().chars().count() >= SEARCH_MIN_CHARS {
                // 防抖：停止输入 SEARCH_DEBOUNCE 后才派发扫描。
                st.search_dispatch_at = Some(Instant::now() + SEARCH_DEBOUNCE);
            } else {
                // 不足触发字数：退回树视图，作废在途扫描。
                st.search_dispatch_at = None;
                st.search_results = None;
                st.search_pending = false;
                st.search_selected = None;
                st.search_epoch.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // —— shell 忙提示（双击目录但 shell 非空闲时短暂显示）——
    if let Some(until) = st.hint_until {
        let now = Instant::now();
        if now < until {
            ui.label(
                egui::RichText::new(s.filetree_shell_busy)
                    .size(10.0)
                    .color(pal.fg_dim),
            );
            // 到点重画一帧清掉提示。
            ui.ctx().request_repaint_after(until - now);
        } else {
            st.hint_until = None;
        }
    }

    if st.root.is_none() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(s.filetree_waiting_cwd)
                .size(11.0)
                .color(pal.fg_dim),
        );
        return;
    }

    ui.add_space(2.0);
    // 搜索态（输入达到触发字数）：扁平结果列表替代树视图。
    let searching = st.search_open && st.search_query.trim().chars().count() >= SEARCH_MIN_CHARS;
    if searching {
        search_results_ui(ui, st, shell_idle, pal, out);
    } else {
        tree_ui(ui, st, shell_idle, pal, can_paste, out);
    }
}

/// 树视图（ScrollArea + ltreeview + 右键菜单/拖拽动作处理）。
fn tree_ui(
    ui: &mut egui::Ui,
    st: &mut FileTreeState,
    shell_idle: bool,
    pal: &theme::Palette,
    can_paste: bool,
    out: &mut FileTreeOutput,
) {
    // 右键菜单点选的动作：菜单闭包（在 ltreeview 内部被调用）只持
    // RefCell 共享引用，树绘制结束后统一处理。
    let menu: RefCell<Option<MenuAction>> = RefCell::new(None);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let FileTreeState {
                tree,
                nodes,
                listings,
                pending,
                load_seq,
                epoch,
                reply_tx,
                hint_until,
                ..
            } = st;
            let mut load = LoadCtx {
                nodes,
                listings,
                pending,
                load_seq,
                epoch: *epoch,
                tx: reply_tx,
                can_paste,
            };
            let (resp, actions) = TreeView::new(ui.make_persistent_id("lumen_file_tree"))
                .allow_multi_selection(false)
                // 拖出树外释放 → MoveExternal（拖到终端区插入路径）。
                // 节点本身 drop_allowed(false)：树内不支持移动文件，
                // 不会出现树内落点指示线与 Move 动作。
                .allow_drag_and_drop(true)
                .show_state(ui, tree, |builder| {
                    add_node(builder, &mut load, 0, pal, &menu);
                });
            // 本帧真实产生的 Activate 目标（回车键路径；鼠标双击因上游
            // bug 走不到这里，见 merge_double_click_activation）。
            let mut activated: Vec<usize> = Vec::new();
            // 本帧的点选动作（双击合成激活的依据）。
            let mut selected_now: Option<Vec<usize>> = None;
            for action in actions {
                match action {
                    Action::Activate(act) => activated.extend(act.selected),
                    Action::SetSelected(sel) => selected_now = Some(sel),
                    // 拖出树外释放：把 (路径, 释放点) 交给 shell 层
                    // 判定是否落在终端区（占位行不参与拖放）。
                    Action::MoveExternal(ext) => {
                        let Some(&id) = ext.source.first() else {
                            continue;
                        };
                        let Some(info) = load.nodes.get(id) else {
                            continue;
                        };
                        if matches!(info.kind, NodeKind::Dir | NodeKind::File) {
                            out.external_drop = Some((info.path.clone(), ext.position));
                        }
                    }
                    // 树内移动不支持（drop_allowed=false 下不应出现，
                    // 防御忽略）；拖动过程无需处理。
                    Action::Move(_) | Action::Drag(_) | Action::DragExternal(_) => {}
                }
            }
            // 激活语义（双击/回车）：目录 → cd（shell 空闲才发），
            // 文件 → 系统默认程序打开。单选模式下至多一个节点。
            for id in merge_double_click_activation(activated, resp.double_clicked(), selected_now)
            {
                let Some(info) = load.nodes.get(id) else {
                    continue;
                };
                match info.kind {
                    NodeKind::Dir => activate_dir(info.path.clone(), shell_idle, hint_until, out),
                    NodeKind::File => out.open_file = Some(info.path.clone()),
                    NodeKind::Overflow(_) | NodeKind::Unreadable | NodeKind::Loading => {}
                }
            }
        });
    // 树绘制结束（FileTreeState 的拆借已归还），处理右键菜单动作。
    if let Some(action) = menu.into_inner() {
        handle_menu_action(st, action, shell_idle, out);
    }
}

/// 鼠标双击激活的合成（egui_ltreeview 0.7.0 上游 bug 规避）。
///
/// 上游 bug：`do_input_output` 的 Click 分支先把单槽 output 设为
/// `SetLastclicked`，随后普通单击路径无条件覆盖为 `SelectOneNode`，
/// `last_clicked_node` 永远不被记录 → 双击判定 `was_clicked_last`
/// 恒 false，鼠标双击的 `Action::Activate` 不可达（回车键走独立的
/// `CollectActivatableNodes` 路径不受影响）。P8 的「双击文件不打开」
/// 即源于此，且自 M3.3 引入 0.7.0 起就存在，与 M3.6 拖拽改造无关。
///
/// 规避：本帧已有真实 Activate（回车 / 上游修复后的双击）直接采用；
/// 否则整树 Response 判定为双击且本帧有点选动作（双击第二击必产生
/// `SetSelected`，egui 双击要求两击同点 6px 内，点中的必是同一行）
/// 时，把被点选的节点视为激活目标。点 closer 三角/空白处不产生
/// `SetSelected`，不会误激活。上游修复后真实 Activate 优先，合成
/// 路径自动让位（不会双重激活）——届时下方「上游 bug 复现」单测会
/// 失败提醒移除本函数。
fn merge_double_click_activation(
    activated: Vec<usize>,
    double_clicked: bool,
    selected_now: Option<Vec<usize>>,
) -> Vec<usize> {
    if !activated.is_empty() || !double_clicked {
        return activated;
    }
    selected_now.unwrap_or_default()
}

/// 处理右键菜单点选的动作。
fn handle_menu_action(
    st: &mut FileTreeState,
    action: MenuAction,
    shell_idle: bool,
    out: &mut FileTreeOutput,
) {
    match action {
        MenuAction::EnterDir(path) => {
            // 与双击目录完全同一条链路：控制字符拒绝 + 忙闸门 + 提示。
            activate_dir(path, shell_idle, &mut st.hint_until, out);
        }
        MenuAction::Create { dir, is_dir } => {
            st.dialog = Some(Dialog::Create {
                dir,
                is_dir,
                name: String::new(),
                focus: true,
                error: None,
            });
        }
        MenuAction::Delete { path, is_dir } => {
            st.dialog = Some(Dialog::ConfirmDelete { path, is_dir });
        }
        MenuAction::Reveal(path) => {
            if let Err(e) = reveal_in_explorer(&path) {
                out.toasts.push((
                    ToastKind::Error,
                    crate::i18n::fmt1(crate::i18n::strings().filetree_reveal_failed_fmt, &e),
                ));
            }
        }
        MenuAction::CopyAbs(path) => {
            out.copy_text = Some(path.display().to_string());
        }
        MenuAction::CopyRel(path) => {
            // 相对树根；不在根下（防御，正常不可能）退回绝对路径。
            let rel = st
                .root
                .as_deref()
                .and_then(|r| path.strip_prefix(r).ok())
                .map_or_else(|| path.display().to_string(), |p| p.display().to_string());
            out.copy_text = Some(rel);
        }
        MenuAction::CopyFiles { path, is_dir } => {
            out.file_copy = Some((path, is_dir));
        }
        MenuAction::PasteInto(dir) => {
            out.file_paste_dir = Some(dir);
        }
    }
}

/// 激活目录的统一语义：路径含控制字符拒绝；shell 空闲请求注入 cd，
/// 忙则树内轻提示 + 上层 toast（树视图与搜索结果共用）。
fn activate_dir(
    path: PathBuf,
    shell_idle: bool,
    hint_until: &mut Option<Instant>,
    out: &mut FileTreeOutput,
) {
    if has_control_chars(&path) {
        // 路径含控制字符（换行/回车等）：写入 PTY 会被行编辑器提前
        // 断行逃出单引号字符串，直接拒绝注入（cd_command 内有同款
        // 兜底）。
        log::warn!("目录名含控制字符，拒绝注入 cd: {}", path.display());
    } else if shell_idle {
        out.cd_dir = Some(path);
    } else {
        // shell 忙：不注入命令，仅提示（上层另弹 toast，见 shell/mod.rs）。
        *hint_until = Some(Instant::now() + HINT_DURATION);
        out.busy_hint = true;
    }
}

/// 搜索结果列表（扁平相对路径；单击定位、双击按节点语义）。
fn search_results_ui(
    ui: &mut egui::Ui,
    st: &mut FileTreeState,
    shell_idle: bool,
    pal: &theme::Palette,
    out: &mut FileTreeOutput,
) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let placeholder = |ui: &mut egui::Ui, text: &str| {
                ui.label(
                    egui::RichText::new(text)
                        .size(11.0)
                        .color(pal.fg_dim)
                        .italics(),
                );
            };
            let s = crate::i18n::strings();
            let Some(res) = &st.search_results else {
                placeholder(ui, s.filetree_searching);
                return;
            };
            if res.items.is_empty() {
                placeholder(ui, s.filetree_no_results);
                return;
            }
            // 先收集动作再应用，绕开 search_results 与 selected 的
            // 同时可变借用。
            let mut clicked = None;
            let mut activated: Option<(PathBuf, bool)> = None;
            for (i, (path, is_dir)) in res.items.iter().enumerate() {
                let mut text = st
                    .root
                    .as_deref()
                    .and_then(|r| path.strip_prefix(r).ok())
                    .map_or_else(|| path.display().to_string(), |p| p.display().to_string());
                if *is_dir {
                    // 目录加尾分隔符标识（扁平列表里没有 closer 三角）。
                    text.push(std::path::MAIN_SEPARATOR);
                }
                let selected = st.search_selected == Some(i);
                let resp = ui
                    .selectable_label(selected, egui::RichText::new(text).size(11.5).color(pal.fg));
                if resp.double_clicked() {
                    activated = Some((path.clone(), *is_dir));
                } else if resp.clicked() {
                    clicked = Some(i);
                }
            }
            if res.truncated {
                placeholder(ui, s.filetree_truncated);
            }
            if let Some(i) = clicked {
                st.search_selected = Some(i);
            }
            if let Some((path, is_dir)) = activated {
                if is_dir {
                    activate_dir(path, shell_idle, &mut st.hint_until, out);
                } else {
                    out.open_file = Some(path);
                }
            }
        });
}

/// 对话框（新建文件/文件夹的名字输入、删除确认）。模态：打开期间
/// main 把键盘焦点交给 egui（[`FileTreeState::dialog_open`]）。
fn dialog_ui(
    ctx: &egui::Context,
    st: &mut FileTreeState,
    pal: &theme::Palette,
    out: &mut FileTreeOutput,
) {
    // 取出对话框规避借用冲突（确认时要调 st.dispatch_*）；
    // 未关闭则在末尾放回。
    let Some(mut dialog) = st.dialog.take() else {
        return;
    };
    let mut close = false;
    let frame = egui::Frame::new()
        .fill(pal.bg_panel)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(16));
    match &mut dialog {
        Dialog::Create {
            dir,
            is_dir,
            name,
            focus,
            error,
        } => {
            let s = crate::i18n::strings();
            let title = if *is_dir {
                s.filetree_create_dir_title
            } else {
                s.filetree_create_file_title
            };
            let mut confirmed = false;
            let modal = egui::Modal::new(egui::Id::new("lumen_filetree_create"))
                .backdrop_color(egui::Color32::from_black_alpha(120))
                .frame(frame)
                .show(ctx, |ui| {
                    ui.set_width(280.0);
                    ui.label(egui::RichText::new(title).size(14.0).strong().color(pal.fg));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(crate::i18n::fmt1(
                                s.filetree_create_location_fmt,
                                dir.display(),
                            ))
                            .size(10.5)
                            .color(pal.fg_dim),
                        )
                        .truncate(),
                    );
                    ui.add_space(8.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(s.filetree_create_name_hint)
                            .desired_width(f32::INFINITY),
                    );
                    if *focus {
                        edit.request_focus();
                        *focus = false;
                    }
                    // 输入框内按 Enter 等同点「创建」。
                    let submitted =
                        edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if let Some(err) = error {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(err.as_str())
                                .size(11.0)
                                .color(pal.error),
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        // 主操作按钮：accent 实底 + 反相文字（M3.7b 黑白 CTA）。
                        let create_btn = egui::Button::new(
                            egui::RichText::new(s.filetree_create_btn).color(pal.accent_fg),
                        )
                        .fill(pal.accent);
                        if ui.add(create_btn).clicked() || submitted {
                            confirmed = true;
                        }
                        if ui.button(s.filetree_cancel_btn).clicked() {
                            close = true;
                        }
                    });
                });
            if confirmed {
                let trimmed = name.trim().to_owned();
                match validate_entry_name(&trimmed) {
                    Err(msg) => {
                        // 校验失败：对话框留着改名字，焦点还给输入框。
                        *error = Some(msg.to_owned());
                        *focus = true;
                    }
                    Ok(()) => {
                        st.dispatch_create(dir.clone(), trimmed, *is_dir);
                        close = true;
                    }
                }
            }
            if modal.should_close() {
                close = true;
            }
        }
        Dialog::ConfirmDelete { path, is_dir } => {
            let s = crate::i18n::strings();
            let name = display_name(path);
            let mut confirmed = false;
            let modal = egui::Modal::new(egui::Id::new("lumen_filetree_delete"))
                .backdrop_color(egui::Color32::from_black_alpha(120))
                .frame(frame)
                .show(ctx, |ui| {
                    ui.set_width(280.0);
                    ui.label(
                        egui::RichText::new(s.filetree_delete_title)
                            .size(14.0)
                            .strong()
                            .color(pal.fg),
                    );
                    ui.add_space(8.0);
                    let what = if *is_dir {
                        s.filetree_delete_what_dir
                    } else {
                        s.filetree_delete_what_file
                    };
                    ui.label(
                        egui::RichText::new(crate::i18n::fmt2(
                            s.filetree_delete_confirm_fmt,
                            what,
                            &name,
                        ))
                        .size(12.0)
                        .color(pal.fg),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        // 危险操作按钮：语义红实底（保留彩色）+ 反相文字
                        // （accent_fg 对深/浅两套红底均 ≥4.5:1，M3.7b）。
                        let del_btn = egui::Button::new(
                            egui::RichText::new(s.filetree_delete_trash_btn).color(pal.accent_fg),
                        )
                        .fill(pal.error);
                        if ui.add(del_btn).clicked() {
                            confirmed = true;
                        }
                        if ui.button(s.filetree_cancel_btn).clicked() {
                            close = true;
                        }
                    });
                });
            if confirmed {
                st.dispatch_delete(path.clone());
                close = true;
            }
            if modal.should_close() {
                close = true;
            }
        }
    }
    if close {
        out.dialog_closed = true;
    } else {
        st.dialog = Some(dialog);
    }
}

/// 懒加载上下文：从 [`FileTreeState`] 拆借出的字段（绕开整体借用，
/// `add_node` 递归与激活处理共用）。
struct LoadCtx<'a> {
    nodes: &'a mut Vec<NodeInfo>,
    listings: &'a mut HashMap<usize, DirListing>,
    pending: &'a mut HashMap<usize, u64>,
    load_seq: &'a mut u64,
    epoch: u64,
    tx: &'a mpsc::Sender<LoadReply>,
    /// part3c-2 #7：是否在目录右键菜单显示「粘贴到此目录」（有远程剪贴板时）。
    can_paste: bool,
}

/// 递归添加一个节点（目录展开时先懒加载子项再下钻）。
fn add_node(
    builder: &mut TreeViewBuilder<'_, usize>,
    load: &mut LoadCtx<'_>,
    id: usize,
    pal: &theme::Palette,
    menu: &RefCell<Option<MenuAction>>,
) {
    let kind = load.nodes[id].kind;
    match kind {
        NodeKind::File => {
            let path = load.nodes[id].path.clone();
            let name = display_name(&path);
            // R8.2：用 label_ui + truncate() 确保长文件名带省略号截断，
            // 避免宽度超出面板导致横向溢出（egui_ltreeview 默认 label() 不截断）。
            builder.node(
                NodeBuilder::leaf(id)
                    .label_ui(move |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&name))
                                .selectable(false)
                                .truncate(),
                        );
                    })
                    .context_menu(move |ui| file_context_menu(ui, &path, menu)),
            );
        }
        NodeKind::Overflow(n) => {
            let s = crate::i18n::strings();
            builder.node(
                NodeBuilder::leaf(id).activatable(false).label(
                    egui::RichText::new(crate::i18n::fmt1(s.filetree_overflow_fmt, n))
                        .size(11.0)
                        .color(pal.fg_dim)
                        .italics(),
                ),
            );
        }
        NodeKind::Unreadable => {
            builder.node(
                NodeBuilder::leaf(id).activatable(false).label(
                    egui::RichText::new(crate::i18n::strings().filetree_unreadable)
                        .size(11.0)
                        .color(pal.fg_dim)
                        .italics(),
                ),
            );
        }
        NodeKind::Loading => {
            builder.node(
                NodeBuilder::leaf(id).activatable(false).label(
                    egui::RichText::new(crate::i18n::strings().filetree_loading)
                        .size(11.0)
                        .color(pal.fg_dim)
                        .italics(),
                ),
            );
        }
        NodeKind::Dir => {
            let path = load.nodes[id].path.clone();
            let name = display_name(&path);
            let is_root = id == 0;
            let can_paste = load.can_paste;
            // activatable：双击/回车在目录上触发 cd（ltreeview 随之禁用
            // 双击开合，展开/折叠走左侧 closer 三角，与 Warp 一致）。
            // 根目录默认展开，其余默认收起（懒加载的前提）。
            // drop_allowed(false)：树内不做文件移动，拖动只用于拖出
            // 树外（终端区插入路径）。
            let open = builder.node(
                NodeBuilder::dir(id)
                    .activatable(true)
                    .default_open(id == 0)
                    .drop_allowed(false)
                    // R8.2：用 label_ui + truncate() 确保目录名带省略号截断。
                    .label_ui(move |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&name))
                                .selectable(false)
                                .truncate(),
                        );
                    })
                    .context_menu(move |ui| {
                        dir_context_menu(ui, &path, is_root, can_paste, menu);
                    }),
            );
            if open {
                ensure_listing(load, id);
                // children 是 id 列表，克隆一份避免递归中长借用 listings。
                let children = load
                    .listings
                    .get(&id)
                    .map(|l| l.children.clone())
                    .unwrap_or_default();
                for child in children {
                    add_node(builder, load, child, pal, menu);
                }
            }
            builder.close_dir();
        }
    }
}

/// 目录节点的右键菜单（树根不提供删除/相对路径——删除 cwd 自身既
/// 危险也无意义，相对自身恒为空）。
fn dir_context_menu(
    ui: &mut egui::Ui,
    path: &Path,
    is_root: bool,
    can_paste: bool,
    menu: &RefCell<Option<MenuAction>>,
) {
    let s = crate::i18n::strings();
    ui.set_min_width(150.0);
    // 进入文件夹 = 双击目录的菜单等价物（需求池 P7）；树根也提供
    // ——shell 中途 cd 走了之后可借此回到树根目录。
    if ui.button(s.filetree_menu_enter_dir).clicked() {
        *menu.borrow_mut() = Some(MenuAction::EnterDir(path.to_path_buf()));
        ui.close();
    }
    ui.separator();
    // part3c-2 #7/上传：复制本目录到文件剪贴板（上传源）；远程剪贴板有项时可粘贴（下载到此）。
    if ui.button(s.remote_menu_copy).clicked() {
        *menu.borrow_mut() = Some(MenuAction::CopyFiles {
            path: path.to_path_buf(),
            is_dir: true,
        });
        ui.close();
    }
    if can_paste && ui.button(s.remote_menu_paste).clicked() {
        *menu.borrow_mut() = Some(MenuAction::PasteInto(path.to_path_buf()));
        ui.close();
    }
    ui.separator();
    if ui.button(s.filetree_menu_new_file).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Create {
            dir: path.to_path_buf(),
            is_dir: false,
        });
        ui.close();
    }
    if ui.button(s.filetree_menu_new_dir).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Create {
            dir: path.to_path_buf(),
            is_dir: true,
        });
        ui.close();
    }
    ui.separator();
    if ui.button(s.filetree_menu_reveal).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Reveal(path.to_path_buf()));
        ui.close();
    }
    if ui.button(s.filetree_menu_copy_abs).clicked() {
        *menu.borrow_mut() = Some(MenuAction::CopyAbs(path.to_path_buf()));
        ui.close();
    }
    if !is_root {
        if ui.button(s.filetree_menu_copy_rel).clicked() {
            *menu.borrow_mut() = Some(MenuAction::CopyRel(path.to_path_buf()));
            ui.close();
        }
        ui.separator();
        if ui.button(s.filetree_menu_delete).clicked() {
            *menu.borrow_mut() = Some(MenuAction::Delete {
                path: path.to_path_buf(),
                is_dir: true,
            });
            ui.close();
        }
    }
}

/// 文件节点的右键菜单（新建的目标目录 = 文件所在目录）。
fn file_context_menu(ui: &mut egui::Ui, path: &Path, menu: &RefCell<Option<MenuAction>>) {
    let s = crate::i18n::strings();
    ui.set_min_width(150.0);
    // part3c-2 #7/上传：复制本文件到文件剪贴板（上传源）。
    if ui.button(s.remote_menu_copy).clicked() {
        *menu.borrow_mut() = Some(MenuAction::CopyFiles {
            path: path.to_path_buf(),
            is_dir: false,
        });
        ui.close();
    }
    ui.separator();
    // 文件必有父目录（位于树根之下），防御回退自身。
    let parent = path.parent().unwrap_or(path);
    if ui.button(s.filetree_menu_new_file).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Create {
            dir: parent.to_path_buf(),
            is_dir: false,
        });
        ui.close();
    }
    if ui.button(s.filetree_menu_new_dir).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Create {
            dir: parent.to_path_buf(),
            is_dir: true,
        });
        ui.close();
    }
    ui.separator();
    if ui.button(s.filetree_menu_reveal).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Reveal(path.to_path_buf()));
        ui.close();
    }
    if ui.button(s.filetree_menu_copy_abs).clicked() {
        *menu.borrow_mut() = Some(MenuAction::CopyAbs(path.to_path_buf()));
        ui.close();
    }
    if ui.button(s.filetree_menu_copy_rel).clicked() {
        *menu.borrow_mut() = Some(MenuAction::CopyRel(path.to_path_buf()));
        ui.close();
    }
    ui.separator();
    if ui.button(s.filetree_menu_delete).clicked() {
        *menu.borrow_mut() = Some(MenuAction::Delete {
            path: path.to_path_buf(),
            is_dir: false,
        });
        ui.close();
    }
}

/// 懒加载：目录首次展开时先插「加载中…」占位并派发后台读取，之后
/// 命中缓存（占位也算缓存命中，防每帧重复派发；回包到达后由
/// [`FileTreeState::drain_replies`] 替换为真实子项）。
fn ensure_listing(load: &mut LoadCtx<'_>, id: usize) {
    if load.listings.contains_key(&id) {
        return;
    }
    let dir = load.nodes[id].path.clone();
    let placeholder = push_node(load.nodes, dir.clone(), NodeKind::Loading);
    load.listings.insert(
        id,
        DirListing {
            children: vec![placeholder],
        },
    );
    *load.load_seq += 1;
    let seq = *load.load_seq;
    load.pending.insert(id, seq);
    let tx = load.tx.clone();
    let epoch = load.epoch;
    // 后台线程读盘（M3 审查项：UI 线程同步 read_dir 会被慢速网络盘
    // 冻结整个应用）。线程按请求派发、用后即弃：请求频率受「目录
    // 首次展开」天然限速；卡死在断连网络盘上的线程随超时自行了结，
    // 其回包按代次丢弃即可。
    std::thread::spawn(move || {
        let result = read_dir_worker(&dir);
        // UI 先退出时通道已关：发送失败静默忽略。
        let _ = tx.send(LoadReply {
            epoch,
            seq,
            dir_id: id,
            result,
        });
    });
}

/// 本地文件树用：读目录并过滤隐藏项（[`read_dir_worker_ex`] 的 `show_hidden=false` 包装）。
fn read_dir_worker(dir: &Path) -> Result<(Vec<(PathBuf, bool)>, usize), ()> {
    read_dir_worker_ex(dir, false)
}

/// 后台线程的目录读取：`show_hidden=false` 过滤隐藏项 → 目录在前文件在后、各按名排序
/// （不区分大小写；小写键在收集时一次算好，避免比较器里 O(n log n) 次重复分配）→ 截断到
/// 单层上限。枚举本身受 [`ENUM_HARD_CAP`] 封顶，超出时溢出计数是下界。读失败（权限/
/// 网络盘断连）返回 `Err`，由 UI 侧画「无法读取」占位，不 panic。
///
/// `show_hidden`：part3c-2 远程「文件管理」语义下控制端可选显示 `.env`/`.gitignore` 等
/// （本地树恒传 `false` 维持现状）。
pub(crate) fn read_dir_worker_ex(
    dir: &Path,
    show_hidden: bool,
) -> Result<(Vec<(PathBuf, bool)>, usize), ()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            log::warn!("读目录失败 {}: {e}", dir.display());
            return Err(());
        }
    };
    // (小写排序键, 是否目录, 路径)。单个条目元数据读取失败（竞态删除
    // 等）跳过。
    let mut entries: Vec<(String, bool, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        if entries.len() >= ENUM_HARD_CAP {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !show_hidden && is_hidden(&name, &meta) {
            continue;
        }
        entries.push((name.to_lowercase(), is_dir(&meta), entry.path()));
    }
    // 目录在前（true 排前用降序），同类按名不区分大小写排序。
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let overflow = entries.len().saturating_sub(MAX_ENTRIES_PER_DIR);
    entries.truncate(MAX_ENTRIES_PER_DIR);
    Ok((
        entries.into_iter().map(|(_, d, path)| (path, d)).collect(),
        overflow,
    ))
}

/// part3c-2 被控端 ListDir 服务：读目录并转成协议 [`lumen_protocol::remote::DirEntry`]
/// （path 不透明 + 被控端算好 display_name + is_dir），供 `RemoteWs::spawn_list_dir` 后台调。
///
/// TODO(片3，review MED-2)：`path` 经 `Path::display()` 有损转成 `String`，非 UTF-8 路径
/// （Windows 罕见 UTF-16 非法代理 / Unix 任意字节名）回喂 `read_dir` 会失配 → 该子目录不可
/// 展开。本项目被控端为 Windows、几乎全 UTF-8 可表示，故 MVP 接受此限制；若日后支持
/// Unix 被控端或要严谨，把协议 `path` 改为 `Vec<u8>`（OsStr 字节）不透明键，与文件传输一并改。
pub(crate) fn list_dir_entries(
    dir: &Path,
    show_hidden: bool,
) -> Result<(Vec<lumen_protocol::remote::DirEntry>, usize), ()> {
    let (entries, overflow) = read_dir_worker_ex(dir, show_hidden)?;
    let out = entries
        .into_iter()
        .map(|(path, is_dir)| lumen_protocol::remote::DirEntry {
            path: path.display().to_string(),
            name: display_name(&path),
            is_dir,
        })
        .collect();
    Ok((out, overflow))
}

/// 后台线程的搜索扫描：自树根 BFS（浅层结果优先），文件名不分大小写
/// 包含匹配；隐藏项过滤与树一致。三重封顶：深度 [`SEARCH_MAX_DEPTH`]、
/// 结果数 [`SEARCH_MAX_RESULTS`]、访问条目数 [`SEARCH_ENUM_CAP`]，
/// 任一触顶即截断返回。代次落后（用户又改了输入）时提前退出，结果
/// 由 UI 侧按代次丢弃。
fn search_worker(
    root: &Path,
    query_lower: &str,
    epoch: u64,
    current: &AtomicU64,
) -> (Vec<(PathBuf, bool)>, bool) {
    let mut items = Vec::new();
    let mut visited = 0usize;
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if current.load(Ordering::Relaxed) != epoch {
            // 扫描已作废：提前退出（回包会被丢弃，标记截断只为语义完整）。
            return (items, true);
        }
        // 单个子目录读失败（权限等）跳过，不中断整个扫描。
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            visited += 1;
            if visited > SEARCH_ENUM_CAP {
                return (items, true);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if is_hidden(&name, &meta) {
                continue;
            }
            let dir_flag = is_dir(&meta);
            if name.to_lowercase().contains(query_lower) {
                items.push((entry.path(), dir_flag));
                if items.len() >= SEARCH_MAX_RESULTS {
                    return (items, true);
                }
            }
            if dir_flag && depth + 1 < SEARCH_MAX_DEPTH {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    (items, false)
}

/// 追加节点并返回其 id（= 下标）。
fn push_node(nodes: &mut Vec<NodeInfo>, path: PathBuf, kind: NodeKind) -> usize {
    nodes.push(NodeInfo { path, kind });
    nodes.len() - 1
}

/// 隐藏项判定：点开头名字，或 Windows Hidden 文件属性。
fn is_hidden(name: &str, meta: &std::fs::Metadata) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = meta;
    false
}

/// 目录判定：Windows 下直接看 FILE_ATTRIBUTE_DIRECTORY——目录联接/
/// 符号链接也按目录展示（std 的 FileType::is_dir 因 symlink 优先会漏判）。
fn is_dir(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_dir()
    }
}

/// 节点显示名：文件名部分；盘符根（`C:\`）等无文件名时显示整路径。
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// M5.3 part3c-2 控制端远程视图渲染产出（**只读渲染**——交互意图收集到此，main 闭包后
/// 以 `&mut state.remote_ws` 施加，规避 `ShellInput` 的 `&remote_ws` 借用冲突）。
#[derive(Default)]
pub struct RemoteFileTreeOutput {
    /// 面板实际宽度（mod.rs 持久化 + 描边用；隐藏时 None）。
    pub panel_width: Option<f32>,
    /// 面板矩形（mod.rs 描边 / 拖宽手柄用；隐藏时 None）。
    pub panel_rect: Option<egui::Rect>,
    /// 本帧被点击的目录节点 id（翻转展开态：纯本地、未缓存则触发 ListDir）。
    pub dir_clicks: Vec<usize>,
    /// 「显示隐藏项」勾选变化（main 闭包后 set + 重列根）。
    pub toggle_hidden: Option<bool>,
    /// 本帧双击的文件的被控端路径（#5：main 起 Fetch → 传到控制端本地默认程序打开）。
    pub fetch_open: Option<String>,
    /// 本帧右键「复制」的远程项 (path, name, is_dir)（#7 下载源 → 文件剪贴板 Remote 侧）。
    pub copy_files: Option<(String, String, bool)>,
    /// 本帧右键「粘贴到此目录」的远程目录 path（上传目标；片5 接入）。
    pub paste_into: Option<String>,
    /// 本帧点了「刷新」图标的目录节点 id（重拉该目录最新内容；新增文件可见）。
    pub refresh_dir: Option<usize>,
}

/// M5.3 part3c-2 控制端远程视图——按需浏览被控端文件系统的目录树（只读渲染）。
///
/// 按 [`RemoteFileTree::visible_rows`] 画当前可见行（自有展开态驱动三角 → 修 #1；含文件
/// 行 → 修 #2；点目录收集到 `out.dir_clicks` → main 翻转展开 + 按需 `ListDir` → 修 #3/#4；
/// 展开/折叠纯本地不发帧 → 修 #6）。面板外壳（id `lumen_filetree` / 宽度 / 折叠 / 描边）
/// 与本地 [`show`] 同款，mod.rs 描边/拖宽手柄逻辑无需分支。`ft=None`（会话起始 / 等待
/// cwd）画占位、绝不回落本地树。
pub fn show_remote(
    root_ui: &mut egui::Ui,
    ft: Option<&crate::remote_ws::RemoteFileTree>,
    visible: bool,
    pal: &theme::Palette,
    width: f32,
    // part3c-2 #7：是否显示远程目录右键「粘贴到此目录」（= 本地侧剪贴板有项，上传目标）。
    can_paste: bool,
    // part3c-2 #5a：是否正在控制某设备——未控制时占位文案显示「未连接设备」而非「等待 cwd」。
    controlling: bool,
) -> RemoteFileTreeOutput {
    let mut out = RemoteFileTreeOutput::default();
    if visible {
        let resp = egui::Panel::left("lumen_filetree")
            .default_size(width)
            .size_range(crate::settings::FILETREE_WIDTH_MIN..=crate::settings::FILETREE_WIDTH_MAX)
            .resizable(true)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.filetree_fill)
                    .inner_margin(egui::Margin::symmetric(6, 8)),
            )
            .show_inside(root_ui, |ui| {
                remote_panel_ui(ui, ft, pal, can_paste, controlling, &mut out);
            });
        out.panel_width = Some(resp.response.rect.width());
        out.panel_rect = Some(resp.response.rect);
    }
    out
}

/// 远程树右键菜单动作（经 `RefCell` 收口，循环结束后写入 `out`——与本地树 `MenuAction`
/// 同款范式，避免在嵌套闭包里直接借 `&mut out` 导致菜单不触发）。
enum RemoteMenuAction {
    /// 复制远程项（下载源）：(path, name, is_dir)。
    Copy(String, String, bool),
    /// 粘贴到此远程目录（上传目标）：dir path。
    Paste(String),
}

/// 远程树面板内容：根名工具条 + 「显示隐藏项」勾选 + 可见行（按 depth 缩进，三角由
/// 控制端自有展开态驱动）。右键「复制」（下载源）/ 目录「粘贴到此目录」（上传目标）。
fn remote_panel_ui(
    ui: &mut egui::Ui,
    ft: Option<&crate::remote_ws::RemoteFileTree>,
    pal: &theme::Palette,
    can_paste: bool,
    controlling: bool,
    out: &mut RemoteFileTreeOutput,
) {
    use crate::remote_ws::RemoteRowKind;
    let s = crate::i18n::strings();
    let root_label = ft.and_then(crate::remote_ws::RemoteFileTree::root_label);
    // 占位文案：未控制设备 → 「未连接设备」（#5a）；控制中但 cwd 未到 → 「等待 shell 上报路径」。
    let placeholder = if controlling {
        s.filetree_waiting_cwd
    } else {
        s.remote_not_connected
    };
    // 工具条：树根名（basename，悬停看全路径）+ 「显示隐藏项」勾选。
    ui.horizontal(|ui| {
        let label = root_label.map_or_else(
            || placeholder.to_string(),
            |r| {
                r.rsplit(['/', '\\'])
                    .find(|seg| !seg.is_empty())
                    .unwrap_or(r)
                    .to_string()
            },
        );
        ui.add(egui::Label::new(egui::RichText::new(label).strong().color(pal.fg)).truncate())
            .on_hover_text(root_label.unwrap_or(""));
    });
    // 树未到：占位，不画树（绝不回落本机树）。
    let Some(ft) = ft.filter(|f| f.root_label().is_some()) else {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(placeholder).size(11.0).color(pal.fg_dim));
        return;
    };
    // 「显示隐藏项」勾选（变化经 out.toggle_hidden 回传 main）。
    let mut show_hidden = ft.show_hidden();
    if ui
        .checkbox(&mut show_hidden, egui::RichText::new(s.remote_show_hidden).size(11.0))
        .changed()
    {
        out.toggle_hidden = Some(show_hidden);
    }
    ui.add_space(2.0);
    // 右键菜单经 RefCell 收口，循环后写入 out——与本地树 MenuAction 同款；直接在 context_menu
    // 闭包里借 &mut out 会导致菜单不触发（复制/粘贴失效）。
    let menu: RefCell<Option<RemoteMenuAction>> = RefCell::new(None);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 三角紧贴名字：缩小行内控件间距（#1：默认 8px 间距让三角离名字太远，与本地树不一致）。
            ui.spacing_mut().item_spacing.x = 3.0;
            // 每行 push_id(row.id, i) 给唯一稳定 id：否则各行 selectable_label 的右键菜单 id 冲突，
            // 右键任一行都弹**第一行**的菜单（修 #4：复制永远复制第一个）。
            for (i, row) in ft.visible_rows().into_iter().enumerate() {
                ui.push_id((row.id, i), |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(row.depth as f32 * 12.0);
                        match row.kind {
                            RemoteRowKind::Dir { open } => {
                                // 三角用 painter 画——`▾`/`▸` 字形在 UI 字体里缺失会渲染成
                                // tofu 方块（修 #1）。点三角或名字都翻转展开。
                                let (tri_rect, tri_resp) = ui
                                    .allocate_exact_size(egui::vec2(11.0, 16.0), egui::Sense::click());
                                paint_tri(ui.painter(), tri_rect, open, pal.fg_dim);
                                let resp = ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new(&row.name).color(pal.fg),
                                    )
                                    .on_hover_text(&row.path);
                                if resp.clicked() || tri_resp.clicked() {
                                    out.dir_clicks.push(row.id);
                                }
                                // #6 刷新图标：悬停目录行时名字右侧出现，点击重拉该目录最新内容。
                                let (rf_rect, rf_resp) = ui
                                    .allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
                                if resp.hovered() || tri_resp.hovered() || rf_resp.hovered() {
                                    paint_refresh_small(ui.painter(), rf_rect, pal.fg_dim);
                                    if rf_resp.on_hover_text(s.remote_refresh_dir_tip).clicked() {
                                        out.refresh_dir = Some(row.id);
                                    }
                                }
                                // 右键：复制（下载源）/ 粘贴到此目录（上传目标，can_paste 时）。
                                resp.context_menu(|ui| {
                                    ui.set_min_width(150.0);
                                    if ui.button(s.remote_menu_copy).clicked() {
                                        *menu.borrow_mut() = Some(RemoteMenuAction::Copy(
                                            row.path.clone(),
                                            row.name.clone(),
                                            true,
                                        ));
                                        ui.close();
                                    }
                                    if can_paste && ui.button(s.remote_menu_paste).clicked() {
                                        *menu.borrow_mut() =
                                            Some(RemoteMenuAction::Paste(row.path.clone()));
                                        ui.close();
                                    }
                                });
                            }
                            RemoteRowKind::File => {
                                // 双击 → Fetch 传到控制端本地默认程序打开（#5）。悬停看全路径。
                                let resp = ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new(&row.name).color(pal.fg),
                                    )
                                    .on_hover_text(&row.path);
                                // 右键：复制（下载源 → 文件剪贴板 Remote 侧）。
                                resp.context_menu(|ui| {
                                    ui.set_min_width(150.0);
                                    if ui.button(s.remote_menu_copy).clicked() {
                                        *menu.borrow_mut() = Some(RemoteMenuAction::Copy(
                                            row.path.clone(),
                                            row.name.clone(),
                                            false,
                                        ));
                                        ui.close();
                                    }
                                });
                                if resp.double_clicked() {
                                    out.fetch_open = Some(row.path.clone());
                                }
                            }
                            RemoteRowKind::Loading => {
                                remote_placeholder_row(ui, pal, s.filetree_loading);
                            }
                            RemoteRowKind::Unreadable => {
                                remote_placeholder_row(ui, pal, s.filetree_unreadable);
                            }
                            RemoteRowKind::Overflow(n) => {
                                remote_placeholder_row(
                                    ui,
                                    pal,
                                    &crate::i18n::fmt1(s.filetree_overflow_fmt, n),
                                );
                            }
                        }
                    });
                });
            }
        });
    // 循环结束后写入 out（避免 context_menu 闭包里嵌套借 &mut out）。
    match menu.into_inner() {
        Some(RemoteMenuAction::Copy(path, name, is_dir)) => {
            out.copy_files = Some((path, name, is_dir));
        }
        Some(RemoteMenuAction::Paste(dir)) => out.paste_into = Some(dir),
        None => {}
    }
}

/// 远程树目录三角（painter 画，避免 `▾`/`▸` 字形缺失渲染成 tofu）：`open`=朝下、否则朝右。
fn paint_tri(painter: &egui::Painter, rect: egui::Rect, open: bool, color: egui::Color32) {
    let c = rect.center();
    let r = 3.5_f32;
    let pts = if open {
        vec![
            egui::pos2(c.x - r, c.y - r * 0.5),
            egui::pos2(c.x + r, c.y - r * 0.5),
            egui::pos2(c.x, c.y + r * 0.8),
        ]
    } else {
        vec![
            egui::pos2(c.x - r * 0.5, c.y - r),
            egui::pos2(c.x - r * 0.5, c.y + r),
            egui::pos2(c.x + r * 0.8, c.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

/// 远程树目录行悬停时的小刷新图标（painter 画约 280° 圆弧 + 箭头）。
fn paint_refresh_small(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 4.0_f32;
    let stroke = egui::Stroke::new(1.3, color);
    let seg = 10usize;
    let (start, sweep) = (60.0_f32, 280.0_f32);
    let pts: Vec<egui::Pos2> = (0..=seg)
        .map(|i| {
            let a = (start + sweep * (i as f32 / seg as f32)).to_radians();
            egui::pos2(c.x + r * a.cos(), c.y - r * a.sin())
        })
        .collect();
    for w in pts.windows(2) {
        painter.line_segment([w[0], w[1]], stroke);
    }
    // 末端箭头（沿弧切线方向两短线）。
    if let (Some(&tip), Some(&prev)) = (pts.last(), pts.get(pts.len().wrapping_sub(2))) {
        let dir = (tip - prev).normalized();
        let perp = egui::vec2(-dir.y, dir.x);
        let al = 2.6_f32;
        painter.line_segment([tip, tip - dir * al + perp * al * 0.7], stroke);
        painter.line_segment([tip, tip - dir * al - perp * al * 0.7], stroke);
    }
}

/// 远程树占位行（加载中 / 无法读取 / 溢出）：灰斜体、不可交互。
fn remote_placeholder_row(ui: &mut egui::Ui, pal: &theme::Palette, text: &str) {
    ui.add(egui::Label::new(
        egui::RichText::new(text).italics().color(pal.fg_dim),
    ));
}

/// 新建文件/文件夹的名字校验（Windows 文件名规则 + 注入防御）。
///
/// 返回值的 `&'static str` 是当前语言的错误文案（由 i18n 表提供）。
///
/// part3c-2 片5：被控端 Put/MkDir 写盘前复用此校验拒绝非法 / 穿越名字（`pub(crate)`）。
pub(crate) fn validate_entry_name(name: &str) -> Result<(), &'static str> {
    let s = crate::i18n::strings();
    if name.is_empty() {
        return Err(s.validate_name_empty);
    }
    if name == "." || name == ".." {
        return Err(s.validate_name_illegal);
    }
    if name.chars().any(char::is_control) {
        return Err(s.validate_name_control_chars);
    }
    if name
        .chars()
        .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(s.validate_name_bad_chars);
    }
    if name.ends_with(['.', ' ']) {
        // Win32 命名空间会静默吞掉结尾点/空格，建出来的名字和输入
        // 对不上，直接拒绝。
        return Err(s.validate_name_trailing);
    }
    // Windows 保留设备名（CON/PRN/AUX/NUL/COM1-9/LPT1-9）：去扩展名后大小写不敏感匹配则拒——
    // 否则 `rename(.tmp, dir\NUL)` 会被 Win32 解析成设备、写入被静默重定向（part3c-2 片5 LOW-2）。
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0');
    if reserved {
        return Err(s.validate_name_illegal);
    }
    Ok(())
}

/// 把字符串按 PowerShell 单引号字符串规则转义：词法器视为单引号的
/// 全部同形字一律翻倍（详见 [`cd_command`] 的安全说明）。
fn escape_powershell_single_quotes(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        if is_powershell_single_quote(c) {
            // 翻倍：单引号串内连续两个（同形）单引号表示一个字面引号。
            escaped.push(c);
        }
        escaped.push(c);
    }
    escaped
}

/// 生成向 PowerShell 注入的 cd 命令字节（含回车）。
///
/// 单引号字符串内空格/中文/`$`/反引号都不展开，是注入路径最安全的
/// 引用方式；但 PowerShell 词法器把一组 Unicode 同形字也当单引号
/// 处理（IsSingleQuote：U+0027/U+2018/U+2019/U+201A/U+201B），必须
/// 全部翻倍转义——只翻倍 ASCII `'` 时，目录名含弯引号即可逃逸出
/// 字符串实现命令注入（M3 审查 high 项）。
///
/// 含 ASCII 控制字符（换行/回车会被行编辑器当 Enter 提前断行逃出
/// 引号）的路径直接返回空字节串拒绝注入；上游 UI（目录激活分支）
/// 已先行拦截，这里是纵深防御。
pub fn cd_command(path: &Path) -> Vec<u8> {
    let raw = path.display().to_string();
    if raw.chars().any(char::is_control) {
        log::warn!("目录名含控制字符，拒绝生成 cd 命令: {}", path.display());
        return Vec::new();
    }
    format!("cd '{}'\r", escape_powershell_single_quotes(&raw)).into_bytes()
}

/// 拖放到终端区时插入命令行的路径文本（**不带回车**，用户接着编辑）。
///
/// 纯安全字符（字母数字与少量路径符号）的路径裸插；含空格/特殊字符
/// 时用单引号包裹并按 [`escape_powershell_single_quotes`] 翻倍内部
/// （同形）单引号——与 [`cd_command`] 共用同一套转义设施（弯引号
/// 同形字防御一致）。含控制字符的路径返回空字节串拒绝插入。
pub fn path_insert_text(path: &Path) -> Vec<u8> {
    let raw = path.display().to_string();
    if raw.chars().any(char::is_control) {
        log::warn!("路径含控制字符，拒绝插入: {}", path.display());
        return Vec::new();
    }
    if raw.chars().all(is_plain_path_char) {
        return raw.into_bytes();
    }
    format!("'{}'", escape_powershell_single_quotes(&raw)).into_bytes()
}

/// 拖放到终端编辑器（Compose 态 footer 输入框）时插入的路径字符串。
///
/// 与 [`path_insert_text`] 使用完全相同的引号包裹规则，但返回 `String`
/// 而非字节串，供 `dispatch(Edit(InsertText(...)))` 路径使用。
/// 尾随空格由调用方按需追加（PTY 路径与编辑器路径不同：PTY 路径写入
/// shell 的行编辑缓冲，尾随空格让光标落在路径后方便继续编辑；编辑器
/// 路径同理，由 main.rs 侧追加以保持两者行为一致）。
///
/// 含控制字符的路径返回 `None`（拒绝插入，纵深防御与 [`path_insert_text`]
/// 一致）。
///
/// # Errors
/// 返回 `None` 而非 `Result`——含控制字符时静默拒绝，调用方直接跳过即可。
pub fn path_insert_text_str(path: &Path) -> Option<String> {
    let raw = path.display().to_string();
    if raw.chars().any(char::is_control) {
        log::warn!("路径含控制字符，拒绝插入（编辑器路径）: {}", path.display());
        return None;
    }
    if raw.chars().all(is_plain_path_char) {
        return Some(raw);
    }
    Some(format!("'{}'", escape_powershell_single_quotes(&raw)))
}

/// 裸插安全的字符白名单（保守集合：不在其中的一律走引号包裹，CJK
/// 路径名被包裹只是冗余、无害）。
fn is_plain_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '\\' | '/' | ':' | '.' | '_' | '-' | '~' | '+')
}

/// PowerShell 词法器视为单引号的全部字符（ASCII `'` + Unicode 同形字，
/// 对应其 CharTraits.IsSingleQuote 集合）。
fn is_powershell_single_quote(c: char) -> bool {
    matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

/// 路径是否含 ASCII 控制字符。NTFS 的 POSIX 命名空间允许换行等控制
/// 字符出现在文件名里（Win32 建不出来但 WSL/原生 API 可以），这类
/// 路径写入 PTY 会被行编辑器当控制键处理，必须拒绝。
fn has_control_chars(path: &Path) -> bool {
    path.display().to_string().chars().any(char::is_control)
}

/// 用系统默认程序打开文件。
///
/// Windows 走 explorer.exe（内部 ShellExecute）：相比 `cmd /C start ""`，
/// 路径不会被 cmd 二次解析（`%` `^` `&` 等元字符与空格/中文都安全）。
/// explorer 转交后立即退出，后台线程回收子进程句柄。
pub fn open_with_default(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    match result {
        Ok(child) => reap_in_background(child),
        Err(e) => log::error!("打开文件失败 {}: {e}", path.display()),
    }
}

/// 在文件管理器中打开并选中目标（右键菜单）。
///
/// Windows 用 `explorer /select,"路径"`：该参数必须作为一个整体原样
/// 传给 explorer——std 的标准引用规则会把整段加引号导致 explorer 不
/// 识别，故用 `raw_arg` 自行引用。Windows 文件名不可能含 `"`，无引号
/// 逃逸风险。
fn reveal_in_explorer(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let child = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()?
    };
    #[cfg(not(windows))]
    let child = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn()?;
    reap_in_background(child);
    Ok(())
}

/// 后台线程回收已 spawn 的子进程句柄（防僵尸句柄堆积）。
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // part3c-2：Option A 的「快照按展开态扁平化与指纹」「快照根默认展开含子项」两测试
    // 已删（snapshot_rows / snapshot_fingerprint / remote_expand 等被删）。Option B 的控制端
    // 远程树状态机单测在片2 随 remote_ws::RemoteFileTree 一并补。

    #[test]
    fn cd命令_普通路径() {
        assert_eq!(
            cd_command(Path::new(r"C:\proj")),
            b"cd 'C:\\proj'\r".to_vec()
        );
    }

    #[test]
    fn cd命令_空格与中文() {
        assert_eq!(
            cd_command(Path::new(r"C:\Program Files\工具")),
            "cd 'C:\\Program Files\\工具'\r".as_bytes().to_vec()
        );
    }

    #[test]
    fn cd命令_单引号翻倍() {
        assert_eq!(
            cd_command(Path::new(r"C:\it's here")),
            b"cd 'C:\\it''s here'\r".to_vec()
        );
    }

    #[test]
    fn cd命令_弯引号同形字翻倍() {
        // PowerShell 把 U+2018/U+2019 也当单引号：不翻倍的话
        // `proj’; calc; ‘x` 这样的目录名就能逃逸出字符串执行 calc。
        assert_eq!(
            cd_command(Path::new("C:\\proj\u{2019}; calc; \u{2018}x")),
            "cd 'C:\\proj\u{2019}\u{2019}; calc; \u{2018}\u{2018}x'\r"
                .as_bytes()
                .to_vec()
        );
        // U+201A / U+201B 同属 IsSingleQuote 集合，一并翻倍。
        assert_eq!(
            cd_command(Path::new("C:\\a\u{201A}b\u{201B}c")),
            "cd 'C:\\a\u{201A}\u{201A}b\u{201B}\u{201B}c'\r"
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn cd命令_美元与反引号原样() {
        // 单引号字符串内 `$` 与反引号都是字面量：不展开、无需转义。
        assert_eq!(
            cd_command(Path::new(r"C:\$env`whoami;rm")),
            b"cd 'C:\\$env`whoami;rm'\r".to_vec()
        );
    }

    #[test]
    fn cd命令_控制字符拒绝() {
        // 换行/回车会被行编辑器当 Enter 提前断行逃出引号，ESC 会被
        // 当控制序列——一律拒绝生成命令（返回空字节串 = 不注入）。
        assert!(cd_command(Path::new("C:\\a\nb")).is_empty());
        assert!(cd_command(Path::new("C:\\a\rb")).is_empty());
        assert!(cd_command(Path::new("C:\\a\x1bb")).is_empty());
        assert!(cd_command(Path::new("C:\\a\tb")).is_empty());
    }

    #[test]
    fn 拖放插入_纯安全字符裸插() {
        assert_eq!(
            path_insert_text(Path::new(r"C:\proj\src\main.rs")),
            b"C:\\proj\\src\\main.rs".to_vec()
        );
    }

    #[test]
    fn 拖放插入_空格与中文包裹引号() {
        assert_eq!(
            path_insert_text(Path::new(r"C:\Program Files\app.exe")),
            b"'C:\\Program Files\\app.exe'".to_vec()
        );
        // 中文不在白名单：包裹引号（冗余但无害）。
        assert_eq!(
            path_insert_text(Path::new(r"C:\工具\a.txt")),
            "'C:\\工具\\a.txt'".as_bytes().to_vec()
        );
    }

    #[test]
    fn 拖放插入_弯引号同形字翻倍() {
        // 与 cd_command 同一套转义：弯引号也翻倍，否则可逃逸出字符串。
        assert_eq!(
            path_insert_text(Path::new("C:\\a\u{2019}b c")),
            "'C:\\a\u{2019}\u{2019}b c'".as_bytes().to_vec()
        );
        assert_eq!(
            path_insert_text(Path::new(r"C:\it's here")),
            b"'C:\\it''s here'".to_vec()
        );
    }

    #[test]
    fn 拖放插入_美元符包裹后字面量() {
        // `$` 不在白名单 → 包裹单引号，串内 `$` 是字面量不展开。
        assert_eq!(
            path_insert_text(Path::new(r"C:\$env stuff")),
            b"'C:\\$env stuff'".to_vec()
        );
    }

    #[test]
    fn 拖放插入_控制字符拒绝() {
        assert!(path_insert_text(Path::new("C:\\a\nb")).is_empty());
        assert!(path_insert_text(Path::new("C:\\a\x1bb")).is_empty());
    }

    // ── path_insert_text_str：编辑器路径，引号规则与 path_insert_text 完全一致 ──

    #[test]
    fn 编辑器路径_纯安全字符裸插() {
        assert_eq!(
            path_insert_text_str(Path::new(r"C:\proj\src\main.rs")),
            Some("C:\\proj\\src\\main.rs".to_string())
        );
    }

    #[test]
    fn 编辑器路径_空格与中文包裹引号() {
        assert_eq!(
            path_insert_text_str(Path::new(r"C:\Program Files\app.exe")),
            Some("'C:\\Program Files\\app.exe'".to_string())
        );
        // 中文不在白名单：包裹引号（冗余但无害，与 path_insert_text 一致）。
        assert_eq!(
            path_insert_text_str(Path::new(r"C:\工具\a.txt")),
            Some("'C:\\工具\\a.txt'".to_string())
        );
    }

    #[test]
    fn 编辑器路径_弯引号同形字翻倍() {
        assert_eq!(
            path_insert_text_str(Path::new("C:\\a\u{2019}b c")),
            Some("'C:\\a\u{2019}\u{2019}b c'".to_string())
        );
        assert_eq!(
            path_insert_text_str(Path::new(r"C:\it's here")),
            Some("'C:\\it''s here'".to_string())
        );
    }

    #[test]
    fn 编辑器路径_控制字符拒绝() {
        // 控制字符路径返回 None（与 path_insert_text 返回空字节串语义对称）。
        assert_eq!(path_insert_text_str(Path::new("C:\\a\nb")), None);
        assert_eq!(path_insert_text_str(Path::new("C:\\a\x1bb")), None);
    }

    #[test]
    fn 编辑器路径_与字节版本输出一致() {
        // 验证 path_insert_text_str 与 path_insert_text 输出字符串完全一致。
        let cases = [
            r"C:\proj\src\main.rs",
            r"C:\Program Files\app",
            r"C:\工具\测试文件.txt",
            r"C:\$env stuff",
        ];
        for s in &cases {
            let p = Path::new(s);
            let bytes = path_insert_text(p);
            let as_str = std::str::from_utf8(&bytes).expect("path_insert_text 输出应为有效 UTF-8");
            assert_eq!(
                path_insert_text_str(p).as_deref(),
                Some(as_str),
                "路径 {s} 的两个函数输出应相同"
            );
        }
    }

    #[test]
    fn 新建名字校验() {
        assert!(validate_entry_name("notes.txt").is_ok());
        assert!(validate_entry_name("新建文件夹").is_ok());
        assert!(validate_entry_name("").is_err());
        assert!(validate_entry_name(".").is_err());
        assert!(validate_entry_name("..").is_err());
        assert!(validate_entry_name("a/b").is_err());
        assert!(validate_entry_name(r"a\b").is_err());
        assert!(validate_entry_name("a:b").is_err());
        assert!(validate_entry_name("a*b").is_err());
        assert!(validate_entry_name("a?b").is_err());
        assert!(validate_entry_name("a\"b").is_err());
        assert!(validate_entry_name("a<b").is_err());
        assert!(validate_entry_name("a|b").is_err());
        assert!(validate_entry_name("a\nb").is_err());
        assert!(validate_entry_name("尾点.").is_err());
        assert!(validate_entry_name("尾空格 ").is_err());
    }

    #[test]
    fn 后台读目录_排序与隐藏过滤() {
        let base = std::env::temp_dir().join(format!("lumen_ft_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("zdir")).expect("建测试目录失败");
        std::fs::write(base.join("Afile.txt"), b"x").expect("写测试文件失败");
        std::fs::write(base.join(".hidden"), b"x").expect("写测试文件失败");
        let (entries, overflow) = read_dir_worker(&base).expect("读目录应成功");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(overflow, 0);
        // 隐藏项被过滤；目录排在文件前。
        let names: Vec<String> = entries.iter().map(|(p, _)| display_name(p)).collect();
        assert_eq!(names, vec!["zdir".to_owned(), "Afile.txt".to_owned()]);
        assert!(entries[0].1, "目录应标记为 is_dir");
        assert!(!entries[1].1, "文件不应标记为 is_dir");
    }

    #[test]
    fn 后台读目录_不存在目录返回err() {
        assert!(read_dir_worker(Path::new(r"C:\lumen_不存在的目录_单测专用")).is_err());
    }

    #[test]
    fn 搜索_递归匹配与隐藏过滤() {
        let base = std::env::temp_dir().join(format!("lumen_search_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub").join("deep")).expect("建测试目录失败");
        std::fs::write(base.join("Report.md"), b"x").expect("写测试文件失败");
        std::fs::write(base.join("sub").join("report_v2.md"), b"x").expect("写测试文件失败");
        std::fs::write(base.join("sub").join("deep").join("REPORTS.txt"), b"x")
            .expect("写测试文件失败");
        std::fs::write(base.join(".report_hidden"), b"x").expect("写测试文件失败");
        std::fs::write(base.join("other.txt"), b"x").expect("写测试文件失败");
        let epoch = AtomicU64::new(7);
        // 不分大小写包含匹配；隐藏项与不匹配项被过滤。
        let (items, truncated) = search_worker(&base, "report", 7, &epoch);
        let _ = std::fs::remove_dir_all(&base);
        assert!(!truncated);
        let mut names: Vec<String> = items.iter().map(|(p, _)| display_name(p)).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "REPORTS.txt".to_owned(),
                "Report.md".to_owned(),
                "report_v2.md".to_owned()
            ]
        );
    }

    #[test]
    fn 双击合成激活_合并规则() {
        // 已有真实 Activate（回车/上游修复后）：原样采用，双击不重复。
        assert_eq!(
            merge_double_click_activation(vec![3], true, Some(vec![3])),
            vec![3]
        );
        assert_eq!(merge_double_click_activation(vec![3], false, None), vec![3]);
        // 双击 + 本帧点选：合成激活点选节点。
        assert_eq!(
            merge_double_click_activation(Vec::new(), true, Some(vec![5])),
            vec![5]
        );
        // 双击但本帧无点选（点了 closer 三角/空白处）：不激活。
        assert!(merge_double_click_activation(Vec::new(), true, None).is_empty());
        // 非双击且无 Activate：不激活。
        assert!(merge_double_click_activation(Vec::new(), false, Some(vec![5])).is_empty());
    }

    /// egui_ltreeview 0.7.0 上游 bug 的无头复现（合成激活路径的依据）：
    /// 鼠标双击不产生 Activate 动作，但产生 SetSelected + 整树 Response
    /// 的 double_clicked。**若本测试失败**说明上游已修复双击 Activate，
    /// 应移除 merge_double_click_activation 的合成分支防止双重激活。
    #[test]
    fn 双击激活_上游bug复现与合成信号() {
        use egui_ltreeview::{NodeBuilder, TreeView, TreeViewState};
        let ctx = egui::Context::default();
        let mut tree: TreeViewState<usize> = TreeViewState::default();
        // 跑一帧：返回 (动作列表, 整树 Response 是否双击)。
        let frame = |tree: &mut TreeViewState<usize>, events: Vec<egui::Event>, t: f64| {
            let mut actions = Vec::new();
            let mut dbl = false;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 240.0),
                )),
                time: Some(t),
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(raw, |ui| {
                let (resp, acts) = TreeView::new(egui::Id::new("dbl_repro"))
                    .allow_multi_selection(false)
                    .allow_drag_and_drop(true)
                    .show_state(ui, tree, |b| {
                        b.node(NodeBuilder::leaf(7usize).label("目标文件"));
                    });
                dbl = resp.double_clicked();
                actions = acts;
            });
            (actions, dbl)
        };
        let pos = egui::pos2(60.0, 10.0); // 根 Ui 无边距下的首行行内
        let button = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        // 帧 0：初始布局（建立行矩形与悬停）。
        frame(&mut tree, vec![egui::Event::PointerMoved(pos)], 0.0);
        // 第一击（按下/抬起分两帧，与真实输入一致）。
        frame(&mut tree, vec![button(true)], 0.05);
        let (a1, _) = frame(&mut tree, vec![button(false)], 0.10);
        assert!(
            a1.iter()
                .any(|a| matches!(a, Action::SetSelected(s) if s == &vec![7])),
            "首击应命中并点选节点（命不中说明测试坐标偏离行矩形）: {a1:?}"
        );
        // 第二击（双击窗口 0.3s、6px 内）。
        frame(&mut tree, vec![button(true)], 0.15);
        let (a2, dbl) = frame(&mut tree, vec![button(false)], 0.20);
        assert!(dbl, "egui 应判定为双击");
        assert!(
            !a2.iter().any(|a| matches!(a, Action::Activate(_))),
            "egui_ltreeview 上游已修复双击 Activate：请移除 merge_double_click_activation 合成分支"
        );
        // 合成路径依赖的信号组合成立：双击 + 本帧点选 → 激活点选节点。
        let sel = a2.iter().find_map(|a| match a {
            Action::SetSelected(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(
            merge_double_click_activation(Vec::new(), dbl, sel),
            vec![7],
            "双击第二击应产生 SetSelected，供合成激活使用"
        );
    }

    #[test]
    fn 搜索_代次作废提前退出() {
        let base = std::env::temp_dir().join(format!("lumen_search_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建测试目录失败");
        std::fs::write(base.join("match.txt"), b"x").expect("写测试文件失败");
        // 当前代次已前进（模拟用户改了输入）：扫描应立即截断退出。
        let epoch = AtomicU64::new(8);
        let (items, truncated) = search_worker(&base, "match", 7, &epoch);
        let _ = std::fs::remove_dir_all(&base);
        assert!(truncated);
        assert!(items.is_empty());
    }
}
