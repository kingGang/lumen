//! 文件树栏（M3.3）：跟随激活会话 cwd 的只读目录树。
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui_ltreeview::{Action, NodeBuilder, TreeView, TreeViewBuilder, TreeViewState};

use super::theme;

/// 文件树栏宽度（逻辑像素）。
pub const PANEL_WIDTH: f32 = 220.0;
/// 收起后保留的窄条宽度（容纳展开按钮，保证可发现性）。
const STRIP_WIDTH: f32 = 22.0;
/// 单层最多展示的条目数。
const MAX_ENTRIES_PER_DIR: usize = 1000;
/// 目录枚举的硬上限（含被展示的部分）：超大目录（十万级）只枚举到
/// 这里即提前终止，枚举成本封顶；此时溢出计数是「至少还有 N 项」的
/// 下界（截断计数），展示文案不变。
const ENUM_HARD_CAP: usize = MAX_ENTRIES_PER_DIR * 10;
/// 「shell 忙」轻提示的展示时长。
const HINT_DURATION: Duration = Duration::from_secs(3);
/// 后台读目录在途时的回包轮询间隔（本应用事件循环只消费 egui 的
/// repaint_delay，worker 线程无法直接唤醒重绘，靠它驱动下一帧收包）。
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    /// 被读取的目录节点 id。
    dir_id: usize,
    /// `Ok((已排序截断的 (路径, 是否目录) 列表, 溢出条数))`；
    /// `Err(())` 表示读取失败（权限/网络盘断连）。
    result: Result<(Vec<(PathBuf, bool)>, usize), ()>,
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
    /// 在途后台读取的目录节点 id 集合（非空时驱动轮询重绘收包）。
    pending: HashSet<usize>,
    /// 代次号：换根/刷新时 +1，旧代回包按号丢弃。
    epoch: u64,
    /// 后台读目录的回包通道（tx 克隆给 worker 线程）。
    reply_tx: mpsc::Sender<LoadReply>,
    reply_rx: mpsc::Receiver<LoadReply>,
    /// 「shell 忙」提示的过期时刻。
    hint_until: Option<Instant>,
}

impl Default for FileTreeState {
    fn default() -> Self {
        let (reply_tx, reply_rx) = mpsc::channel();
        Self {
            visible: true,
            root: None,
            tree: TreeViewState::default(),
            nodes: Vec::new(),
            listings: HashMap::new(),
            pending: HashSet::new(),
            epoch: 0,
            reply_tx,
            reply_rx,
            hint_until: None,
        }
    }
}

impl FileTreeState {
    /// 树根跟随激活会话 cwd：变化（切 tab / cd / 首次上报）时整树重置
    /// ——节点表、目录缓存、展开与选中状态全部重建（规格：根变化时
    /// 重置展开状态）。
    fn sync_root(&mut self, cwd: Option<&Path>) {
        if self.root.as_deref() == cwd {
            return;
        }
        self.root = cwd.map(Path::to_path_buf);
        self.reset_nodes();
    }

    /// 重建节点表（「刷新」按钮也走这里，代价是展开状态一并丢失——
    /// 换取 id 分配的简单与确定性）。代次号 +1：在途后台读取的回包
    /// 全部作废，防旧根/旧树的结果污染新节点表。
    fn reset_nodes(&mut self) {
        self.nodes.clear();
        self.listings.clear();
        self.pending.clear();
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

    /// 收取后台读目录的回包：当前代次的把「加载中…」占位替换为真实
    /// 子项，旧代次（换根/刷新前派发）的直接丢弃。
    fn drain_replies(&mut self) {
        while let Ok(reply) = self.reply_rx.try_recv() {
            if reply.epoch != self.epoch || !self.pending.remove(&reply.dir_id) {
                continue;
            }
            let dir_path = self.nodes[reply.dir_id].path.clone();
            let children = match reply.result {
                Err(()) => vec![push_node(&mut self.nodes, dir_path, NodeKind::Unreadable)],
                Ok((entries, overflow)) => {
                    let mut children: Vec<usize> = entries
                        .into_iter()
                        .map(|(path, is_dir)| {
                            push_node(
                                &mut self.nodes,
                                path,
                                if is_dir { NodeKind::Dir } else { NodeKind::File },
                            )
                        })
                        .collect();
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
}

/// 一帧文件树 UI 的产出（由 main.rs 执行，UI 只产出动作）。
#[derive(Default)]
pub struct FileTreeOutput {
    /// 激活了目录且 shell 空闲：请求向激活会话注入 cd。
    pub cd_dir: Option<PathBuf>,
    /// 激活了文件：用系统默认程序打开。
    pub open_file: Option<PathBuf>,
}

/// 绘制文件树栏（位于 tab 侧栏右侧、终端区左侧）。
/// 收起时画一条窄条（仅展开按钮），展开时画完整面板。
pub fn show(
    root: &mut egui::Ui,
    st: &mut FileTreeState,
    cwd: Option<&Path>,
    shell_idle: bool,
    pal: &theme::Palette,
) -> FileTreeOutput {
    let mut out = FileTreeOutput::default();
    st.sync_root(cwd);
    // 先收后台读目录回包（面板收起时也收：重新展开即见结果）。
    st.drain_replies();

    if st.visible {
        egui::Panel::left("lumen_filetree")
            .exact_size(PANEL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.filetree_fill)
                    .inner_margin(egui::Margin::symmetric(6, 8)),
            )
            .show_inside(root, |ui| panel_ui(ui, st, shell_idle, pal, &mut out));
    } else {
        egui::Panel::left("lumen_filetree_strip")
            .exact_size(STRIP_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.filetree_fill)
                    .inner_margin(egui::Margin::symmetric(1, 8)),
            )
            .show_inside(root, |ui| {
                let btn = egui::Button::new(egui::RichText::new("▶").size(9.0).color(pal.fg_dim))
                    .small();
                if ui.add(btn).on_hover_text("展开文件树 (Ctrl+B)").clicked() {
                    st.visible = true;
                }
            });
    }

    // 仍有在途后台读取（含本帧 panel_ui 刚派发的）：安排轮询重绘，
    // 驱动下一帧继续收包——必须放在面板绘制之后，否则首个派发帧
    // 不会被唤醒，「加载中…」会卡到下一个无关事件才刷新。
    if !st.pending.is_empty() {
        root.ctx().request_repaint_after(LOAD_POLL_INTERVAL);
    }
    out
}

/// 面板内容：工具条 + 轻提示 + 树。
fn panel_ui(
    ui: &mut egui::Ui,
    st: &mut FileTreeState,
    shell_idle: bool,
    pal: &theme::Palette,
    out: &mut FileTreeOutput,
) {
    // —— 工具条：收起按钮 + 根目录名（悬停看全路径）+ 刷新 ——
    ui.horizontal(|ui| {
        let collapse =
            egui::Button::new(egui::RichText::new("◀").size(9.0).color(pal.fg_dim)).small();
        if ui.add(collapse).on_hover_text("收起文件树 (Ctrl+B)").clicked() {
            st.visible = false;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let refresh =
                egui::Button::new(egui::RichText::new("刷新").size(10.0).color(pal.fg_dim))
                    .small();
            if ui.add(refresh).on_hover_text("重新读取目录").clicked() {
                st.reset_nodes();
            }
            let title = st.root.as_deref().map_or_else(|| "文件".to_owned(), display_name);
            let label = egui::Label::new(
                egui::RichText::new(title).size(12.0).color(pal.fg),
            )
            .truncate();
            let resp = ui.add(label);
            if let Some(root) = &st.root {
                resp.on_hover_text(root.display().to_string());
            }
        });
    });

    // —— shell 忙提示（双击目录但 shell 非空闲时短暂显示）——
    if let Some(until) = st.hint_until {
        let now = Instant::now();
        if now < until {
            ui.label(
                egui::RichText::new("Shell 忙碌中，未执行 cd")
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
            egui::RichText::new("等待 shell 上报路径…")
                .size(11.0)
                .color(pal.fg_dim),
        );
        return;
    }

    ui.add_space(2.0);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let FileTreeState {
                tree,
                nodes,
                listings,
                pending,
                epoch,
                reply_tx,
                hint_until,
                ..
            } = st;
            let mut load = LoadCtx {
                nodes,
                listings,
                pending,
                epoch: *epoch,
                tx: reply_tx,
            };
            let (_resp, actions) = TreeView::new(ui.make_persistent_id("lumen_file_tree"))
                .allow_multi_selection(false)
                .allow_drag_and_drop(false)
                .show_state(ui, tree, |builder| {
                    add_node(builder, &mut load, 0, pal);
                });
            // 激活动作（双击/回车）：目录 → cd（shell 空闲才发），
            // 文件 → 系统默认程序打开。单选模式下至多一个节点。
            for action in actions {
                let Action::Activate(act) = action else {
                    continue;
                };
                for id in act.selected {
                    let Some(info) = load.nodes.get(id) else {
                        continue;
                    };
                    match info.kind {
                        NodeKind::Dir => {
                            if has_control_chars(&info.path) {
                                // 路径含控制字符（换行/回车等）：写入 PTY
                                // 会被行编辑器提前断行逃出单引号字符串，
                                // 直接拒绝注入（cd_command 内有同款兜底）。
                                log::warn!(
                                    "目录名含控制字符，拒绝注入 cd: {}",
                                    info.path.display()
                                );
                            } else if shell_idle {
                                out.cd_dir = Some(info.path.clone());
                            } else {
                                // shell 忙：不注入命令，仅树内浏览 + 轻提示。
                                *hint_until = Some(Instant::now() + HINT_DURATION);
                            }
                        }
                        NodeKind::File => out.open_file = Some(info.path.clone()),
                        NodeKind::Overflow(_) | NodeKind::Unreadable | NodeKind::Loading => {}
                    }
                }
            }
        });
}

/// 懒加载上下文：从 [`FileTreeState`] 拆借出的字段（绕开整体借用，
/// `add_node` 递归与激活处理共用）。
struct LoadCtx<'a> {
    nodes: &'a mut Vec<NodeInfo>,
    listings: &'a mut HashMap<usize, DirListing>,
    pending: &'a mut HashSet<usize>,
    epoch: u64,
    tx: &'a mpsc::Sender<LoadReply>,
}

/// 递归添加一个节点（目录展开时先懒加载子项再下钻）。
fn add_node(
    builder: &mut TreeViewBuilder<'_, usize>,
    load: &mut LoadCtx<'_>,
    id: usize,
    pal: &theme::Palette,
) {
    let kind = load.nodes[id].kind;
    match kind {
        NodeKind::File => {
            let name = display_name(&load.nodes[id].path);
            builder.node(NodeBuilder::leaf(id).label(name));
        }
        NodeKind::Overflow(n) => {
            builder.node(NodeBuilder::leaf(id).activatable(false).label(
                egui::RichText::new(format!("…还有 {n} 项未显示"))
                    .size(11.0)
                    .color(pal.fg_dim)
                    .italics(),
            ));
        }
        NodeKind::Unreadable => {
            builder.node(NodeBuilder::leaf(id).activatable(false).label(
                egui::RichText::new("无法读取")
                    .size(11.0)
                    .color(pal.fg_dim)
                    .italics(),
            ));
        }
        NodeKind::Loading => {
            builder.node(NodeBuilder::leaf(id).activatable(false).label(
                egui::RichText::new("加载中…")
                    .size(11.0)
                    .color(pal.fg_dim)
                    .italics(),
            ));
        }
        NodeKind::Dir => {
            let name = display_name(&load.nodes[id].path);
            // activatable：双击/回车在目录上触发 cd（ltreeview 随之禁用
            // 双击开合，展开/折叠走左侧 closer 三角，与 Warp 一致）。
            // 根目录默认展开，其余默认收起（懒加载的前提）。
            let open = builder.node(
                NodeBuilder::dir(id)
                    .activatable(true)
                    .default_open(id == 0)
                    .label(name),
            );
            if open {
                ensure_listing(load, id);
                // children 是 id 列表，克隆一份避免递归中长借用 listings。
                let children =
                    load.listings.get(&id).map(|l| l.children.clone()).unwrap_or_default();
                for child in children {
                    add_node(builder, load, child, pal);
                }
            }
            builder.close_dir();
        }
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
    load.listings.insert(id, DirListing { children: vec![placeholder] });
    load.pending.insert(id);
    let tx = load.tx.clone();
    let epoch = load.epoch;
    // 后台线程读盘（M3 审查项：UI 线程同步 read_dir 会被慢速网络盘
    // 冻结整个应用）。线程按请求派发、用后即弃：请求频率受「目录
    // 首次展开」天然限速；卡死在断连网络盘上的线程随超时自行了结，
    // 其回包按代次丢弃即可。
    std::thread::spawn(move || {
        let result = read_dir_worker(&dir);
        // UI 先退出时通道已关：发送失败静默忽略。
        let _ = tx.send(LoadReply { epoch, dir_id: id, result });
    });
}

/// 后台线程的目录读取：过滤隐藏项 → 目录在前文件在后、各按名排序
/// （不区分大小写；小写键在收集时一次算好，避免比较器里 O(n log n)
/// 次重复分配）→ 截断到单层上限。枚举本身受 [`ENUM_HARD_CAP`] 封顶，
/// 超出时溢出计数是下界。读失败（权限/网络盘断连）返回 `Err`，由
/// UI 侧画「无法读取」占位，不 panic。
fn read_dir_worker(dir: &Path) -> Result<(Vec<(PathBuf, bool)>, usize), ()> {
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
        if is_hidden(&name, &meta) {
            continue;
        }
        entries.push((name.to_lowercase(), is_dir(&meta), entry.path()));
    }
    // 目录在前（true 排前用降序），同类按名不区分大小写排序。
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let overflow = entries.len().saturating_sub(MAX_ENTRIES_PER_DIR);
    entries.truncate(MAX_ENTRIES_PER_DIR);
    Ok((entries.into_iter().map(|(_, d, path)| (path, d)).collect(), overflow))
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

/// 生成向 PowerShell 注入的 cd 命令字节（含回车）。
///
/// 单引号字符串内空格/中文/`$`/反引号都不展开，是注入路径最安全的
/// 引用方式；但 PowerShell 词法器把一组 Unicode 同形字也当单引号
/// 处理（IsSingleQuote：U+0027/U+2018/U+2019/U+201A/U+201B），必须
/// 全部翻倍转义——只翻倍 ASCII `'` 时，目录名含弯引号即可逃逸出
/// 字符串实现命令注入（M3 审查 high 项）。
///
/// 含 ASCII 控制字符（换行/回车会被行编辑器当 Enter 提前断行逃出
/// 引号）的路径直接返回空字节串拒绝注入；上游 UI（panel_ui 的目录
/// 激活分支）已先行拦截，这里是纵深防御。
pub fn cd_command(path: &Path) -> Vec<u8> {
    let raw = path.display().to_string();
    if raw.chars().any(char::is_control) {
        log::warn!("目录名含控制字符，拒绝生成 cd 命令: {}", path.display());
        return Vec::new();
    }
    let mut escaped = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        if is_powershell_single_quote(c) {
            // 翻倍：单引号串内连续两个（同形）单引号表示一个字面引号。
            escaped.push(c);
        }
        escaped.push(c);
    }
    format!("cd '{escaped}'\r").into_bytes()
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
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::error!("打开文件失败 {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd命令_普通路径() {
        assert_eq!(cd_command(Path::new(r"C:\proj")), b"cd 'C:\\proj'\r".to_vec());
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
            "cd 'C:\\a\u{201A}\u{201A}b\u{201B}\u{201B}c'\r".as_bytes().to_vec()
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
}
