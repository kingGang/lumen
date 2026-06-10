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
//! - 单层条目上限 1000，超出折叠成「…还有 N 项未显示」占位行
//!   （ltreeview 无虚拟化，这是万级目录不卡的保障）。
//! - 读目录失败（权限/网络盘）显示灰色「无法读取」占位行，不 panic。
//! - 激活（双击/回车）目录 → shell 空闲时请求注入 cd，忙时仅提示；
//!   激活文件 → 系统默认程序打开；单击仅选中，开合走 closer 三角。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_ltreeview::{Action, NodeBuilder, TreeView, TreeViewBuilder, TreeViewState};

use super::theme;

/// 文件树栏宽度（逻辑像素）。
pub const PANEL_WIDTH: f32 = 220.0;
/// 收起后保留的窄条宽度（容纳展开按钮，保证可发现性）。
const STRIP_WIDTH: f32 = 22.0;
/// 单层最多展示的条目数。
const MAX_ENTRIES_PER_DIR: usize = 1000;
/// 「shell 忙」轻提示的展示时长。
const HINT_DURATION: Duration = Duration::from_secs(3);

/// 节点种类（Overflow/Unreadable 是不可交互的占位行）。
#[derive(Clone, Copy)]
enum NodeKind {
    Dir,
    File,
    /// 该层还有 N 项因单层上限未显示。
    Overflow(usize),
    /// 目录读取失败（权限/网络盘断连）。
    Unreadable,
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
    /// 懒加载缓存：目录节点 id → 子节点列表。
    listings: HashMap<usize, DirListing>,
    /// 「shell 忙」提示的过期时刻。
    hint_until: Option<Instant>,
}

impl Default for FileTreeState {
    fn default() -> Self {
        Self {
            visible: true,
            root: None,
            tree: TreeViewState::default(),
            nodes: Vec::new(),
            listings: HashMap::new(),
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
    /// 换取 id 分配的简单与确定性）。
    fn reset_nodes(&mut self) {
        self.nodes.clear();
        self.listings.clear();
        self.tree = TreeViewState::default();
        if let Some(root) = &self.root {
            // 根节点固定占用 id 0（add_node 以 0 起步）。
            self.nodes.push(NodeInfo {
                path: root.clone(),
                kind: NodeKind::Dir,
            });
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

    if !st.visible {
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
        return out;
    }

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
                hint_until,
                ..
            } = st;
            let (_resp, actions) = TreeView::new(ui.make_persistent_id("lumen_file_tree"))
                .allow_multi_selection(false)
                .allow_drag_and_drop(false)
                .show_state(ui, tree, |builder| {
                    add_node(builder, nodes, listings, 0, pal);
                });
            // 激活动作（双击/回车）：目录 → cd（shell 空闲才发），
            // 文件 → 系统默认程序打开。单选模式下至多一个节点。
            for action in actions {
                let Action::Activate(act) = action else {
                    continue;
                };
                for id in act.selected {
                    let Some(info) = nodes.get(id) else {
                        continue;
                    };
                    match info.kind {
                        NodeKind::Dir => {
                            if shell_idle {
                                out.cd_dir = Some(info.path.clone());
                            } else {
                                // shell 忙：不注入命令，仅树内浏览 + 轻提示。
                                *hint_until = Some(Instant::now() + HINT_DURATION);
                            }
                        }
                        NodeKind::File => out.open_file = Some(info.path.clone()),
                        NodeKind::Overflow(_) | NodeKind::Unreadable => {}
                    }
                }
            }
        });
}

/// 递归添加一个节点（目录展开时先懒加载子项再下钻）。
fn add_node(
    builder: &mut TreeViewBuilder<'_, usize>,
    nodes: &mut Vec<NodeInfo>,
    listings: &mut HashMap<usize, DirListing>,
    id: usize,
    pal: &theme::Palette,
) {
    let kind = nodes[id].kind;
    match kind {
        NodeKind::File => {
            let name = display_name(&nodes[id].path);
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
        NodeKind::Dir => {
            let name = display_name(&nodes[id].path);
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
                ensure_listing(nodes, listings, id);
                // children 是 id 列表，克隆一份避免递归中长借用 listings。
                let children = listings.get(&id).map(|l| l.children.clone()).unwrap_or_default();
                for child in children {
                    add_node(builder, nodes, listings, child, pal);
                }
            }
            builder.close_dir();
        }
    }
}

/// 懒加载：目录首次展开时读取子项，之后命中缓存（「刷新」整体重建）。
fn ensure_listing(
    nodes: &mut Vec<NodeInfo>,
    listings: &mut HashMap<usize, DirListing>,
    id: usize,
) {
    if listings.contains_key(&id) {
        return;
    }
    let dir = nodes[id].path.clone();
    let listing = read_dir_sorted(&dir, nodes);
    listings.insert(id, listing);
}

/// 读目录并整理：过滤隐藏项 → 目录在前文件在后、各按名排序（不区分
/// 大小写）→ 截断到单层上限（超出部分折叠成占位行）。
/// 读失败（权限/网络盘）返回灰色「无法读取」占位，不 panic。
fn read_dir_sorted(dir: &Path, nodes: &mut Vec<NodeInfo>) -> DirListing {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            log::warn!("读目录失败 {}: {e}", dir.display());
            let id = push_node(nodes, dir.to_path_buf(), NodeKind::Unreadable);
            return DirListing { children: vec![id] };
        }
    };
    // (排序名, 是否目录, 路径)。单个条目元数据读取失败（竞态删除等）跳过。
    let mut entries: Vec<(String, bool, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if is_hidden(&name, &meta) {
            continue;
        }
        entries.push((name, is_dir(&meta), entry.path()));
    }
    // 目录在前（true 排前用降序），同类按名不区分大小写排序。
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    let overflow = entries.len().saturating_sub(MAX_ENTRIES_PER_DIR);
    entries.truncate(MAX_ENTRIES_PER_DIR);
    let mut children: Vec<usize> = entries
        .into_iter()
        .map(|(_, d, path)| {
            push_node(nodes, path, if d { NodeKind::Dir } else { NodeKind::File })
        })
        .collect();
    if overflow > 0 {
        children.push(push_node(nodes, dir.to_path_buf(), NodeKind::Overflow(overflow)));
    }
    DirListing { children }
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
/// 单引号字符串内只有 `'` 需要翻倍转义，空格/中文/`$` 都不展开，
/// 是注入路径最安全的引用方式。
pub fn cd_command(path: &Path) -> Vec<u8> {
    let escaped = path.display().to_string().replace('\'', "''");
    format!("cd '{escaped}'\r").into_bytes()
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
}
