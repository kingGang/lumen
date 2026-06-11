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

/// 左侧会话栏宽度（逻辑像素）。
pub const SIDEBAR_WIDTH: f32 = 180.0;

/// 窗格右上角关闭按钮的边长（逻辑像素，F5 批2）。
const PANE_CLOSE_SIZE: f32 = 16.0;

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
    /// 是否为焦点窗格（多窗格时画 1px accent 边框）。
    pub focused: bool,
}

/// 一帧外壳 UI 的输入（main.rs 按帧构造的状态快照）。
pub struct ShellInput<'a> {
    /// 激活 tab 的窗格（布局顺序：先上排后下排、行内自左向右）。
    pub panes: &'a [PaneView],
    /// tab 条目（侧栏列表；顶栏标题取自其中的激活条目）。
    pub tabs: &'a [TabItem],
    /// 登录态（顶栏头像、头像菜单、设置页 Account 三处同源展示）。
    pub profile: Option<&'a crate::profile::Profile>,
    /// 焦点窗格的 cwd（文件树跟随；OSC 9;9 上报）。
    pub cwd: Option<&'a std::path::Path>,
    /// 焦点窗格 shell 空闲（文件树 cd 注入闸门）。
    pub shell_idle: bool,
}

/// 一帧外壳 UI 的产出。
pub struct ShellOutput {
    /// 终端工作区整体矩形（egui 逻辑点坐标；拖放落点判定等用）。
    pub term_rect: egui::Rect,
    /// 各窗格矩形（与 [`ShellInput::panes`] 同序，已对齐物理像素；
    /// main.rs 据此重建离屏纹理 / resize / 路由鼠标与 IME）。
    pub pane_rects: Vec<egui::Rect>,
    /// 本帧用户点击了终端区（焦点交还终端）。
    pub term_clicked: bool,
    /// 点击了某个窗格（下标对应 [`ShellInput::panes`]；切换焦点窗格）。
    pub pane_clicked: Option<usize>,
    /// 点击了某窗格右上角的 ✕（关闭该窗格；按钮仅多窗格时出现）。
    pub pane_close: Option<usize>,
    /// 各窗格关闭按钮的命中矩形（egui 逻辑坐标，与窗格同序；单窗格
    /// 时为空）。main.rs 据此让 raw 鼠标路由对 ✕ 让位（不聚焦/不建
    /// 选区/不交出终端焦点，点击由 egui 侧处理）。
    pub pane_close_rects: Vec<egui::Rect>,
    /// 顶栏「＋」：焦点 tab 内新增窗格（同 Ctrl+Shift+D，F5）。
    pub new_pane: bool,
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
        pane_close_rects: Vec::new(),
        new_pane: false,
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
    let pal = theme::palette(app_settings.appearance.theme.is_light());
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

    egui::Panel::left("lumen_sidebar")
        .exact_size(SIDEBAR_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.bg_dark)
                .inner_margin(egui::Margin::symmetric(8, 10)),
        )
        .show_inside(root, |ui| sidebar_ui(ui, input.tabs, st, pal, &mut out));

    // 中间一栏：文件树（可折叠；树根跟随激活会话 cwd）。开合改变
    // 终端区宽度，沿用「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
    let ft = filetree::show(root, &mut st.filetree, input.cwd, input.shell_idle, pal);
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
            // 分屏布局（F5）：固定均分 1~6 格，窗格各自一张离屏纹理。
            let rects = layout::pane_rects(input.panes.len(), area);
            for (i, (pane, rect)) in input.panes.iter().zip(&rects).enumerate() {
                let rect = rect.round_to_pixels(ppp);
                if let Some(tex) = pane.tex {
                    ui.put(
                        rect,
                        egui::Image::new(egui::load::SizedTexture::new(tex, rect.size())),
                    );
                }
                // 点击窗格 → 聚焦该窗格 + 焦点交还终端。选区/块点击/
                // 滚轮仍走 window_event 按窗格矩形路由（见 main.rs）。
                let resp = ui.interact(rect, ui.id().with(("pane", i)), egui::Sense::click());
                if resp.clicked() {
                    out.pane_clicked = Some(i);
                    out.term_clicked = true;
                }
                // 焦点窗格指示：多窗格时画 1px accent 边框（单窗格
                // 不画，满屏边框只是视觉噪音）。
                if pane.focused && input.panes.len() > 1 {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                // 窗格关闭按钮（F5 批2）：多窗格时悬停窗格右上角浮现
                // 小 ✕（裁决：右键已被「有选区复制/无选区粘贴」惯例
                // 占用，右键菜单会破坏既有交互；悬停 ✕ 是 Warp 的窗格
                // 控件形态、成本也更低）。命中区每帧固定注册（id 稳定、
                // 首点即响应），仅悬停本窗格时绘制；✕ 用画线而非字形
                // （不赌字体覆盖）。raw 鼠标路由对该矩形让位见 main.rs。
                if input.panes.len() > 1 {
                    let close_rect = egui::Rect::from_min_size(
                        egui::pos2(rect.max.x - PANE_CLOSE_SIZE - 6.0, rect.min.y + 6.0),
                        egui::vec2(PANE_CLOSE_SIZE, PANE_CLOSE_SIZE),
                    );
                    let cresp = ui.interact(
                        close_rect,
                        ui.id().with(("pane_close", i)),
                        egui::Sense::click(),
                    );
                    if ui.rect_contains_pointer(rect) || cresp.hovered() {
                        let painter = ui.painter();
                        let c = close_rect.center();
                        if cresp.hovered() {
                            painter.circle_filled(c, PANE_CLOSE_SIZE / 2.0, pal.bg_highlight);
                        }
                        let r = 3.5;
                        let stroke = egui::Stroke::new(
                            1.2,
                            if cresp.hovered() { pal.fg } else { pal.fg_dim },
                        );
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
                }
                out.pane_rects.push(rect);
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

        let fill = if entry.active {
            pal.bg_highlight
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
