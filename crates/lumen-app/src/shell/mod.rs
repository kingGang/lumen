//! 应用外壳 UI（egui）：顶栏 + 侧栏 + 文件树 + 终端工作区布局 +
//! 设置/登录覆盖层。
//!
//! M3.2 起侧栏是真功能的会话 tab 列表：条目（标题 + 未读点 + 激活
//! 高亮）点击切换、右键菜单重命名/关闭、底部新建。M3.3 增加中间一栏
//! 文件树（跟随激活会话 cwd，可折叠）。M3.4 增加设置界面（全屏覆盖
//! 层，入口为侧栏底部齿轮与 Ctrl+,）。M3.5 增加顶栏（标题 + 头像
//! 菜单）与登录覆盖层（mock）。UI 只产出动作（[`ShellOutput`]），
//! 会话增删切换/PTY 写入/设置即时生效/登录写盘由 main.rs 执行。

pub mod completion_ui;
pub mod filetree;
pub mod history_search_ui;
pub mod layout;
pub mod login_ui;
pub mod remote_ui;
pub mod settings_ui;
pub mod statusbar;
pub mod theme;
pub mod toast;
pub mod topbar;

/// 窗格标题栏里关闭按钮的边长（逻辑像素，F5 批2 引入、F7① 迁入
/// 标题栏常驻）。
const PANE_CLOSE_SIZE: f32 = 16.0;

/// 窗格标题栏高度（逻辑像素，F7①）：占高从终端内容区扣除（该格
/// 终端行数相应减少）。pub(crate)：main 的恢复路径按它估算各窗格
/// spawn 时的初始行列（B2 修复，估算须与首帧实际占高同源）。
pub(crate) const PANE_TITLE_HEIGHT: f32 = 24.0;

/// 一个 tab 在侧栏的展示数据（由 main.rs 按帧构造；M3.7 起侧栏
/// 条目 = tab，每 tab 含 1~6 个终端窗格）。
pub struct TabItem {
    pub id: u64,
    /// 名称行（自定义名 > 焦点窗格 cwd 尾目录名 > OSC 标题 > 「会话 N」，
    /// 取值见 Tab::display_name，恒非空）。重命名以它为初值。
    pub name: String,
    /// 路径行（焦点窗格 cwd 完整文件夹路径，OSC 9;9 上报）；cwd 未知
    /// （新会话首个提示符前）为 None，仅画名称行。
    pub path: Option<String>,
    pub active: bool,
    /// 后台期间 tab 内任意窗格有未读输出（条目右侧小圆点）。
    pub unseen: bool,
    /// tab 内窗格数（>1 时条目右侧标「N 格」，F5 批2 视觉打磨）。
    pub pane_count: usize,
    /// 会话图标纹理（F7②：前台运行程序 exe 图标）；None 时回退自绘
    /// 终端字形（取不到图标/非 Windows）。
    pub icon: Option<egui::TextureId>,
    /// 会话是否忙（claude 等 TUI 在工作，由其 OSC 0 标题 spinner 判定）：
    /// 条目右侧画转圈 spinner。
    pub busy: bool,
}

/// 跨帧保留的外壳 UI 状态。
#[derive(Default)]
pub struct ShellState {
    /// 进行中的重命名：(会话 id, 编辑中文本)。编辑期间键盘归 egui。
    pub renaming: Option<(u64, String)>,
    /// 重命名刚开始，下一帧把焦点交给编辑框。
    rename_focus: bool,
    /// 进行中的窗格重命名（需求2）：(窗格会话 id, 编辑中文本)。与侧栏
    /// tab renaming 并行的独立状态——窗格按【稳定 id】定位（非下标，
    /// 避免后台 shell 异步退出 close_pane 重排下标后失配，致编辑框永不
    /// 重绘、pane_renaming 永久 Some、终端键盘死锁）。编辑期间键盘归 egui。
    pub pane_renaming: Option<(u64, String)>,
    /// 窗格重命名刚开始，下一帧把焦点交给标题栏编辑框。
    pane_rename_focus: bool,
    /// 文件树（树根/展开/可见性等跨帧状态）。
    pub filetree: filetree::FileTreeState,
    /// 设置页（开关/分类/字体编辑缓冲等跨帧状态）。
    pub settings: settings_ui::SettingsUiState,
    /// 登录覆盖层（开关/输入缓冲等跨帧状态）。
    pub login: login_ui::LoginUiState,
    /// 历史搜索面板（M4.3 Ctrl+R；开关/query/selected 等跨帧状态）。
    pub history_search: history_search_ui::HistorySearchUiState,
    /// 文件路径补全弹窗（M4.4 批1 Tab；open/selected 跨帧状态）。
    pub completion: completion_ui::CompletionUiState,
    /// 系统提示框队列（toast；shell 内外都可 push，见 toast.rs）。
    pub toast: toast::ToastState,
    /// 进行中的远程设备重命名（M5.2）：(设备 id, 编辑中文本)。编辑期间键盘归 egui。
    pub renaming_device: Option<(String, String)>,
    /// 设备重命名刚开始，下一帧把焦点交给编辑框。
    rename_device_focus: bool,
    /// 远程控制配对 UI 跨帧状态（M5.3 part2b：配对码输入缓冲/焦点）。
    pub remote_ui: remote_ui::RemoteUiState,
}

/// 激活 tab 中一个窗格的展示数据（终端工作区分屏用，F5）。
pub struct PaneView {
    /// 窗格稳定会话 id（= Session.id）：窗格重命名按 id 定位（需求2），
    /// 避免下标随异步增删/换位失配致死锁或改错窗格。
    pub id: u64,
    /// 窗格离屏纹理的 egui 句柄（注册失败的极端情况为 None，该
    /// 窗格本帧只占位不画图像）。
    pub tex: Option<egui::TextureId>,
    /// 是否为焦点窗格（多窗格时画 1px accent 边框 + 标题栏提亮）。
    pub focused: bool,
    /// 标题栏展示名（F7①：cwd 尾目录名 > OSC 标题 > 「窗格 N」，
    /// 与侧栏 display_title 同源取值；过长由 UI 截断）。
    pub title: String,
    /// 标题悬停提示：完整 cwd（截断时可看全）；cwd 未知时 None。
    pub title_hover: Option<String>,
}

/// 背景图绘制参数（P13）：由 main 构造、传入 shell 绘制层。
pub struct BgImageInput {
    /// egui 纹理 id（由 [`background::load_background_texture`] 注册）。
    pub texture_id: egui::TextureId,
    /// 图片原始宽度（用于 cover UV 计算）。
    pub width: u32,
    /// 图片原始高度（用于 cover UV 计算）。
    pub height: u32,
    /// 不透明度（0.05～1.0）。
    pub opacity: f32,
    /// 暗化强度（0.0～0.9，0.0 = 不暗化）。
    pub dim: f32,
}

/// 一帧外壳 UI 的输入（main.rs 按帧构造的状态快照）。
pub struct ShellInput<'a> {
    /// 激活 tab 的窗格（布局顺序：先上排后下排、行内自左向右）。
    pub panes: &'a [PaneView],
    /// 激活 tab 的窗格比例布局（F7③；本帧快照，main 持有真身）。
    pub layout: layout::PaneLayout,
    /// 最大化的窗格下标（P14）：Some 时该窗格独占终端工作区，其余
    /// 窗格本帧不画（矩形占位 NOTHING）；分隔条不显示、拖动换位
    /// 禁用。main 维护下标合法性，shell 侧仍做防御过滤。
    pub maximized: Option<usize>,
    /// tab 条目（侧栏列表；顶栏标题取自其中的激活条目）。
    pub tabs: &'a [TabItem],
    /// 登录态（顶栏头像、头像菜单、设置页 Account 三处同源展示）。
    pub profile: Option<&'a crate::profile::Profile>,
    /// 头像菜单更新项：Some(版本号) = 有就绪更新（显示「更新到 vX」），
    /// None = 无更新（显示「检查更新」）。main 据 update_ready/available 构造。
    pub update_version: Option<String>,
    /// 焦点窗格的 cwd（文件树跟随；OSC 9;9 上报）。
    pub cwd: Option<&'a std::path::Path>,
    /// 焦点窗格 shell 空闲（文件树 cd 注入闸门）。
    pub shell_idle: bool,
    /// 系统当前是否深色模式（P12 Sync with OS：外壳色板与设置页
    /// 「当前主题」展示按它解析生效主题 id）。
    pub os_dark: bool,
    /// 背景图绘制参数（P13）：None 表示未启用/加载失败，不绘制。
    pub bg_image: Option<BgImageInput>,
    /// 当前有效输入模式（M4.1 批E：底部状态栏显示）。
    /// 由 main 每帧调用 effective_mode 推导后传入，shell 侧不缓存。
    #[cfg(feature = "input-editor")]
    pub input_mode: crate::mode::InputMode,
    /// 经典直通模式开关（M4.1 批E：状态栏右端按钮状态）。
    #[cfg(feature = "input-editor")]
    pub force_fallback: bool,
    /// 历史搜索面板本帧展示的行（由 main 在 render 前计算；面板关闭时传空切片）。
    pub history_rows: &'a [history_search_ui::HistoryRow],
    /// 补全弹窗本帧展示数据（M4.4 批1）：Some = 弹窗可见，None = 不显示。
    /// 候选列表由 main 在 render 前计算好。
    pub completion_view: Option<completion_ui::CompletionView<'a>>,
    /// 远程设备列表（M5.2；仅远程 tab 渲染，服务端已按 last_seen 倒序）。
    pub remote_devices: &'a [lumen_protocol::DeviceRecord],
    /// 当前选中的远程设备 id（高亮用）。
    pub active_device_id: Option<&'a str>,
    /// M5.3 远程控制：控制端待配对态（Some = 渲染配对码输入模态）。
    pub remote_pairing: Option<&'a crate::remote_ws::PairingPrompt>,
    /// M5.3 远程控制：被控端来件控制请求态（Some = 渲染来件横幅 + 配对码）。
    pub remote_incoming: Option<&'a crate::remote_ws::IncomingControl>,
    /// M5.3 远程控制：活跃会话态（Some = 渲染「被控中 / 控制中」横幅）。
    pub remote_session: Option<&'a crate::remote_ws::ActiveSession>,
    /// M5.3 part3b：控制端远程镜像离屏纹理（Some = 控制中+远程视图，终端工作区改画
    /// 远端镜像；wgpu 上色，复用窗格渲染器）。
    pub remote_mirror_tex: Option<egui::TextureId>,
    /// M5.3 part3c-1：被控端推来的文件树快照（Some = 已收到首份快照）。
    pub remote_filetree: Option<&'a crate::remote_ws::RemoteFileTree>,
    /// M5.3 part3c-1：是否处于远程视图（控制中+远程视图，= `is_mirror_active`）。为真则
    /// 文件树栏一律画被控端只读树（快照未到则空树+「等待 cwd」占位），**不回落本地树**
    /// ——否则会把本机目录树画进远程栏、且点击 cd/打开误作用于控制端本机（本地/远程串扰）。
    pub remote_view_active: bool,
    /// M5.3 part3c-2 #7：文件剪贴板来源侧（None=空）。本地树据此决定是否显示「粘贴到此目录」
    /// （= 剪贴板为 Remote，下载目标）；远程树据此（= 剪贴板为 Local，上传目标）。
    pub file_clipboard_side: Option<crate::remote_ws::ClipSide>,
    /// M5.3 part3c-2 #7：覆盖弹窗待决的冲突项数（Some = 渲染覆盖确认模态）。
    pub overwrite_conflict_count: Option<usize>,
}

/// part3c-2 #7 覆盖确认模态的用户选择。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverwriteChoice {
    /// 覆盖全部已存在项。
    Overwrite,
    /// 跳过已存在项（只传不冲突的）。
    Skip,
    /// 取消整次粘贴。
    Cancel,
}

/// 一帧外壳 UI 的产出。
pub struct ShellOutput {
    /// 终端工作区整体矩形（egui 逻辑点坐标；拖放落点判定等用）。
    pub term_rect: egui::Rect,
    /// 各窗格的**终端内容矩形**（与 [`ShellInput::panes`] 同序，已
    /// 对齐物理像素；F7① 起不含顶部标题栏——标题栏上的鼠标事件不
    /// 进终端）。main.rs 据此重建离屏纹理 / resize / 路由鼠标与 IME。
    pub pane_rects: Vec<egui::Rect>,
    /// 本帧用户点击了终端区（焦点交还终端）。
    pub term_clicked: bool,
    /// 点击了某个窗格（下标对应 [`ShellInput::panes`]；切换焦点窗格）。
    pub pane_clicked: Option<usize>,
    /// 点击了某窗格标题栏的 ✕（关闭该窗格；F7① 起常驻标题栏，
    /// 单窗格时关闭 = 关整个 tab）。
    pub pane_close: Option<usize>,
    /// 点击了某窗格标题栏的最大化/还原按钮（P14；多窗格时 ✕ 左侧）：
    /// main 对该窗格 toggle 最大化（与 Ctrl+Shift+Enter 同语义）。
    pub pane_maximize: Option<usize>,
    /// 拖动窗格标题栏松手落在另一窗格上：(源窗格, 目标窗格)，请求
    /// 交换两窗格的**内容**（panes 下标互换、布局权重不动——位置
    /// 换、比例格不变；焦点跟随被拖窗格落位。F7②）。落在源格自身
    /// 或所有窗格之外为 None（取消，无副作用）。
    pub pane_swap: Option<(usize, usize)>,
    /// 各窗格标题栏按钮（✕ 与最大化/还原）的命中矩形（egui 逻辑
    /// 坐标，仅可见窗格、不保序）。main.rs 据此让 raw 鼠标路由让位
    /// （不聚焦/不建选区/不交出终端焦点，点击由 egui 侧处理）。
    pub pane_close_rects: Vec<egui::Rect>,
    /// 顶栏窗控：拖动自绘标题栏空白区（M3.8）——main 调 window.drag_window()。
    pub drag_title_bar: bool,
    /// 顶栏窗控：最小化窗口（M3.8）。
    pub minimize_window: bool,
    /// 顶栏窗控：切换最大化/还原（M3.8）。
    pub toggle_maximize_window: bool,
    /// 顶栏窗控：关闭窗口（M3.8）——走与 CloseRequested 同路径（落盘再退）。
    pub close_window: bool,
    /// 顶栏窗控：显示系统窗口菜单（右键标题栏空白区，M3.8）；
    /// 坐标为 egui 逻辑点（调用方换算为物理像素后传 show_window_menu）。
    pub show_window_menu_at: Option<(f32, f32)>,
    /// 顶栏最大化/还原按钮本帧的 egui 逻辑坐标矩形（M3.8 批2 Snap
    /// Layouts 子类化）：main 换算为屏幕物理像素后写入 snap_layouts
    /// 原子，子类过程据此在 WM_NCHITTEST 时返回 HTMAXBUTTON。
    /// None 表示本帧按钮不可见，main 跳过更新（保留上一帧的矩形）。
    pub maximize_btn_rect: Option<egui::Rect>,
    /// 顶栏「＋」：焦点 tab 内新增窗格（同 Ctrl+Shift+D，F5）。
    pub new_pane: bool,
    /// 顶栏③还原窗格大小（P15 / 问题7）：当前 tab 全部窗格比例恢复均分
    /// （最大化态先退出）；复位后 main 落盘。
    pub layout_reset: bool,
    /// 顶栏①切换会话栏显示/隐藏（问题7）：Some(v) = 新可见值，None = 未操作。
    /// main 更新 settings.layout.sidebar_visible 并落盘。
    pub toggle_sidebar: Option<bool>,
    /// 顶栏②切换文件树显示/隐藏（问题7，Ctrl+B 同状态源）。
    pub toggle_filetree: Option<bool>,
    /// 本地/远程视图切换（M5.2）：main 写 settings.layout.view_mode + 存盘。
    pub toggle_view_mode: Option<bool>,
    /// 选中了某远程设备（M5.2）：main 记 active_device_id。
    pub activate_device: Option<String>,
    /// 提交远程设备改名（M5.2）：(设备 id, 新名)。
    pub rename_device: Option<(String, String)>,
    /// 删除远程设备（M5.2）：设备 id。
    pub delete_device: Option<String>,
    /// 设备改名编辑本帧以键盘结束（main 把焦点还终端，仿会话重命名）。
    pub rename_device_ended_by_key: bool,
    /// M5.3：发起控制某设备（双击在线设备 / 右键「连接」）→ main 调 request_control。
    pub connect_device: Option<String>,
    /// M5.3：控制端提交配对码 → main 调 submit_pairing。
    pub submit_pairing_code: Option<String>,
    /// M5.3：控制端取消配对（关弹窗）→ main 调 cancel_pairing。
    pub cancel_pairing: bool,
    /// M5.3：被控端拒绝来件控制请求 → main 调 decline。
    pub decline_control: bool,
    /// M5.3：任一端结束当前远程会话 → main 调 end_session。
    pub end_remote_session: bool,
    /// 分隔条拖动中：(分隔条, 指针位置（逻辑点）)。main 据此把对应
    /// 边界拖到指针处（绝对定位无累积漂移；最小尺寸钳制在 layout）。
    pub divider_drag: Option<(layout::DividerKind, egui::Pos2)>,
    /// 分隔条拖动本帧结束（main 落盘比例，F7 持久化）。
    pub divider_drag_ended: bool,
    /// 双击了分隔条：该方向恢复均分（列分隔=该排列宽、排分隔=排高）。
    pub divider_reset: Option<layout::DividerKind>,
    /// 各分隔条命中矩形（egui 逻辑坐标）。main 据此让 raw 鼠标路由
    /// 让位：按下不聚焦/不建选区/不交出终端焦点（拖动由 egui 处理）。
    pub divider_rects: Vec<egui::Rect>,
    /// 左侧会话栏本帧实际宽度（逻辑点；P10）。main 在指针松开且与
    /// 已存值有差时写入 settings.json（拖动中不写盘）。
    pub sidebar_width: f32,
    /// 文件树栏本帧实际宽度（逻辑点；P10）。收起（窄条）时为 None
    /// ——不覆盖已存的展开宽度。
    pub filetree_width: Option<f32>,
    /// 侧栏/文件树栏右缘的面板拖宽手柄命中矩形（egui 逻辑坐标，
    /// P10）。egui 的 resize 手柄以面板边线为中心向两侧各探入
    /// resize_grab_radius_side 像素，文件树右缘的手柄会盖住终端区
    /// 左缘——main 据此让 raw 鼠标路由让位（按下不聚焦/不建选区/
    /// 不交出终端焦点，拖宽本身由 egui 处理）。
    pub panel_resize_rects: Vec<egui::Rect>,
    /// 点击了某 tab 条目（切换激活）。
    pub activate: Option<u64>,
    /// 请求关闭某 tab（右键菜单）。
    pub close: Option<u64>,
    /// 提交的重命名：(tab id, 新名字)。空字符串 = 清除自定义名。
    pub rename: Option<(u64, String)>,
    /// 重命名编辑本帧以**键盘**结束（Enter 提交 / Esc 取消）。点击
    /// 别处取消时为 false——main 只在键盘结束时把焦点还给终端，
    /// 点击结束时尊重鼠标仲裁的结果（点面板不抢回焦点）。
    pub rename_ended_by_key: bool,
    /// 提交的窗格重命名（需求2）：(窗格会话 id, 新名字)。空字符串 = 清除
    /// 自定义名（回退默认标题）。按窗格【稳定 id】定位（同 tab 的 id 语义）。
    pub pane_rename: Option<(u64, String)>,
    /// 窗格重命名编辑本帧以**键盘**结束（语义同 rename_ended_by_key）。
    pub pane_rename_ended_by_key: bool,
    /// 点击了「新建会话」。
    pub new_session: bool,
    /// 文件树：激活了目录且 shell 空闲，请求向焦点窗格注入 cd。
    pub cd_dir: Option<std::path::PathBuf>,
    /// 文件树：激活了文件，用系统默认程序打开。
    pub open_file: Option<std::path::PathBuf>,
    /// M5.3 part3c-2 控制端远程树：本帧被点击的目录节点 id（main 翻转展开 + 按需 ListDir）。
    pub remote_dir_clicks: Vec<usize>,
    /// 控制端远程树：「显示隐藏项」勾选变化（main set + 重列根）。
    pub remote_toggle_hidden: Option<bool>,
    /// 控制端远程树：双击文件的被控端路径（main 起 Fetch → 传到本地默认程序打开，#5）。
    pub remote_fetch_open: Option<String>,
    /// 控制端远程树：点了「刷新」图标的目录节点 id（main 重拉该目录最新内容）。
    pub remote_refresh_dir: Option<usize>,
    /// 控制端远程树：本帧单击选中的节点 id（main 设 ft.selected → 高亮 + Ctrl+C 下载源）。
    pub remote_select: Option<usize>,
    /// 本帧鼠标是否在文件树面板内（main 存到下一帧，作 Ctrl+C/V 快捷键门控）。
    pub filetree_hovered: bool,
    /// part3c-2 #7：复制文件 / 文件夹到剪贴板 (来源侧, path, name, is_dir)。
    pub file_copy: Option<(crate::remote_ws::ClipSide, String, String, bool, u64)>,
    /// part3c-2 #7：粘贴到某目录 (目标侧, 目录 path)（main 据剪贴板侧 × 目标侧定方向）。
    pub file_paste: Option<(crate::remote_ws::ClipSide, String)>,
    /// part3c-2 #7：覆盖确认模态的本帧选择（main 据此续传 / 跳过 / 取消）。
    pub overwrite_choice: Option<OverwriteChoice>,
    /// 文件树：节点拖放到某窗格，把路径文本插入该窗格命令行（不带
    /// 回车，转义见 filetree::path_insert_text）。元组为 (落点所在
    /// 窗格下标, 路径)；落点不在任何窗格（间隙/区外）时整体为 None。
    pub insert_path: Option<(usize, std::path::PathBuf)>,
    /// 文件树：请求写剪贴板的文本（复制绝对/相对路径；arboard 在
    /// main 持有）。
    pub copy_text: Option<String>,
    /// 文件树：对话框（新建/删除确认）本帧关闭（main 把焦点交还终端）。
    pub filetree_dialog_closed: bool,
    /// 设置页本帧被打开（main 把终端焦点交给 egui）。
    pub settings_opened: bool,
    /// 设置页本帧被关闭（main 把焦点交还终端，IME 复位链路照旧）。
    pub settings_closed: bool,
    /// 设置页改了字体/字号（main 重配置 renderer 并全会话 resize）。
    pub settings_font_changed: bool,
    /// 设置页改了主题（main 切终端 Theme + egui 样式联动）。
    pub settings_theme_changed: bool,
    /// 设置页改了背景图参数（opacity/dim/enabled），不需重载纹理。
    /// main 更新 renderer 透明状态 + 写盘。
    pub settings_background_params_changed: bool,
    /// 设置页改了背景图路径（选新图/清除），需重载纹理。
    /// main 调 apply_background_image + 写盘。
    pub settings_background_image_changed: bool,
    /// 设置页改了界面语言（F6）：main 进 need_save 落盘。
    pub settings_language_changed: bool,
    /// 登录覆盖层本帧被打开（main 把终端焦点交给 egui）。
    pub login_opened: bool,
    /// 登录覆盖层本帧被关闭（main 按覆盖层整体状态决定焦点归属）。
    pub login_closed: bool,
    /// mock 登录成功的档案（main 写盘并更新全局登录态——顶栏头像、
    /// 头像菜单、设置页 Account 三处同源即时联动）。
    pub logged_in: Option<crate::profile::Profile>,
    /// 请求登出（头像菜单或设置页 Account；main 删盘并清登录态）。
    pub logged_out: bool,
    /// 底部状态栏点击了经典直通切换按钮（M4.1 批E）：走 dispatch ToggleFallback 同路径。
    #[cfg(feature = "input-editor")]
    pub toggle_fallback: bool,
    /// 历史搜索面板：用户选定的命令文本（应填入输入框并关闭面板）。
    pub history_accept: Option<String>,
    /// 历史搜索面板：本帧请求关闭（Esc / backdrop 点击）。
    pub history_closed: bool,
    /// 历史搜索面板：query 本帧发生变化（main 应 request_redraw 以便下帧重算结果）。
    pub history_query_changed: bool,
    /// 补全弹窗（M4.4 批1）：用户接受的候选下标（应替换 token 并关闭弹窗）。
    pub completion_accept: Option<usize>,
    /// 补全弹窗（M4.4 批1）：本帧请求关闭（Esc）。
    pub completion_closed: bool,
    /// 设置页「更新」分区或头像菜单「检查更新」（F3）：main 起一次手动检查。
    pub update_check_now: bool,
    /// 设置页改了更新设置（auto_check 开关，F3）：main 进 need_save 落盘。
    pub settings_update_changed: bool,
    /// 设置页改了网络代理（开关/地址）：main 落盘并刷新生效代理镜像。
    pub settings_proxy_changed: bool,
    /// 设置页 Network 改了服务端地址（M5.2）：main 落盘 + 应用 cloud 全局。
    pub settings_server_url_changed: bool,
    /// 头像菜单「更新到 vX」：有就绪更新时显示更新弹窗（main 清 dismissed）。
    pub open_update: bool,
    /// 头像菜单「更新日志」：main 打开 GitHub Releases 页。
    pub open_whats_new: bool,
    /// 头像菜单「文档」：main 打开 GitHub 仓库 README。
    pub open_documentation: bool,
    /// 头像菜单「反馈」：main 打开 GitHub Issues。
    pub open_feedback: bool,
}

/// 绘制整个外壳：顶栏 + 左侧会话栏 + 中间文件树 + 中央终端纹理 +
/// 设置/登录覆盖层。输入是 main 按帧构造的状态快照（[`ShellInput`]）；
/// `app_settings` 是设置页直接编辑的数据（变更经 [`ShellOutput`]
/// 通知 main 即时生效与写盘）；`is_maximized` 用于顶栏窗控按钮图标
/// 切换（M3.8 自绘标题栏）。
pub fn show(
    root: &mut egui::Ui,
    input: &ShellInput<'_>,
    st: &mut ShellState,
    app_settings: &mut crate::settings::Settings,
    is_maximized: bool,
) -> ShellOutput {
    let mut out = ShellOutput {
        term_rect: egui::Rect::NOTHING,
        pane_rects: Vec::new(),
        term_clicked: false,
        pane_clicked: None,
        pane_close: None,
        pane_maximize: None,
        pane_swap: None,
        pane_close_rects: Vec::new(),
        drag_title_bar: false,
        minimize_window: false,
        toggle_maximize_window: false,
        close_window: false,
        show_window_menu_at: None,
        maximize_btn_rect: None,
        new_pane: false,
        layout_reset: false,
        toggle_sidebar: None,
        toggle_filetree: None,
        toggle_view_mode: None,
        activate_device: None,
        rename_device: None,
        delete_device: None,
        rename_device_ended_by_key: false,
        connect_device: None,
        submit_pairing_code: None,
        cancel_pairing: false,
        decline_control: false,
        end_remote_session: false,
        divider_drag: None,
        divider_drag_ended: false,
        divider_reset: None,
        divider_rects: Vec::new(),
        sidebar_width: app_settings.layout.sidebar_width,
        filetree_width: None,
        panel_resize_rects: Vec::new(),
        activate: None,
        close: None,
        rename: None,
        rename_ended_by_key: false,
        pane_rename: None,
        pane_rename_ended_by_key: false,
        new_session: false,
        cd_dir: None,
        open_file: None,
        remote_dir_clicks: Vec::new(),
        remote_toggle_hidden: None,
        remote_fetch_open: None,
        remote_refresh_dir: None,
        remote_select: None,
        filetree_hovered: false,
        file_copy: None,
        file_paste: None,
        overwrite_choice: None,
        insert_path: None,
        copy_text: None,
        filetree_dialog_closed: false,
        settings_opened: false,
        settings_closed: false,
        settings_font_changed: false,
        settings_theme_changed: false,
        settings_background_params_changed: false,
        settings_background_image_changed: false,
        settings_language_changed: false,
        login_opened: false,
        login_closed: false,
        logged_in: None,
        logged_out: false,
        #[cfg(feature = "input-editor")]
        toggle_fallback: false,
        history_accept: None,
        history_closed: false,
        history_query_changed: false,
        completion_accept: None,
        completion_closed: false,
        update_check_now: false,
        settings_update_changed: false,
        settings_proxy_changed: false,
        settings_server_url_changed: false,
        open_update: false,
        open_whats_new: false,
        open_documentation: false,
        open_feedback: false,
    };
    // 生效主题的外壳色板（P12）：Lumen 双主题取手调静态板、其余主题
    // 派生（每帧少量色彩数学，开销可忽略）。
    let pal = &theme::shell_palette(crate::settings::theme_info(
        app_settings.effective_theme_id(input.os_dark),
    ));
    // 重命名目标可能已被关闭（进程退出等）：清掉孤儿编辑态，
    // 否则编辑框永不渲染、也永不失焦，键盘焦点会卡在 egui 侧。
    if st
        .renaming
        .as_ref()
        .is_some_and(|(id, _)| !input.tabs.iter().any(|e| e.id == *id))
    {
        st.renaming = None;
        // 编辑框已随会话消失，视同键盘结束：焦点交还终端（否则键盘
        // 焦点悬空，用户必须再点一次终端区才能打字）。
        out.rename_ended_by_key = true;
    }
    // 同理清窗格重命名孤儿态（需求2）：目标窗格已关闭（含后台 shell 异步
    // 退出走 close_pane）或已切到别的 tab（不在当前 input.panes 里）时，
    // 编辑框永不渲染也永不失焦，pane_renaming 会永久 Some——main 焦点仲裁
    // 据此每帧强制 terminal_focused=false、guard.renaming 恒真，终端键盘被
    // 永久锁死。按【id】比对清孤儿，视同键盘结束交还焦点（对标 tab）。
    if st
        .pane_renaming
        .as_ref()
        .is_some_and(|(id, _)| !input.panes.iter().any(|p| p.id == *id))
    {
        st.pane_renaming = None;
        out.pane_rename_ended_by_key = true;
    }

    // —— 顶栏（先于侧栏加入面板布局，横贯整窗）：标题 + 头像菜单 ——
    // 标题与窗口标题同源（激活 tab 的 display_title，恒非空），
    // 无激活条目（防御）时退回应用名。
    let active_title = input
        .tabs
        .iter()
        .find(|e| e.active)
        .map_or("Lumen", |e| e.name.as_str());
    let tb = topbar::show(
        root,
        active_title,
        input.panes.len(),
        input.profile,
        pal,
        is_maximized,
        topbar::ViewState {
            sidebar_visible: app_settings.layout.sidebar_visible,
            filetree_visible: st.filetree.visible,
            update_version: input.update_version.clone(),
            current_view: app_settings.layout.view_mode,
        },
    );
    if tb.new_pane {
        out.new_pane = true;
    }
    // 头像菜单更新组 / 资源组动作转发。
    if tb.check_update {
        out.update_check_now = true;
    }
    if tb.open_update {
        out.open_update = true;
    }
    if tb.open_whats_new {
        out.open_whats_new = true;
    }
    if tb.open_documentation {
        out.open_documentation = true;
    }
    if tb.open_feedback {
        out.open_feedback = true;
    }
    if tb.reset_layout {
        out.layout_reset = true;
    }
    // 问题7：三视图切换信号转发
    if tb.toggle_sidebar.is_some() {
        out.toggle_sidebar = tb.toggle_sidebar;
    }
    if let Some(v) = tb.toggle_filetree {
        // 文件树 toggle：同步 filetree state（与 Ctrl+B 共享同一 visible 状态源）
        st.filetree.visible = v;
        out.toggle_filetree = Some(v);
    }
    // M5.2：本地/远程 tab 切换 → 转发给 main（写 settings.layout.view_mode + 存盘）。
    if let Some(v) = tb.toggle_view_mode {
        out.toggle_view_mode = Some(v);
    }
    if tb.open_settings {
        out.settings_opened = true;
    }
    if tb.open_shortcuts {
        // 直接带分类打开：下方 settings_opened 分支见已 open 不会再
        // 以默认分类重复初始化。
        st.settings.open_with_shortcuts(app_settings);
        out.settings_opened = true;
    }
    if tb.open_login {
        out.login_opened = true;
    }
    if tb.log_out {
        out.logged_out = true;
    }
    // M3.8 自绘标题栏：窗口控制信号转发
    if tb.drag_title_bar {
        out.drag_title_bar = true;
    }
    if tb.minimize_window {
        out.minimize_window = true;
    }
    if tb.toggle_maximize_window {
        out.toggle_maximize_window = true;
    }
    if tb.close_window {
        out.close_window = true;
    }
    if tb.show_window_menu_at.is_some() {
        out.show_window_menu_at = tb.show_window_menu_at;
    }
    // M3.8 批2：最大化按钮逻辑矩形（Snap Layouts 热区，main 换算为屏幕坐标）。
    if tb.maximize_btn_rect.is_some() {
        out.maximize_btn_rect = tb.maximize_btn_rect;
    }

    // 左侧会话栏（问题7：sidebar_visible 控制是否渲染）。
    // 隐藏时不画面板，侧栏宽度报 0（main 不更新 settings.layout.sidebar_width）。
    let grab = root.style().interaction.resize_grab_radius_side;
    let edge_rect = |edge_x: f32, root: &egui::Ui| {
        let y = root.available_rect_before_wrap().y_range();
        egui::Rect::from_x_y_ranges(edge_x - grab..=edge_x + grab, y)
    };
    // 远程设备列表（M5.2）：仅远程 tab 显示，置于会话栏左侧（第三列）。
    if app_settings.layout.view_mode {
        let rl_resp = egui::Panel::left("lumen_remote_list")
            .default_size(crate::settings::REMOTE_LIST_WIDTH_DEFAULT)
            .size_range(
                crate::settings::REMOTE_LIST_WIDTH_MIN..=crate::settings::REMOTE_LIST_WIDTH_MAX,
            )
            .resizable(true)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg_dark)
                    .inner_margin(egui::Margin::symmetric(8, 10)),
            )
            .show_inside(root, |ui| {
                remote_device_list_ui(
                    ui,
                    input.remote_devices,
                    input.active_device_id,
                    st,
                    pal,
                    &mut out,
                )
            });
        // 轮廓描边（四边，像素对齐）。
        {
            use egui::emath::GuiRounding as _;
            let ppp = root.pixels_per_point();
            let r = rl_resp.response.rect.round_to_pixels(ppp);
            let hw = 0.5 / ppp;
            let stroke = egui::Stroke::new(1.0 / ppp, pal.panel_outline);
            let p = root.painter();
            p.line_segment(
                [
                    egui::pos2(r.min.x + hw, r.min.y),
                    egui::pos2(r.min.x + hw, r.max.y),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(r.max.x - hw, r.min.y),
                    egui::pos2(r.max.x - hw, r.max.y),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(r.min.x, r.min.y + hw),
                    egui::pos2(r.max.x, r.min.y + hw),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(r.min.x, r.max.y - hw),
                    egui::pos2(r.max.x, r.max.y - hw),
                ],
                stroke,
            );
        }
        out.panel_resize_rects
            .push(edge_rect(rl_resp.response.rect.max.x, root));
    }

    if app_settings.layout.sidebar_visible {
        // 可拖宽（P10）。default_size 只在 egui 无面板记忆（首帧）时生效
        // = 还原持久化宽度；此后宽度由 egui 面板自管，实际值经
        // sidebar_width 报回 main，松手时写盘。
        let sb_resp = egui::Panel::left("lumen_sidebar")
            .default_size(app_settings.layout.sidebar_width)
            .size_range(crate::settings::SIDEBAR_WIDTH_MIN..=crate::settings::SIDEBAR_WIDTH_MAX)
            .resizable(true)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg_dark)
                    .inner_margin(egui::Margin::symmetric(8, 10)),
            )
            .show_inside(root, |ui| sidebar_ui(ui, input.tabs, st, pal, &mut out));
        out.sidebar_width = sb_resp.response.rect.width();
        // P16b：左侧会话栏轮廓描边——画左/右/上/下四边。共享边去重规则
        // （P16b 复验修正）：每对相邻面板的共享边由**左侧**面板画右边线，
        // 右侧面板不画左边线——侧栏画右（侧栏|文件树共享边）、文件树画右
        // （文件树|命令行区共享边）、两者均不画左。原「侧栏省右+文件树省左」
        // 把同一条共享边两侧都省了，中间反而没线（海风哥实测反馈）。
        // 像素对齐（1/ppp 逻辑点 = 1 物理像素），防分数 DPI 模糊。
        {
            use egui::emath::GuiRounding as _;
            let ppp = root.pixels_per_point();
            let r = sb_resp.response.rect.round_to_pixels(ppp);
            let hw = 0.5 / ppp; // 半像素：inner 边相当于向内缩半像素绘制
            let col = pal.panel_outline;
            let stroke = egui::Stroke::new(1.0 / ppp, col);
            let p = root.painter();
            // 左边
            p.line_segment(
                [
                    egui::pos2(r.min.x + hw, r.min.y),
                    egui::pos2(r.min.x + hw, r.max.y),
                ],
                stroke,
            );
            // 右边（侧栏|文件树共享边，由本侧负责画）
            p.line_segment(
                [
                    egui::pos2(r.max.x - hw, r.min.y),
                    egui::pos2(r.max.x - hw, r.max.y),
                ],
                stroke,
            );
            // 上边
            p.line_segment(
                [
                    egui::pos2(r.min.x, r.min.y + hw),
                    egui::pos2(r.max.x, r.min.y + hw),
                ],
                stroke,
            );
            // 下边
            p.line_segment(
                [
                    egui::pos2(r.min.x, r.max.y - hw),
                    egui::pos2(r.max.x, r.max.y - hw),
                ],
                stroke,
            );
        }
        // 侧栏右缘的拖宽手柄命中区（与面板边线同心、向两侧各探
        // resize_grab_radius_side）：报给 main 让 raw 鼠标按下让位——
        // 拖宽期间不交出终端焦点，调完宽度接着打字不断流。
        out.panel_resize_rects
            .push(edge_rect(sb_resp.response.rect.max.x, root));
    }

    // 中间一栏：文件树（可折叠 + 可拖宽 P10；树根跟随激活会话 cwd）。
    // 开合/拖宽改变终端区宽度，沿用同一条矩形变化链路。
    // M5.3 part3c-2：控制中+远程视图时改画**被控端 Option B 浏览树**（复用同一栏位/开合/宽度）；
    // 否则画本地树。`panel_width/rect` 两路同构，下方描边/拖宽手柄逻辑无需分支。
    // 片1 为占位桩（仅按 RootChanged 的 cwd 画占位）；片2 换成交互式浏览树。
    use crate::remote_ws::ClipSide;
    let (ft_panel_width, ft_panel_rect, ft_external_drop) = if input.remote_view_active {
        // 远程视图：一律画被控端 Option B 浏览树（只读渲染）。cwd 未到时画占位，绝不回落
        // 本地树（否则点击串扰控制端本机）。交互意图（展开点击 / 显示隐藏 / 复制粘贴）收集到
        // rout，由 main 闭包后以 &mut state.remote_ws 施加。远程树可粘贴 = 系统剪贴板有文件
        //（本地复制的文件→上传到被控端；本地复制现走系统剪贴板 CF_HDROP，与资源管理器互通）。
        let can_paste = crate::clipboard_files::has_files();
        // 是否正在控制设备（占位文案区分「未连接设备」/「等待 cwd」）。
        let controlling = input.remote_session.is_some_and(|sess| {
            matches!(sess.role, lumen_protocol::remote::Role::Controller)
        });
        let rout = filetree::show_remote(
            root,
            input.remote_filetree,
            st.filetree.visible,
            pal,
            app_settings.layout.filetree_width,
            can_paste,
            controlling,
        );
        out.filetree_width = rout.panel_width;
        out.remote_dir_clicks = rout.dir_clicks;
        out.remote_toggle_hidden = rout.toggle_hidden;
        out.remote_fetch_open = rout.fetch_open;
        out.remote_refresh_dir = rout.refresh_dir;
        out.remote_select = rout.select;
        out.filetree_hovered = rout.hovered;
        // 复制远程项 → 剪贴板 Remote 侧（下载源）；粘贴到远程目录 → Remote 目标（上传，片5）。
        out.file_copy = rout
            .copy_files
            .map(|(path, name, is_dir, size)| (ClipSide::Remote, path, name, is_dir, size));
        out.file_paste = rout.paste_into.map(|dir| (ClipSide::Remote, dir));
        (rout.panel_width, rout.panel_rect, None) // 远程树无拖放
    } else {
        // 本地树可粘贴 = 系统剪贴板有文件（资源管理器/Lumen 本地复制 → 本机复制到此目录）
        // 或 Lumen 内部有远程项（远程复制 → 下载到此目录）。粘贴方向在 main 按目标侧分派。
        let can_paste = crate::clipboard_files::has_files()
            || input.file_clipboard_side == Some(ClipSide::Remote);
        let ft = filetree::show(
            root,
            &mut st.filetree,
            input.cwd,
            input.shell_idle,
            pal,
            app_settings.layout.filetree_width,
            can_paste,
        );
        out.filetree_width = ft.panel_width;
        out.filetree_hovered = ft.hovered;
        out.cd_dir = ft.cd_dir;
        out.open_file = ft.open_file;
        out.copy_text = ft.copy_text;
        out.filetree_dialog_closed = ft.dialog_closed;
        // 复制本地项 → 剪贴板 Local 侧（上传源，片5）；粘贴到本地目录 → Local 目标（下载）。
        out.file_copy = ft.file_copy.map(|(path, is_dir)| {
            let name = path
                .file_name()
                .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            // 本地复制走系统剪贴板 CF_HDROP，不需要 size（恒 0）。
            (ClipSide::Local, path.display().to_string(), name, is_dir, 0)
        });
        out.file_paste = ft
            .file_paste_dir
            .map(|dir| (ClipSide::Local, dir.display().to_string()));
        if ft.busy_hint {
            st.toast
                .push(toast::ToastKind::Warn, crate::i18n::strings().shell_busy_cd);
        }
        for (kind, text) in ft.toasts {
            st.toast.push(kind, text);
        }
        (ft.panel_width, ft.panel_rect, ft.external_drop)
    };
    // part3c-2 #7：覆盖确认模态（粘贴检测到同名时，main 设 overwrite_conflict_count）。
    if let Some(count) = input.overwrite_conflict_count {
        out.overwrite_choice = overwrite_modal(root.ctx(), count, pal);
    }
    // P16b：文件树栏轮廓描边——只画右/上/下三边，省略左边。
    // 左边是与侧栏的共享边（侧栏右边 = 文件树左边），双画叠 2px；
    // 文件树可折叠为窄条、可拖宽，相邻关系动态变化时也始终不叠边。
    // 无 panel_rect（文件树完全隐藏）时跳过。
    if let Some(ft_rect) = ft_panel_rect {
        use egui::emath::GuiRounding as _;
        let ppp = root.pixels_per_point();
        let r = ft_rect.round_to_pixels(ppp);
        let hw = 0.5 / ppp;
        let col = pal.panel_outline;
        let stroke = egui::Stroke::new(1.0 / ppp, col);
        let p = root.painter();
        // 右边
        p.line_segment(
            [
                egui::pos2(r.max.x - hw, r.min.y),
                egui::pos2(r.max.x - hw, r.max.y),
            ],
            stroke,
        );
        // 上边
        p.line_segment(
            [
                egui::pos2(r.min.x, r.min.y + hw),
                egui::pos2(r.max.x, r.min.y + hw),
            ],
            stroke,
        );
        // 下边
        p.line_segment(
            [
                egui::pos2(r.min.x, r.max.y - hw),
                egui::pos2(r.max.x, r.max.y - hw),
            ],
            stroke,
        );
    }
    if ft_panel_width.is_some() {
        // 文件树右缘手柄探入终端区左缘数像素，必须让位（收起窄条不
        // 可拖宽，无手柄不让位）。
        out.panel_resize_rects
            .push(edge_rect(root.available_rect_before_wrap().min.x, root));
    }
    // 本地树的 cd_dir/open_file/copy_text/dialog/toasts 已在上面 else 分支消费；
    // 远程树（片1 占位桩）暂无这些产出，片2+ 在 if 分支收集 ListDir/Fetch/复制粘贴动作。

    // ── 底部状态栏（M4.1 批E）：Panel::bottom 必须在 CentralPanel 之前声明
    // （egui 面板布局：bottom/top > left/right > central，声明顺序决定剩余区域压缩方向）。
    // AltScreen 时 footer 隐藏（ComposerView::hidden），但状态栏仍保持可见以便
    // 用户随时知道当前模式（与 footer 逻辑独立）。
    #[cfg(feature = "input-editor")]
    {
        let sb_resp = egui::Panel::bottom("lumen_statusbar")
            .exact_size(statusbar::HEIGHT)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg_dark)
                    .inner_margin(egui::Margin::symmetric(0, 0)),
            )
            .show_inside(root, |ui| {
                let sb_out =
                    statusbar::show(ui, input.input_mode, input.cwd, input.force_fallback, pal);
                if sb_out.toggle_fallback {
                    out.toggle_fallback = true;
                }
            });
        let _ = sb_resp;
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show_inside(root, |ui| {
            // 终端区与各窗格矩形对齐到物理像素：分数 DPI（如 125%）下
            // 面板布局出的逻辑矩形换算物理像素可为分数，而离屏纹理
            // 尺寸只能取整数——呈现 quad 与纹理像素数不等时 Nearest
            // 采样会在区中部复制/丢一行 texel（1px 接缝、半像素错位）。
            // 这里先取整再布置 Image，main.rs 用同一矩形换算纹理尺寸
            // 与窗格像素矩形，保证三者同源、采样 1:1。
            use egui::emath::GuiRounding as _;
            let ppp = ui.pixels_per_point();
            let area = ui.available_rect_before_wrap().round_to_pixels(ppp);
            out.term_rect = area;

            // M5.3 part3b：控制端镜像——终端工作区改画远端镜像纹理（Middle 层叠在本地
            // 窗格之上，铺满 area）。本地窗格仍在下方渲染，被遮盖即可。纹理内容由 main
            // 的窗格渲染段画好（wgpu 上色）；纹理已是终端尺寸，按 area 铺满。
            if let Some(tex) = input.remote_mirror_tex {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("lumen_remote_mirror"),
                ));
                painter.image(
                    tex,
                    area,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }

            // 背景图（P13）：在终端工作区整体底部绘制，绘制发生在任何
            // 窗格内容之前（窗格标题栏/分隔线在循环内稍后盖住背景图）。
            // 侧栏/文件树/顶栏在各自面板内，不受影响。
            // cover 模式：保宽高比居中裁剪填满 area。
            // opacity 控制不透明度；dim>0 时额外叠一层半透明黑蒙层。
            if let Some(bg) = &input.bg_image {
                let (uv_min, uv_max) = crate::background::cover_uv(
                    bg.width as f32,
                    bg.height as f32,
                    area.width(),
                    area.height(),
                );
                let tint = egui::Color32::from_rgba_unmultiplied(
                    255,
                    255,
                    255,
                    // opacity ∈ [0.05,1.0] → alpha ∈ [13,255]
                    (bg.opacity.clamp(0.05, 1.0) * 255.0).round() as u8,
                );
                ui.painter().image(
                    bg.texture_id,
                    area,
                    egui::Rect::from_min_max(
                        egui::pos2(uv_min[0], uv_min[1]),
                        egui::pos2(uv_max[0], uv_max[1]),
                    ),
                    tint,
                );
                // 额外暗化蒙层（dim > 0）：叠一层半透明黑，增强文字可读性。
                if bg.dim > 0.0 {
                    let dim_alpha = (bg.dim.clamp(0.0, 0.9) * 255.0).round() as u8;
                    ui.painter()
                        .rect_filled(area, 0.0, egui::Color32::from_black_alpha(dim_alpha));
                }
            }

            // P16b：命令行区（CentralPanel 整体）轮廓描边——只画右/上/下三边，
            // 省略左边（左边是与文件树右边的共享边，文件树侧已画）。
            //
            // 【层级修复（P16b 问题3）】原来在 area 赋值后立即画，此后 ui.put
            // 的终端纹理图像会把它盖掉。现在改用 Foreground painter：Foreground
            // 层在 egui 布局完整帧结束后统一叠在最上，绝对不会被面板内容遮住。
            // 同时与焦点窗格 accent 边框（同为 Foreground）共层但 panel_outline
            // 先画、accent 后画（循环内），accent 会盖住 panel_outline 同像素
            // 处——视觉层次符合预期：轮廓 < 焦点 accent。
            // （描边在闭包末尾用 Foreground painter 绘制，见下方）
            // 分屏布局（F5/F7③）：网格结构固定、比例可调（权重挂
            // Tab，main 传入本帧快照）；布局与窗格数不符（防御：结构
            // 刚变更的过渡帧）时退回均分。
            let uniform_fallback;
            let lay = if input.layout.pane_count() == input.panes.len() {
                &input.layout
            } else {
                uniform_fallback = layout::PaneLayout::uniform(input.panes.len());
                &uniform_fallback
            };
            // 最大化（P14）：该窗格独占整个终端工作区，其余窗格矩形
            // 置 NOTHING 占位（与 panes 保持同序对位，main 按下标跳过
            // 隐藏窗格的矩形应用/渲染/鼠标路由）。下标防御过滤。
            let maximized = input.maximized.filter(|&m| m < input.panes.len());
            let rects = match maximized {
                Some(m) => {
                    let mut v = vec![egui::Rect::NOTHING; input.panes.len()];
                    v[m] = area;
                    v
                }
                None => lay.pane_rects(area),
            };
            // 标题栏拖动换位的本帧状态（F7②）：拖动中 (源下标, 指针
            // 位置) 驱动视觉反馈；松手 (源下标, 落点) 做换位判定。
            // 在窗格循环里采集、循环后统一处理（落点判定需要全部
            // 整格矩形）。
            let mut title_drag: Option<(usize, egui::Pos2)> = None;
            let mut title_drop: Option<(usize, egui::Pos2)> = None;
            for (i, (pane, rect)) in input.panes.iter().zip(&rects).enumerate() {
                // 最大化期间的隐藏窗格（P14）：不画、矩形 NOTHING 占位
                // 保持与 panes 的下标对位（main 据此跳过其矩形应用与
                // 渲染——后台照常消化输出，同「非激活 tab」闸门）。
                //
                // 【契约·勿改】main.rs 的 resize 循环（6029-6055）以「矩形
                // 退化与否」（NOTHING/非有限/宽高 < 1pt）为**唯一**判据跳过
                // 隐藏窗格，刻意**不读**实时 maximized 状态——因为点标题栏
                // 「还原」按钮会在同一帧 run_ui 内把 maximized 改回 None
                // （shell_out.pane_maximize→toggle_maximize_pane），而本帧
                // pane_rects 仍是改前的最大化布局。故此处隐藏窗格**必须**
                // 产退化的 NOTHING；绝不可改成 0 尺寸或极小的「真」矩形，
                // 否则会绕过那道 guard，重新引入「还原帧把隐藏窗格 resize
                // 成 1 列、每行截断丢内容」的串扰 bug（海风哥实测过）。
                if maximized.is_some_and(|m| m != i) {
                    out.pane_rects.push(egui::Rect::NOTHING);
                    continue;
                }
                let rect = rect.round_to_pixels(ppp);
                // —— 窗格标题栏（F7①）：顶部窄条，左标题右 ✕，占高从
                // 终端内容区扣除（行数相应减少，沿用矩形→resize 链
                // 路）。单窗格也显示（裁决：保持形态一致、关闭入口
                // 常驻，且是批次2 拖动换位的抓手——Warp 本尊单格顶部
                // 也有 pane 条）。极矮窗格防御：标题栏最多占一半高。
                let title_h = PANE_TITLE_HEIGHT.min(rect.height() / 2.0);
                let title_rect = egui::Rect::from_min_max(
                    rect.min,
                    egui::pos2(rect.max.x, rect.min.y + title_h),
                )
                .round_to_pixels(ppp);
                let content_rect =
                    egui::Rect::from_min_max(egui::pos2(rect.min.x, title_rect.max.y), rect.max);
                // 焦点窗格标题栏提亮一档（底 btn_bg/文字 fg vs 底
                // bg_dark/文字 fg_dim），作为 accent 边框外的焦点指示
                // 补充。
                let (bar_bg, bar_fg) = if pane.focused {
                    (pal.btn_bg, pal.fg)
                } else {
                    (pal.bg_dark, pal.fg_dim)
                };
                ui.painter().rect_filled(title_rect, 0.0, bar_bg);

                // —— 标题栏右侧按钮区：仅多窗格显示（✕ 关闭 + 最大化/还原）。
                // 单窗格无需关闭（关闭 = 关整个 tab，走 Ctrl+Shift+W）、也无
                // 最大化语义，整个按钮区省去、标题占满到右端（海风哥
                // 2026-06-13，推翻 F7① 单窗格也显示 ✕ 的旧裁决）。✕ 与最大化
                // 均画线（不赌字体覆盖）；命中矩形进 pane_close_rects 让 raw
                // 鼠标路由让位（点击不聚焦/不建选区）。
                let mut title_end = title_rect.max.x - 8.0;
                if input.panes.len() > 1 {
                    // ✕ 关闭按钮（标题栏右端）。
                    let close_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            title_rect.max.x - 4.0 - PANE_CLOSE_SIZE / 2.0,
                            title_rect.center().y,
                        ),
                        egui::vec2(PANE_CLOSE_SIZE, PANE_CLOSE_SIZE),
                    );
                    let cresp = ui.interact(
                        close_rect,
                        ui.id().with(("pane_close", i)),
                        egui::Sense::click(),
                    );
                    {
                        let painter = ui.painter();
                        let c = close_rect.center();
                        if cresp.hovered() {
                            painter.circle_filled(c, PANE_CLOSE_SIZE / 2.0, pal.bg_highlight);
                        }
                        let r = 3.5;
                        let stroke =
                            egui::Stroke::new(1.2, if cresp.hovered() { pal.fg } else { bar_fg });
                        painter.line_segment(
                            [egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)],
                            stroke,
                        );
                        painter.line_segment(
                            [egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)],
                            stroke,
                        );
                    }
                    if cresp
                        .on_hover_text(crate::i18n::strings().pane_close_tip)
                        .clicked()
                    {
                        out.pane_close = Some(i);
                    }
                    out.pane_close_rects.push(close_rect);

                    // 最大化/还原按钮（P14）：✕ 左侧。普通态画 ▢（最大化），
                    // 最大化态画 ⧉（双矩形错位，还原）。
                    let max_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            close_rect.min.x - 4.0 - PANE_CLOSE_SIZE / 2.0,
                            title_rect.center().y,
                        ),
                        egui::vec2(PANE_CLOSE_SIZE, PANE_CLOSE_SIZE),
                    );
                    let mresp = ui.interact(
                        max_rect,
                        ui.id().with(("pane_maximize", i)),
                        egui::Sense::click(),
                    );
                    {
                        let painter = ui.painter();
                        let c = max_rect.center();
                        if mresp.hovered() {
                            painter.circle_filled(c, PANE_CLOSE_SIZE / 2.0, pal.bg_highlight);
                        }
                        let stroke =
                            egui::Stroke::new(1.2, if mresp.hovered() { pal.fg } else { bar_fg });
                        if maximized.is_some() {
                            // 还原图标 ⧉：错位双矩形——后框只画露出的
                            // 上右两边，前框完整（前框底色填充挡住后框
                            // 重叠段会连 hover 圆底一起盖掉，故改画线）。
                            let r = 2.5;
                            let back = egui::Rect::from_center_size(
                                c + egui::vec2(1.5, -1.5),
                                egui::vec2(2.0 * r, 2.0 * r),
                            );
                            painter.line_segment(
                                [
                                    egui::pos2(back.min.x + 2.0, back.min.y),
                                    egui::pos2(back.max.x, back.min.y),
                                ],
                                stroke,
                            );
                            painter.line_segment(
                                [
                                    egui::pos2(back.max.x, back.min.y),
                                    egui::pos2(back.max.x, back.max.y - 2.0),
                                ],
                                stroke,
                            );
                            let front = egui::Rect::from_center_size(
                                c + egui::vec2(-1.0, 1.0),
                                egui::vec2(2.0 * r, 2.0 * r),
                            );
                            painter.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
                        } else {
                            // 最大化图标 ▢：单矩形描边。
                            let r = 3.5;
                            painter.rect_stroke(
                                egui::Rect::from_center_size(c, egui::vec2(2.0 * r, 2.0 * r)),
                                0.0,
                                stroke,
                                egui::StrokeKind::Middle,
                            );
                        }
                    }
                    let tip = {
                        let s = crate::i18n::strings();
                        if maximized.is_some() {
                            s.pane_restore_tip
                        } else {
                            s.pane_maximize_tip
                        }
                    };
                    if mresp.on_hover_text(tip).clicked() {
                        out.pane_maximize = Some(i);
                    }
                    out.pane_close_rects.push(max_rect);
                    title_end = max_rect.min.x - 4.0;
                }

                // 标题：左侧单行截断展示；点击标题栏 = 聚焦该窗格，
                // 拖动标题栏 = 拖起整个窗格换位（F7①②）。点击与拖动
                // 的仲裁交给 egui 现成语义：按下后移动不超阈值（6 逻辑
                // px）且未长按算点击，超出才算拖——clicked 与 dragged
                // 互斥，不会一次手势两个动作都触发。悬停展示完整 cwd
                // （截断时可看全）。
                let title_hit = egui::Rect::from_min_max(
                    title_rect.min,
                    egui::pos2(title_end, title_rect.max.y),
                );
                let text_rect = egui::Rect::from_min_max(
                    egui::pos2(title_hit.min.x + 8.0, title_hit.min.y),
                    title_hit.max,
                );
                // 窗格标题栏（需求2）：重命名中画行内 TextEdit，否则画
                // 标题 Label（点击聚焦 / 双击进重命名 / 右键菜单重命名 /
                // 拖动换位）。镜像侧栏标签重命名机制（见 sidebar_ui），
                // 窗格用下标 i 定位（侧栏用会话 id）。
                let is_pane_renaming =
                    st.pane_renaming.as_ref().is_some_and(|(id, _)| *id == pane.id);
                if is_pane_renaming {
                    // 行内编辑框替代标题：Enter 提交、Esc 或点击别处取消
                    // （三种情况 TextEdit 都失焦）。缓冲取自 st.pane_renaming
                    // （pane 是不可变借用，不能改 pane.title）。
                    if let Some((_, buf)) = st.pane_renaming.as_mut() {
                        let mut edit_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(text_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        let resp = edit_ui
                            .add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
                        if st.pane_rename_focus {
                            resp.request_focus();
                            st.pane_rename_focus = false;
                        }
                        if resp.lost_focus() {
                            // 与侧栏 tab 重命名同构：键盘（Enter/Esc）结束才让
                            // main 把焦点还终端；点击别处取消尊重鼠标仲裁。
                            let by_key = ui.input(|inp| {
                                inp.key_pressed(egui::Key::Enter)
                                    || inp.key_pressed(egui::Key::Escape)
                            });
                            if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                                // 空名 = 清除自定义名（main 据空串回退默认标题）。
                                out.pane_rename = Some((pane.id, buf.trim().to_owned()));
                            }
                            out.pane_rename_ended_by_key = by_key;
                            st.pane_renaming = None;
                        }
                    }
                } else {
                    let mut title_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(text_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    title_ui.add(
                        egui::Label::new(
                            egui::RichText::new(&pane.title).size(12.0).color(bar_fg),
                        )
                        .truncate()
                        .selectable(false),
                    );
                    let tresp = ui.interact(
                        title_hit,
                        ui.id().with(("pane_title", i)),
                        egui::Sense::click_and_drag(),
                    );
                    if tresp.clicked() {
                        out.pane_clicked = Some(i);
                        out.term_clicked = true;
                    }
                    // 双击标题进重命名（需求2）：以当前显示标题为编辑初值，
                    // 下一帧把焦点交给编辑框。
                    if tresp.double_clicked() {
                        st.pane_renaming = Some((pane.id, pane.title.clone()));
                        st.pane_rename_focus = true;
                    }
                    // 右键菜单「重命名」（需求2，复用 i18n menu_rename）。
                    // context_menu 取 &self，放在下方 on_hover_text（移动
                    // tresp）之前。
                    tresp.context_menu(|ui| {
                        if ui.button(crate::i18n::strings().menu_rename).clicked() {
                            st.pane_renaming = Some((pane.id, pane.title.clone()));
                            st.pane_rename_focus = true;
                            ui.close();
                        }
                    });
                    // 拖动换位（F7②）：仅多窗格时有交换对象；最大化期间
                    // 只剩一格可见、无落点，禁用（P14）。悬停标题栏给
                    // Grab 光标提示可拖；拖动中/松手的状态循环后处理。
                    if input.panes.len() > 1 && maximized.is_none() {
                        if tresp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                        }
                        if tresp.dragged() {
                            if let Some(p) = tresp.interact_pointer_pos() {
                                title_drag = Some((i, p));
                            }
                        } else if tresp.drag_stopped() {
                            if let Some(p) = tresp.interact_pointer_pos() {
                                title_drop = Some((i, p));
                            }
                        }
                    }
                    if let Some(path) = &pane.title_hover {
                        tresp.on_hover_text(path.clone());
                    }
                }

                // 终端内容区：离屏纹理 + 点击聚焦。选区/块点击/滚轮仍
                // 走 window_event 按**内容矩形**路由（见 main.rs）——
                // 标题栏不在其中，标题栏上的鼠标事件不进终端。
                if let Some(tex) = pane.tex {
                    ui.put(
                        content_rect,
                        egui::Image::new(egui::load::SizedTexture::new(tex, content_rect.size())),
                    );
                }
                let resp = ui.interact(
                    content_rect,
                    ui.id().with(("pane", i)),
                    egui::Sense::click(),
                );
                if resp.clicked() {
                    out.pane_clicked = Some(i);
                    out.term_clicked = true;
                }
                // 焦点窗格指示：多窗格时画 1px accent 边框（整格含
                // 标题栏；M3.7b 起 accent = 纯白/近黑；单窗格不画，
                // 满屏边框只是视觉噪音——最大化态同理，P14）。
                if pane.focused && input.panes.len() > 1 && maximized.is_none() {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                // 窗格矩形 = 终端内容区（标题栏已扣除）：main 据此重建
                // 离屏纹理 / resize / 路由鼠标与 IME。
                out.pane_rects.push(content_rect);
            }

            // —— 分隔条（F7③ + P11 一体）：可视 1px 灰阶细线 + 加宽
            // 命中区。hover/拖动变 resize 光标；拖动把相邻两格/两排的
            // 边界拖到指针处（实时生效，main 施加权重）；双击恢复该
            // 方向均分。命中区比可视线宽（≥8 逻辑点且 ≥6 物理像素）
            // 便于抓取，向两侧压住窗格边缘几个像素——interact 注册
            // 晚于窗格（同层后注册者在上），按下优先归分隔条；raw
            // 鼠标路由的让位见 main.rs（divider_rects_px）。
            // 最大化期间分隔条不显示不可拖（P14：只剩一格，无比例可
            // 调；divider_rects 留空 = raw 鼠标无让位区）。
            let dividers = if maximized.is_some() {
                Vec::new()
            } else {
                lay.dividers(area)
            };
            let hit_w = 8.0f32.max(6.0 / ppp);
            for (di, div) in dividers.iter().enumerate() {
                let vertical = matches!(div.kind, layout::DividerKind::Col { .. });
                let hit = if vertical {
                    div.rect
                        .expand2(egui::vec2((hit_w - div.rect.width()).max(0.0) / 2.0, 0.0))
                } else {
                    div.rect
                        .expand2(egui::vec2(0.0, (hit_w - div.rect.height()).max(0.0) / 2.0))
                };
                let resp = ui.interact(
                    hit,
                    ui.id().with(("pane_divider", di)),
                    egui::Sense::click_and_drag(),
                );
                let icon = if vertical {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                };
                // 拖动中指针可能滑出命中区：dragged 期间也保持光标。
                if resp.hovered() || resp.dragged() {
                    ui.ctx().set_cursor_icon(icon);
                }
                if resp.double_clicked() {
                    out.divider_reset = Some(div.kind);
                } else if resp.dragged() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        out.divider_drag = Some((div.kind, p));
                    }
                }
                if resp.drag_stopped() {
                    out.divider_drag_ended = true;
                }
                // 可视线：1 物理像素，居中于间隙、像素对齐（色值取
                // 黑白色板的分隔档 bg_highlight，P11）。
                let line = if vertical {
                    egui::Rect::from_center_size(
                        div.rect.center(),
                        egui::vec2(1.0 / ppp, div.rect.height()),
                    )
                } else {
                    egui::Rect::from_center_size(
                        div.rect.center(),
                        egui::vec2(div.rect.width(), 1.0 / ppp),
                    )
                };
                ui.painter()
                    .rect_filled(line.round_to_pixels(ppp), 0.0, pal.bg_highlight);
                out.divider_rects.push(hit);
            }

            // —— 标题栏拖动换位（F7②）——拖动中：悬停目标窗格画
            // accent 高亮（2px 内描边 + 8% 半透明蒙层盖整格），指针
            // 右下跟一张半透明标题小卡（Foreground 层，不受面板裁
            // 剪）；落点判定用**整格矩形**（含标题栏，rects 非内容
            // 矩形）。松手落在其他窗格 → 产出 pane_swap（main 交换
            // panes 下标，权重不动）；落在源格/窗格外 → 取消无副作用。
            if let Some((src, pos)) = title_drag {
                if let Some(dst) = swap_target(&rects, src, pos) {
                    ui.painter().rect(
                        rects[dst].round_to_pixels(ppp),
                        0.0,
                        pal.accent.gamma_multiply(0.08),
                        egui::Stroke::new(2.0, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                // 跟手浮层：源窗格标题的圆角小卡（半透明，让得出底下
                // 的目标高亮），画在 Foreground 层盖过一切面板内容。
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("pane_title_drag_overlay"),
                ));
                let galley = painter.layout_no_wrap(
                    input.panes[src].title.clone(),
                    egui::FontId::proportional(12.0),
                    pal.fg,
                );
                let pad = egui::vec2(10.0, 5.0);
                let chip = egui::Rect::from_min_size(
                    pos + egui::vec2(14.0, 10.0),
                    galley.size() + pad * 2.0,
                );
                painter.rect(
                    chip,
                    4.0,
                    pal.bg_panel.gamma_multiply(0.85),
                    egui::Stroke::new(1.0, pal.bg_highlight),
                    egui::StrokeKind::Inside,
                );
                painter.galley(chip.min + pad, galley, pal.fg);
                // 拖动中指针可能滑出标题栏命中区：保持抓取光标。
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            if let Some((src, pos)) = title_drop {
                out.pane_swap = swap_target(&rects, src, pos).map(|dst| (src, dst));
                // 拖动结束（无论换位还是取消）键盘焦点交还终端：按下
                // 落在标题栏时 raw 路由按「点击面板」交出过焦点，这里
                // 收回——拖完接着打字不该断流。
                out.term_clicked = true;
            }

            // P16b：命令行区（CentralPanel 整体）轮廓描边——右/上/下三边。
            // 用 **Middle 层** painter：高于 Background（终端纹理贴图、窗格标题
            // 栏等都在 Background，故描边盖在其上、可见——P16b 问题3 要求），但
            // **低于 Foreground**（toast / 头像菜单 / 各弹窗 / 模态框都是
            // Foreground），故描边不再盖在这些弹窗之上（海风哥 2026-06-14：底部
            // 描边线显示在弹窗上面——下边线在终端区/状态栏边界，原 Foreground 与
            // toast 同层冲突）。不画左边：文件树栏右边线即为本区左边线。
            {
                let fg_painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("central_panel_outline"),
                ));
                let hw = 0.5 / ppp;
                let col = pal.panel_outline;
                let stroke = egui::Stroke::new(1.0 / ppp, col);
                // 右边
                fg_painter.line_segment(
                    [
                        egui::pos2(area.max.x - hw, area.min.y),
                        egui::pos2(area.max.x - hw, area.max.y),
                    ],
                    stroke,
                );
                // 上边
                fg_painter.line_segment(
                    [
                        egui::pos2(area.min.x, area.min.y + hw),
                        egui::pos2(area.max.x, area.min.y + hw),
                    ],
                    stroke,
                );
                // 下边
                fg_painter.line_segment(
                    [
                        egui::pos2(area.min.x, area.max.y - hw),
                        egui::pos2(area.max.x, area.max.y - hw),
                    ],
                    stroke,
                );
            }
        });

    // 文件树节点拖放的落点判定：要等 CentralPanel 布局出本帧窗格
    // 矩形，故放在面板之后。落在某窗格 → 请求把路径插入**该窗格**
    // 的命令行（F5 批2：目标 = 鼠标落点所在窗格，main 会先聚焦它）；
    // 落在别处（侧栏/树内回弹/窗格间隙）→ 静默忽略。
    if let Some((path, pos)) = ft_external_drop {
        if let Some(pi) = out.pane_rects.iter().position(|r| r.contains(pos)) {
            out.insert_path = Some((pi, path));
        }
    }

    // —— 设置覆盖层（盖住三栏；终端在其下照常消化输出与渲染）——
    // 齿轮按钮本帧点击 → 立即打开（同帧呈现，避免一帧裸跳）。
    if out.settings_opened && !st.settings.open {
        st.settings.open_with(app_settings);
    }
    if st.settings.open {
        let s_out = settings_ui::show(
            root.ctx(),
            &mut st.settings,
            app_settings,
            input.profile,
            pal,
            input.os_dark,
        );
        out.settings_font_changed = s_out.font_changed;
        out.settings_theme_changed = s_out.theme_changed;
        out.settings_background_params_changed = s_out.background_params_changed;
        out.settings_background_image_changed = s_out.background_image_changed;
        out.settings_language_changed = s_out.language_changed;
        out.update_check_now = s_out.update_check_now;
        out.settings_update_changed = s_out.update_changed;
        out.settings_proxy_changed = s_out.proxy_changed;
        out.settings_server_url_changed = s_out.server_url_changed;
        if s_out.log_out {
            out.logged_out = true;
        }
        if s_out.open_login {
            // Account 的 Log in：登录卡片叠在设置页之上（后绘制者在
            // 上层），登录成功后 Account 即时显示已登录态。
            out.login_opened = true;
        }
        if s_out.closed {
            st.settings.open = false;
            out.settings_closed = true;
        }
    }

    // —— 登录覆盖层（最后绘制 = 盖在设置页之上）——
    // 入口：头像菜单 Log in / 设置页 Account 的 Log in，本帧点击
    // 立即打开（同帧呈现）。
    if out.login_opened && !st.login.open {
        st.login.open_clean();
    }
    if st.login.open {
        let l_out = login_ui::show(root.ctx(), &mut st.login, pal);
        if l_out.logged_in.is_some() {
            out.logged_in = l_out.logged_in;
        }
        if l_out.closed {
            st.login.open = false;
            out.login_closed = true;
        }
    }

    // —— M5.3 远程控制 UI：配对模态（控制端）+ 顶部状态横幅（被控/控制）——
    if let Some(prompt) = input.remote_pairing {
        let r_out = remote_ui::pairing_modal(root.ctx(), &mut st.remote_ui, prompt, pal);
        if let Some(code) = r_out.submit_code {
            out.submit_pairing_code = Some(code);
        }
        if r_out.cancel_pairing {
            out.cancel_pairing = true;
        }
    } else {
        // 无待配对：复位输入缓冲（被拒/成功/取消后回到干净态）。
        st.remote_ui.reset();
    }
    {
        let b_out = remote_ui::banner(root.ctx(), input.remote_incoming, input.remote_session, pal);
        if b_out.decline {
            out.decline_control = true;
        }
        if b_out.end_session {
            out.end_remote_session = true;
        }
    }

    // —— 补全弹窗（M4.4 批1 Tab；锚定小浮层，盖在设置/登录之上，toast 之下）——
    // completion_view 由 main 每帧传入；Some = 显示弹窗，None = 不显示。
    if let Some(cv) = &input.completion_view {
        let c_out = completion_ui::show(root.ctx(), &mut st.completion, cv, pal);
        if let Some(idx) = c_out.accept {
            out.completion_accept = Some(idx);
        }
        if c_out.closed {
            out.completion_closed = true;
        }
    }

    // —— 历史搜索面板（M4.3 Ctrl+R；盖在设置/登录之上，toast 之下）——
    // 面板 open 时调用；产出写入 ShellOutput 对应字段。
    if st.history_search.open {
        let hs_out =
            history_search_ui::show(root.ctx(), &mut st.history_search, input.history_rows, pal);
        if hs_out.accept.is_some() {
            out.history_accept = hs_out.accept;
        }
        if hs_out.closed {
            st.history_search.open = false;
            out.history_closed = true;
        }
        if hs_out.query_changed {
            out.history_query_changed = true;
        }
    }

    // —— 系统提示浮层（最后绘制 = 叠在一切覆盖层之上）——
    toast::show(root.ctx(), &mut st.toast, pal);
    out
}

/// part3c-2 #7 覆盖确认模态：粘贴检测到同名时弹（仿 `dialog_ui` 删除确认）。返回用户选择
/// （`None` = 仍在等待）；点背景 / Esc 视为取消。
fn overwrite_modal(
    ctx: &egui::Context,
    conflict_count: usize,
    pal: &theme::Palette,
) -> Option<OverwriteChoice> {
    let s = crate::i18n::strings();
    let mut choice = None;
    let frame = egui::Frame::new()
        .fill(pal.bg_panel)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(16));
    let modal = egui::Modal::new(egui::Id::new("lumen_overwrite_modal"))
        .backdrop_color(egui::Color32::from_black_alpha(120))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(
                egui::RichText::new(crate::i18n::fmt1(s.remote_overwrite_prompt_fmt, conflict_count))
                    .size(13.0)
                    .color(pal.fg),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                // 覆盖 = 危险操作：语义红实底 + 反相文字（同删除确认按钮范式）。
                let ow = egui::Button::new(
                    egui::RichText::new(s.remote_overwrite_overwrite).color(pal.accent_fg),
                )
                .fill(pal.error);
                if ui.add(ow).clicked() {
                    choice = Some(OverwriteChoice::Overwrite);
                }
                if ui.button(s.remote_overwrite_skip).clicked() {
                    choice = Some(OverwriteChoice::Skip);
                }
                if ui.button(s.filetree_cancel_btn).clicked() {
                    choice = Some(OverwriteChoice::Cancel);
                }
            });
        });
    if choice.is_none() && modal.should_close() {
        choice = Some(OverwriteChoice::Cancel); // 点背景 / Esc = 取消。
    }
    choice
}

/// 标题栏拖动换位的落点判定（F7②）：指针落在哪个窗格（`rects` =
/// 整格矩形，含标题栏）。落在源格自身或所有窗格之外返回 None
/// （取消，无副作用）。
fn swap_target(rects: &[egui::Rect], src: usize, pos: egui::Pos2) -> Option<usize> {
    rects
        .iter()
        .position(|r| r.contains(pos))
        .filter(|&dst| dst != src)
}

/// 侧栏内容：顶部标题栏条（「会话」标签 + 小「＋」按钮）+ tab 条目列表（可滚动）。
///
/// R8 改造：
/// - 删除底部「⚙ 设置」按钮（设置入口=头像菜单 Settings + Ctrl+,）
/// - 删除底部「＋ 新建会话」按钮
/// - 顶部新增标题栏条：左「会话」标签 + 右小「＋」按钮（tooltip 三语，Ctrl+T）
///   → 点击=新建会话信号（复用原底部按钮信号路径）
///
/// R9 改造：
/// - tab 条目列表用 ScrollArea::vertical() 包裹（会话多时可上下滚动）
/// - ScrollArea id 固定（"sidebar_tab_scroll"），右键菜单/重命名/拖拽 id 仍绑各条目 entry.id
fn sidebar_ui(
    ui: &mut egui::Ui,
    tabs: &[TabItem],
    st: &mut ShellState,
    pal: &theme::Palette,
    out: &mut ShellOutput,
) {
    let s = crate::i18n::strings();

    // ── 顶部标题栏条（高 30px，与文件树工具条风格对齐）──────────────────
    {
        // 分配 30px 高的整行区域，内用 left_to_right 布局
        let (title_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
        let mut title_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(title_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // 左侧「会话」标签
        title_ui.add(
            egui::Label::new(
                egui::RichText::new(s.sidebar_sessions)
                    .size(12.0)
                    .color(pal.fg_dim),
            )
            .selectable(false),
        );
        // 右侧「＋」按钮（小图标，painter 画 12×12 十字，hover 圆角底）
        // 先用 RTL 子布局把按钮推到右端
        let remaining = title_ui.available_rect_before_wrap();
        let btn_size = egui::vec2(22.0, 22.0);
        let btn_rect = egui::Rect::from_min_size(
            egui::pos2(
                remaining.max.x - btn_size.x,
                remaining.center().y - btn_size.y / 2.0,
            ),
            btn_size,
        );
        let btn_resp = title_ui.interact(
            btn_rect,
            title_ui.id().with("sidebar_new_session_plus"),
            egui::Sense::click(),
        );
        // 绘制圆角底 hover 效果
        if btn_resp.hovered() {
            title_ui
                .painter()
                .rect_filled(btn_rect, 4.0, pal.bg_highlight);
        }
        // 画 + 号（12×12，线宽 1.2）
        {
            let painter = title_ui.painter();
            let c = btn_rect.center();
            let r = 5.0_f32; // 半径 5 → 总长 10
            let fg = if btn_resp.hovered() {
                pal.fg
            } else {
                pal.fg_dim
            };
            let stroke = egui::Stroke::new(1.2, fg);
            painter.line_segment([egui::pos2(c.x - r, c.y), egui::pos2(c.x + r, c.y)], stroke);
            painter.line_segment([egui::pos2(c.x, c.y - r), egui::pos2(c.x, c.y + r)], stroke);
        }
        if btn_resp.on_hover_text(s.sidebar_new_session_tip).clicked() {
            out.new_session = true;
        }
    }
    ui.add_space(2.0);

    // R9：会话列表包 ScrollArea，占满标题栏以下全部剩余高度。
    // auto_shrink([false, false]) 保证面板未填满时 ScrollArea 也撑满，
    // 避免 egui 把剩余空白分配给外层 ui 导致列表区域缩水。
    // id 固定（"sidebar_tab_scroll"）保证 scroll offset 跨帧持久，
    // 与条目 entry.id 无耦合，重命名/右键菜单/拖拽的 egui id 仍绑各自 entry.id。
    egui::ScrollArea::vertical()
        .id_salt("sidebar_tab_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for entry in tabs {
                // 重命名中的条目：行内编辑框替代按钮。Enter 提交、Esc 或
                // 点击别处取消（egui 的 TextEdit 在这三种情况都会失焦）。
                let is_renaming = st.renaming.as_ref().is_some_and(|(id, _)| *id == entry.id);
                if is_renaming {
                    if let Some((_, buf)) = st.renaming.as_mut() {
                        let resp =
                            ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
                        if st.rename_focus {
                            resp.request_focus();
                            st.rename_focus = false;
                        }
                        if resp.lost_focus() {
                            // 区分结束方式：键盘（Enter 提交 / Esc 取消）结束时
                            // main 把焦点还给终端；点击别处取消则不还——那次
                            // 点击已按鼠标仲裁决定了焦点归属（点终端区 true、
                            // 点面板/头像 false），强行翻回会让悬浮菜单开着时
                            // 键盘直通 PTY。
                            let by_key = ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)
                            });
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                out.rename = Some((entry.id, buf.trim().to_owned()));
                            }
                            out.rename_ended_by_key = by_key;
                            st.renaming = None;
                        }
                    }
                    continue;
                }

                // 两行条目（F7②样式）：左终端图标 + 名称行 + 路径行。
                // 激活=selection 底（控件梯度最高档，一眼可辨），悬停=
                // bg_highlight；圆角 2（海风哥 2026-06-14：6 太大）。
                const ROW_H: f32 = 46.0;
                const ICON_COL: f32 = 30.0;
                let (rect, mut resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_H),
                    egui::Sense::click(),
                );
                let bg = if entry.active {
                    pal.selection
                } else if resp.hovered() {
                    pal.bg_highlight
                } else {
                    egui::Color32::TRANSPARENT
                };
                if bg != egui::Color32::TRANSPARENT {
                    ui.painter().rect_filled(rect, 2.0, bg);
                }
                // 左侧图标（垂直居中于行）：有真实程序图标则画纹理，
                // 否则回退自绘终端字形。
                let icon_center = egui::pos2(rect.left() + ICON_COL / 2.0, rect.center().y);
                if let Some(tex) = entry.icon {
                    let img_rect = egui::Rect::from_center_size(icon_center, egui::vec2(20.0, 20.0));
                    ui.painter().image(
                        tex,
                        img_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                } else {
                    draw_session_icon(ui, icon_center, entry.active, pal);
                }
                // 右侧为未读点/窗格数/状态指示预留宽度，避免文字压到它们。
                let right_reserve: f32 = if entry.pane_count > 1 {
                    if entry.unseen {
                        40.0
                    } else {
                        34.0
                    }
                } else if entry.unseen {
                    16.0
                } else {
                    8.0
                };
                // 状态指示在最右居中，确保给它留出空间。
                let right_reserve = if entry.busy {
                    right_reserve.max(22.0)
                } else {
                    right_reserve
                };
                let text_left = rect.left() + ICON_COL;
                let text_w = (rect.right() - right_reserve - text_left).max(10.0);
                // 名称 + 路径两行（路径未知时仅名称行），整体垂直居中于行内。
                let block_h = if entry.path.is_some() { 30.0 } else { 16.0 };
                let text_rect = egui::Rect::from_min_size(
                    egui::pos2(text_left, rect.center().y - block_h / 2.0),
                    egui::vec2(text_w, block_h),
                );
                let mut text_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(text_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                text_ui.spacing_mut().item_spacing.y = 2.0;
                // 名称行：激活时用前景亮色，否则常规前景；超宽截断省略。
                text_ui.add(
                    egui::Label::new(egui::RichText::new(&entry.name).size(13.0).color(pal.fg))
                        .selectable(false)
                        .truncate(),
                );
                // 路径行：暗色小字；超宽截断（完整路径走悬停提示）。
                if let Some(path) = &entry.path {
                    text_ui.add(
                        egui::Label::new(egui::RichText::new(path).size(11.0).color(pal.fg_dim))
                            .selectable(false)
                            .truncate(),
                    );
                    resp = resp.on_hover_text(path.clone());
                }
                if resp.clicked() {
                    out.activate = Some(entry.id);
                }
                resp.context_menu(|ui| {
                    let s = crate::i18n::strings();
                    if ui.button(s.menu_rename).clicked() {
                        st.renaming = Some((entry.id, entry.name.clone()));
                        st.rename_focus = true;
                        ui.close();
                    }
                    if ui.button(s.menu_close).clicked() {
                        out.close = Some(entry.id);
                        ui.close();
                    }
                });
                // 会话忙指示（右侧垂直居中）：claude 等 TUI 工作时显示科技感
                // 旋转点环（自绘渐变拖尾，比默认 Spinner 精致、跟随强调色）。
                if entry.busy {
                    draw_busy_spinner(
                        ui,
                        egui::pos2(rect.right() - 11.0, rect.center().y),
                        6.0,
                        pal.accent,
                    );
                }
                // 未读小圆点（后台有新输出，切换到该 tab 时清除）——贴名称行右端。
                // 忙指示存在时让位（状态信息更及时）。
                if entry.unseen && !entry.busy {
                    let center = egui::pos2(rect.right() - 10.0, rect.top() + 13.0);
                    ui.painter().circle_filled(center, 3.0, pal.accent);
                }
                // 窗格数指示（F5 批2）：多窗格 tab 在条目右上标「N 格」
                // （有未读点时左移让位）。
                if entry.pane_count > 1 {
                    let x = rect.right() - if entry.unseen { 20.0 } else { 8.0 };
                    ui.painter().text(
                        egui::pos2(x, rect.top() + 13.0),
                        egui::Align2::RIGHT_CENTER,
                        crate::i18n::fmt1(s.pane_count_fmt, entry.pane_count),
                        egui::FontId::proportional(10.0),
                        pal.fg_dim,
                    );
                }
            }
        });

    // R8：底部「⚙ 设置」和「＋ 新建会话」按钮已删除。
    // 设置入口=头像菜单 Settings + Ctrl+,（两者均健在）。
    // 新建会话入口=侧栏标题栏右端小「＋」按钮（Ctrl+T）。
}

/// 远程设备列表（M5.2）：在线置顶 / 离线置底、本机标记、选中高亮、
/// 右键改名 / 删除。结构仿会话侧栏 [`sidebar_ui`]，简化为两行条目。
fn remote_device_list_ui(
    ui: &mut egui::Ui,
    devices: &[lumen_protocol::DeviceRecord],
    active_id: Option<&str>,
    st: &mut ShellState,
    pal: &theme::Palette,
    out: &mut ShellOutput,
) {
    let s = crate::i18n::strings();

    // 标题栏条（高 30px，与会话栏标题对齐）。
    {
        let (title_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
        let mut title_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(title_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        title_ui.add(
            egui::Label::new(
                egui::RichText::new(s.remote_list_title)
                    .size(12.0)
                    .color(pal.fg_dim),
            )
            .selectable(false),
        );
    }
    ui.add_space(2.0);

    // 过滤掉本机（本机会话已在「本地」tab 显示，远程列表只列其他设备）；
    // 排序：在线置顶、离线置底（各组内保持服务端 last_seen 倒序）。
    let mut order: Vec<usize> = (0..devices.len())
        .filter(|&i| !devices[i].is_self)
        .collect();
    order.sort_by_key(|&i| !devices[i].online);

    egui::ScrollArea::vertical()
        .id_salt("remote_device_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 用 order（已过滤本机）判空：仅有本机时也显示「暂无设备」。
            if order.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(s.remote_empty)
                        .size(11.0)
                        .color(pal.fg_dim),
                );
                return;
            }
            for &i in &order {
                let dev = &devices[i];
                // 重命名中：行内编辑框（仿会话重命名仲裁）。
                let is_renaming = st
                    .renaming_device
                    .as_ref()
                    .is_some_and(|(id, _)| *id == dev.id);
                if is_renaming {
                    if let Some((_, buf)) = st.renaming_device.as_mut() {
                        let resp =
                            ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
                        if st.rename_device_focus {
                            resp.request_focus();
                            st.rename_device_focus = false;
                        }
                        if resp.lost_focus() {
                            let by_key = ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)
                            });
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                out.rename_device = Some((dev.id.clone(), buf.trim().to_owned()));
                            }
                            out.rename_device_ended_by_key = by_key;
                            st.renaming_device = None;
                        }
                    }
                    continue;
                }

                const ROW_H: f32 = 44.0;
                const DOT_COL: f32 = 22.0;
                let active = active_id == Some(dev.id.as_str());
                let (rect, mut resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_H),
                    egui::Sense::click(),
                );
                let bg = if active {
                    pal.selection
                } else if resp.hovered() {
                    pal.bg_highlight
                } else {
                    egui::Color32::TRANSPARENT
                };
                if bg != egui::Color32::TRANSPARENT {
                    ui.painter().rect_filled(rect, 2.0, bg);
                }
                // 左侧在线圆点（绿=在线，灰=离线）。
                let dot_c = egui::pos2(rect.left() + DOT_COL / 2.0, rect.center().y);
                let dot_color = if dev.online {
                    egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
                } else {
                    pal.fg_dim
                };
                ui.painter().circle_filled(dot_c, 4.0, dot_color);
                // 名称行 + 状态行。
                let text_left = rect.left() + DOT_COL;
                let text_w = (rect.right() - 8.0 - text_left).max(10.0);
                let text_rect = egui::Rect::from_min_size(
                    egui::pos2(text_left, rect.center().y - 15.0),
                    egui::vec2(text_w, 30.0),
                );
                let mut text_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(text_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                text_ui.spacing_mut().item_spacing.y = 2.0;
                let name_color = if dev.online { pal.fg } else { pal.fg_dim };
                text_ui.add(
                    egui::Label::new(egui::RichText::new(&dev.name).size(13.0).color(name_color))
                        .selectable(false)
                        .truncate(),
                );
                let status = if dev.is_self {
                    let on = if dev.online {
                        s.remote_online
                    } else {
                        s.remote_offline
                    };
                    format!("{on} · {}", s.remote_this_device)
                } else if dev.online {
                    s.remote_online.to_string()
                } else {
                    format!("{}（{}）", s.remote_offline, s.remote_unavailable)
                };
                text_ui.add(
                    egui::Label::new(egui::RichText::new(status).size(11.0).color(pal.fg_dim))
                        .selectable(false)
                        .truncate(),
                );

                // 仅在线设备可选中（离线不可连接）。单击选中、双击发起控制。
                if resp.clicked() && dev.online {
                    out.activate_device = Some(dev.id.clone());
                }
                if resp.double_clicked() && dev.online {
                    out.connect_device = Some(dev.id.clone());
                }
                resp = resp.on_hover_text(format!("{} · {}", dev.os, dev.app_version));
                resp.context_menu(|ui| {
                    let s = crate::i18n::strings();
                    // 在线设备：右键「连接」（控制）置顶。
                    if dev.online && ui.button(s.remote_menu_connect).clicked() {
                        out.connect_device = Some(dev.id.clone());
                        ui.close();
                    }
                    if ui.button(s.menu_rename).clicked() {
                        st.renaming_device = Some((dev.id.clone(), dev.name.clone()));
                        st.rename_device_focus = true;
                        ui.close();
                    }
                    if ui.button(s.remote_menu_delete).clicked() {
                        out.delete_device = Some(dev.id.clone());
                        ui.close();
                    }
                });
            }
        });
}

/// 会话「忙」指示：自绘科技感方形轨迹 spinner——一个小圆点沿**正方形
/// 边框**匀速跑圈（上→右→下→左），后面跟一串逐点缩小变暗的拖尾。
/// 速度由 egui 时间驱动（每帧 `request_repaint` 续动），跟随强调色。
/// `half` 为正方形半边长。
fn draw_busy_spinner(ui: &egui::Ui, center: egui::Pos2, half: f32, color: egui::Color32) {
    ui.ctx().request_repaint(); // 驱动连续动画
    let t = ui.input(|i| i.time) as f32;
    let side = 2.0 * half; // 单边长
    let perim = 4.0 * side; // 周长
    // 沿正方形边框把「行进距离 d」映射到坐标（上边→右边→下边→左边）。
    let point_at = |d: f32| -> egui::Pos2 {
        let d = d.rem_euclid(perim);
        let (l, top, r, bot) = (
            center.x - half,
            center.y - half,
            center.x + half,
            center.y + half,
        );
        if d < side {
            egui::pos2(l + d, top) // 上边：左 → 右
        } else if d < 2.0 * side {
            egui::pos2(r, top + (d - side)) // 右边：上 → 下
        } else if d < 3.0 * side {
            egui::pos2(r - (d - 2.0 * side), bot) // 下边：右 → 左
        } else {
            egui::pos2(l, bot - (d - 3.0 * side)) // 左边：下 → 上
        }
    };
    const TRAIL: usize = 6; // 拖尾点数（含头）
    let head = t * perim * 0.55; // 每秒约 0.55 圈
    let gap = perim * 0.07; // 拖尾点间距
    for k in 0..TRAIL {
        let frac = k as f32 / TRAIL as f32; // 0=头 … 接近 1=尾
        let pos = point_at(head - k as f32 * gap);
        let alpha = (((1.0 - frac) * 0.85 + 0.15) * 255.0) as u8; // 头亮尾暗
        let dot_r = 0.5 + (1.0 - frac) * 0.65; // 头大尾小（整体缩小一半）
        ui.painter().circle_filled(
            pos,
            dot_r,
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
}

/// 侧栏会话条目左侧的终端图标（codicon terminal 风格）。
///
/// 圆角外框内画 `>` 提示符 + 下划线光标；`active` 时用前景亮色，否则暗色。
fn draw_session_icon(ui: &egui::Ui, center: egui::Pos2, active: bool, pal: &theme::Palette) {
    let painter = ui.painter();
    let fg = if active { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.3, fg);
    // 外框 18×15，圆角 3，0.5 像素对齐（防模糊）。
    let bw = 18.0_f32;
    let bh = 15.0_f32;
    let ox = (center.x - bw / 2.0 + 0.5).floor() - 0.5;
    let oy = (center.y - bh / 2.0 + 0.5).floor() - 0.5;
    let frame = egui::Rect::from_min_size(egui::pos2(ox, oy), egui::vec2(bw, bh));
    painter.rect_stroke(frame, 3.0, stroke, egui::StrokeKind::Middle);
    // 内部 `>` 提示符（两段线构成尖角，左上起笔）。
    let px = ox + 4.5;
    let py = oy + 4.0;
    let chev = 3.0_f32;
    painter.line_segment([egui::pos2(px, py), egui::pos2(px + chev, py + chev)], stroke);
    painter.line_segment(
        [egui::pos2(px + chev, py + chev), egui::pos2(px, py + 2.0 * chev)],
        stroke,
    );
    // 下划线光标（提示符右侧短横，贴底）。
    let uy = oy + bh - 4.0;
    painter.line_segment(
        [egui::pos2(px + chev + 2.0, uy), egui::pos2(ox + bw - 4.0, uy)],
        stroke,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两格左右布局的整格矩形（含标题栏）。
    fn two_rects() -> Vec<egui::Rect> {
        vec![
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0)),
            egui::Rect::from_min_size(egui::pos2(102.0, 0.0), egui::vec2(100.0, 100.0)),
        ]
    }

    #[test]
    fn 换位落点_命中目标窗格() {
        // 从 0 号拖到 1 号窗格内部 → 目标 1；反向同理。
        assert_eq!(
            swap_target(&two_rects(), 0, egui::pos2(150.0, 50.0)),
            Some(1)
        );
        assert_eq!(
            swap_target(&two_rects(), 1, egui::pos2(50.0, 50.0)),
            Some(0)
        );
    }

    #[test]
    fn 换位落点_源格与区外取消() {
        // 落回源格自身 / 窗格间隙 / 终端区外 → 取消（None）。
        assert_eq!(swap_target(&two_rects(), 0, egui::pos2(50.0, 50.0)), None);
        assert_eq!(swap_target(&two_rects(), 0, egui::pos2(101.0, 50.0)), None);
        assert_eq!(swap_target(&two_rects(), 0, egui::pos2(300.0, 300.0)), None);
    }
}
