//! 应用外壳 UI（egui）：顶栏 + 侧栏 + 文件树 + 终端工作区布局 +
//! 设置/登录覆盖层。
//!
//! M3.2 起侧栏是真功能的会话 tab 列表：条目（标题 + 未读点 + 激活
//! 高亮）点击切换、右键菜单重命名/关闭、底部新建。M3.3 增加中间一栏
//! 文件树（跟随激活会话 cwd，可折叠）。M3.4 增加设置界面（全屏覆盖
//! 层，入口为侧栏底部齿轮与 Ctrl+,）。M3.5 增加顶栏（标题 + 头像
//! 菜单）与登录覆盖层（mock）。UI 只产出动作（[`ShellOutput`]），
//! 会话增删切换/PTY 写入/设置即时生效/登录写盘由 main.rs 执行。

pub mod filetree;
pub mod layout;
pub mod login_ui;
pub mod settings_ui;
pub mod theme;
pub mod toast;
pub mod topbar;

/// 窗格标题栏里关闭按钮的边长（逻辑像素，F5 批2 引入、F7① 迁入
/// 标题栏常驻）。
const PANE_CLOSE_SIZE: f32 = 16.0;

/// 窗格标题栏高度（逻辑像素，F7①）：占高从终端内容区扣除（该格
/// 终端行数相应减少）。
const PANE_TITLE_HEIGHT: f32 = 24.0;

/// 一个 tab 在侧栏的展示数据（由 main.rs 按帧构造；M3.7 起侧栏
/// 条目 = tab，每 tab 含 1~6 个终端窗格）。
pub struct TabItem {
    pub id: u64,
    /// 展示标题（自定义名 > 焦点窗格 cwd 完整路径 > OSC 标题 >
    /// 「会话 N」，取值见 Tab::display_title，恒非空）。
    pub title: String,
    /// 悬停提示：默认名为 cwd 时为完整路径（条目宽度有限会截断，
    /// 悬停看全路径，F4）；其余情况 None 不挂提示。
    pub hover_path: Option<String>,
    pub active: bool,
    /// 后台期间 tab 内任意窗格有未读输出（条目右侧小圆点）。
    pub unseen: bool,
    /// tab 内窗格数（>1 时条目右侧标「N 格」，F5 批2 视觉打磨）。
    pub pane_count: usize,
}

/// 跨帧保留的外壳 UI 状态。
#[derive(Default)]
pub struct ShellState {
    /// 进行中的重命名：(会话 id, 编辑中文本)。编辑期间键盘归 egui。
    pub renaming: Option<(u64, String)>,
    /// 重命名刚开始，下一帧把焦点交给编辑框。
    rename_focus: bool,
    /// 文件树（树根/展开/可见性等跨帧状态）。
    pub filetree: filetree::FileTreeState,
    /// 设置页（开关/分类/字体编辑缓冲等跨帧状态）。
    pub settings: settings_ui::SettingsUiState,
    /// 登录覆盖层（开关/输入缓冲等跨帧状态）。
    pub login: login_ui::LoginUiState,
    /// 系统提示框队列（toast；shell 内外都可 push，见 toast.rs）。
    pub toast: toast::ToastState,
}

/// 激活 tab 中一个窗格的展示数据（终端工作区分屏用，F5）。
pub struct PaneView {
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
    /// 焦点窗格的 cwd（文件树跟随；OSC 9;9 上报）。
    pub cwd: Option<&'a std::path::Path>,
    /// 焦点窗格 shell 空闲（文件树 cd 注入闸门）。
    pub shell_idle: bool,
    /// 系统当前是否深色模式（P12 Sync with OS：外壳色板与设置页
    /// 「当前主题」展示按它解析生效主题 id）。
    pub os_dark: bool,
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
    /// 顶栏「＋」：焦点 tab 内新增窗格（同 Ctrl+Shift+D，F5）。
    pub new_pane: bool,
    /// 顶栏「▦」（P15）：当前 tab 全部窗格比例恢复均分（最大化态
    /// 先退出）；复位后 main 落盘。
    pub layout_reset: bool,
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
    /// 点击了「新建会话」。
    pub new_session: bool,
    /// 文件树：激活了目录且 shell 空闲，请求向焦点窗格注入 cd。
    pub cd_dir: Option<std::path::PathBuf>,
    /// 文件树：激活了文件，用系统默认程序打开。
    pub open_file: Option<std::path::PathBuf>,
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
    /// 登录覆盖层本帧被打开（main 把终端焦点交给 egui）。
    pub login_opened: bool,
    /// 登录覆盖层本帧被关闭（main 按覆盖层整体状态决定焦点归属）。
    pub login_closed: bool,
    /// mock 登录成功的档案（main 写盘并更新全局登录态——顶栏头像、
    /// 头像菜单、设置页 Account 三处同源即时联动）。
    pub logged_in: Option<crate::profile::Profile>,
    /// 请求登出（头像菜单或设置页 Account；main 删盘并清登录态）。
    pub logged_out: bool,
}

/// 绘制整个外壳：顶栏 + 左侧会话栏 + 中间文件树 + 中央终端纹理 +
/// 设置/登录覆盖层。输入是 main 按帧构造的状态快照（[`ShellInput`]）；
/// `app_settings` 是设置页直接编辑的数据（变更经 [`ShellOutput`]
/// 通知 main 即时生效与写盘）。
pub fn show(
    root: &mut egui::Ui,
    input: &ShellInput<'_>,
    st: &mut ShellState,
    app_settings: &mut crate::settings::Settings,
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
        new_pane: false,
        layout_reset: false,
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
        new_session: false,
        cd_dir: None,
        open_file: None,
        insert_path: None,
        copy_text: None,
        filetree_dialog_closed: false,
        settings_opened: false,
        settings_closed: false,
        settings_font_changed: false,
        settings_theme_changed: false,
        login_opened: false,
        login_closed: false,
        logged_in: None,
        logged_out: false,
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

    // —— 顶栏（先于侧栏加入面板布局，横贯整窗）：标题 + 头像菜单 ——
    // 标题与窗口标题同源（激活 tab 的 display_title，恒非空），
    // 无激活条目（防御）时退回应用名。
    let active_title = input
        .tabs
        .iter()
        .find(|e| e.active)
        .map_or("Lumen", |e| e.title.as_str());
    let tb = topbar::show(root, active_title, input.panes.len(), input.profile, pal);
    if tb.new_pane {
        out.new_pane = true;
    }
    if tb.reset_layout {
        out.layout_reset = true;
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

    // 左侧会话栏：可拖宽（P10）。default_size 只在 egui 无面板记忆
    // （首帧）时生效 = 还原持久化宽度；此后宽度由 egui 面板自管，
    // 实际值经 sidebar_width 报回 main，松手时写盘。拖动改变终端区
    // 宽度，沿用「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
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
    // 侧栏右缘的拖宽手柄命中区（与面板边线同心、向两侧各探
    // resize_grab_radius_side）：报给 main 让 raw 鼠标按下让位——
    // 拖宽期间不交出终端焦点，调完宽度接着打字不断流。
    let grab = root.style().interaction.resize_grab_radius_side;
    let edge_rect = |edge_x: f32, root: &egui::Ui| {
        let y = root.available_rect_before_wrap().y_range();
        egui::Rect::from_x_y_ranges(edge_x - grab..=edge_x + grab, y)
    };
    out.panel_resize_rects
        .push(edge_rect(sb_resp.response.rect.max.x, root));

    // 中间一栏：文件树（可折叠 + 可拖宽 P10；树根跟随激活会话 cwd）。
    // 开合/拖宽改变终端区宽度，沿用同一条矩形变化链路。
    let ft = filetree::show(
        root,
        &mut st.filetree,
        input.cwd,
        input.shell_idle,
        pal,
        app_settings.layout.filetree_width,
    );
    out.filetree_width = ft.panel_width;
    if ft.panel_width.is_some() {
        // 文件树右缘手柄探入终端区左缘数像素，必须让位（收起窄条不
        // 可拖宽，无手柄不让位）。
        out.panel_resize_rects
            .push(edge_rect(root.available_rect_before_wrap().min.x, root));
    }
    out.cd_dir = ft.cd_dir;
    out.open_file = ft.open_file;
    out.copy_text = ft.copy_text;
    out.filetree_dialog_closed = ft.dialog_closed;
    if ft.busy_hint {
        // shell 忙未注入 cd：树内轻提示之外再弹 toast（更醒目，树栏
        // 收窄/视线在终端区时也能看到）。
        st.toast
            .push(toast::ToastKind::Warn, "Shell 正忙，未执行 cd");
    }
    // 文件操作/搜索的结果反馈（egui 帧内 push，当帧即可见）。
    for (kind, text) in ft.toasts {
        st.toast.push(kind, text);
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

                // ✕ 常驻标题栏右端（F7①：从悬停浮现迁入标题栏；单窗格
                // 关闭 = 关整个 tab，与 Ctrl+Shift+W 同语义）。✕ 用画线
                // 而非字形（不赌字体覆盖）；raw 鼠标路由对该矩形让位见
                // main.rs（pane_close_rects_px）。
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
                if cresp.on_hover_text("关闭窗格 (Ctrl+Shift+W)").clicked() {
                    out.pane_close = Some(i);
                }
                out.pane_close_rects.push(close_rect);

                // —— 最大化/还原按钮（P14）：✕ 左侧，仅多窗格时显示
                // （单窗格本就满屏，无最大化语义）。普通态画 ▢（最大
                // 化），最大化态画 ⧉（双矩形，还原）；painter 画线与
                // ✕ 同款风格，不赌字体覆盖。命中矩形进 pane_close_rects
                // 让 raw 鼠标路由让位（同 ✕：点击不聚焦/不建选区）。
                let mut title_end = close_rect.min.x - 4.0;
                if input.panes.len() > 1 {
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
                    let tip = if maximized.is_some() {
                        "还原窗格 (Ctrl+Shift+Enter)"
                    } else {
                        "最大化窗格 (Ctrl+Shift+Enter)"
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
                let mut title_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(text_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                title_ui.add(
                    egui::Label::new(egui::RichText::new(&pane.title).size(12.0).color(bar_fg))
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
        });

    // 文件树节点拖放的落点判定：要等 CentralPanel 布局出本帧窗格
    // 矩形，故放在面板之后。落在某窗格 → 请求把路径插入**该窗格**
    // 的命令行（F5 批2：目标 = 鼠标落点所在窗格，main 会先聚焦它）；
    // 落在别处（侧栏/树内回弹/窗格间隙）→ 静默忽略。
    if let Some((path, pos)) = ft.external_drop {
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

    // —— 系统提示浮层（最后绘制 = 叠在一切覆盖层之上）——
    toast::show(root.ctx(), &mut st.toast, pal);
    out
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

/// 侧栏内容：tab 条目列表 + 底部设置/新建按钮。
fn sidebar_ui(
    ui: &mut egui::Ui,
    tabs: &[TabItem],
    st: &mut ShellState,
    pal: &theme::Palette,
    out: &mut ShellOutput,
) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new("会话").size(11.0).color(pal.fg_dim));
    ui.add_space(4.0);

    for entry in tabs {
        // 重命名中的条目：行内编辑框替代按钮。Enter 提交、Esc 或
        // 点击别处取消（egui 的 TextEdit 在这三种情况都会失焦）。
        let is_renaming = st.renaming.as_ref().is_some_and(|(id, _)| *id == entry.id);
        if is_renaming {
            if let Some((_, buf)) = st.renaming.as_mut() {
                let resp = ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
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

        // 激活条目用 selection 档（控件梯度最高档）：与悬停的
        // bg_highlight 拉开一档，激活态一眼可辨（M3.7b 高对比）。
        let fill = if entry.active {
            pal.selection
        } else {
            egui::Color32::TRANSPARENT
        };
        let btn =
            egui::Button::new(egui::RichText::new(format!("● {}", entry.title)).color(pal.fg))
                .fill(fill)
                .wrap_mode(egui::TextWrapMode::Truncate)
                .min_size(egui::vec2(ui.available_width(), 30.0));
        let mut resp = ui.add(btn);
        if let Some(path) = &entry.hover_path {
            // 默认名 = cwd：条目截断时悬停可看完整路径。
            resp = resp.on_hover_text(path.clone());
        }
        if resp.clicked() {
            out.activate = Some(entry.id);
        }
        resp.context_menu(|ui| {
            if ui.button("重命名").clicked() {
                st.renaming = Some((entry.id, entry.title.clone()));
                st.rename_focus = true;
                ui.close();
            }
            if ui.button("关闭").clicked() {
                out.close = Some(entry.id);
                ui.close();
            }
        });
        // 未读小圆点（后台有新输出，切换到该 tab 时清除）。
        if entry.unseen {
            let center = egui::pos2(resp.rect.right() - 10.0, resp.rect.center().y);
            ui.painter().circle_filled(center, 3.0, pal.accent);
        }
        // 窗格数指示（F5 批2）：多窗格 tab 在条目右侧标「N 格」
        // （有未读点时左移让位）。
        if entry.pane_count > 1 {
            let x = resp.rect.right() - if entry.unseen { 18.0 } else { 8.0 };
            ui.painter().text(
                egui::pos2(x, resp.rect.center().y),
                egui::Align2::RIGHT_CENTER,
                format!("{} 格", entry.pane_count),
                egui::FontId::proportional(10.0),
                pal.fg_dim,
            );
        }
    }

    // 底部（bottom_up：先加的在最底）：齿轮设置 → 新建会话。
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(2.0);
        let gear = egui::Button::new(egui::RichText::new("⚙ 设置").color(pal.fg_dim))
            .min_size(egui::vec2(ui.available_width(), 26.0));
        if ui.add(gear).on_hover_text("设置 (Ctrl+,)").clicked() {
            out.settings_opened = true;
        }
        let plus = egui::Button::new(egui::RichText::new("＋ 新建会话").color(pal.fg_dim))
            .min_size(egui::vec2(ui.available_width(), 28.0));
        if ui.add(plus).clicked() {
            out.new_session = true;
        }
    });
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
