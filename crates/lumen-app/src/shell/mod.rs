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
pub mod login_ui;
pub mod settings_ui;
pub mod theme;
pub mod topbar;

/// 左侧会话栏宽度（逻辑像素）。
pub const SIDEBAR_WIDTH: f32 = 180.0;

/// 一条会话在侧栏的展示数据（由 main.rs 按帧构造）。
pub struct SessionEntry {
    pub id: u64,
    /// 展示标题（自定义名 > OSC 标题 > 默认名，已做空回退）。
    pub title: String,
    pub active: bool,
    /// 后台期间有未读输出（条目右侧小圆点）。
    pub unseen: bool,
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
}

/// 一帧外壳 UI 的输入（main.rs 按帧构造的状态快照）。
pub struct ShellInput<'a> {
    /// 终端离屏纹理的 egui 句柄。
    pub term_tex: egui::TextureId,
    /// 会话条目（侧栏列表；顶栏标题取自其中的激活条目）。
    pub sessions: &'a [SessionEntry],
    /// 登录态（顶栏头像、头像菜单、设置页 Account 三处同源展示）。
    pub profile: Option<&'a crate::profile::Profile>,
    /// 激活会话的 cwd（文件树跟随；OSC 9;9 上报）。
    pub cwd: Option<&'a std::path::Path>,
    /// 激活会话 shell 空闲（文件树 cd 注入闸门）。
    pub shell_idle: bool,
}

/// 一帧外壳 UI 的产出。
pub struct ShellOutput {
    /// 终端工作区矩形（egui 逻辑点坐标）。
    pub term_rect: egui::Rect,
    /// 本帧用户点击了终端区（焦点交还终端）。
    pub term_clicked: bool,
    /// 点击了某会话条目（切换激活）。
    pub activate: Option<u64>,
    /// 请求关闭某会话（右键菜单）。
    pub close: Option<u64>,
    /// 提交的重命名：(会话 id, 新名字)。空字符串 = 清除自定义名。
    pub rename: Option<(u64, String)>,
    /// 点击了「新建会话」。
    pub new_session: bool,
    /// 文件树：激活了目录且 shell 空闲，请求向激活会话注入 cd。
    pub cd_dir: Option<std::path::PathBuf>,
    /// 文件树：激活了文件，用系统默认程序打开。
    pub open_file: Option<std::path::PathBuf>,
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
        term_clicked: false,
        activate: None,
        close: None,
        rename: None,
        new_session: false,
        cd_dir: None,
        open_file: None,
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
        .is_some_and(|(id, _)| !input.sessions.iter().any(|e| e.id == *id))
    {
        st.renaming = None;
    }

    // —— 顶栏（先于侧栏加入面板布局，横贯整窗）：标题 + 头像菜单 ——
    // 标题与窗口标题同源（激活会话的 display_title，侧栏条目已做
    // 空回退），无激活条目（防御）时退回应用名。
    let active_title = input
        .sessions
        .iter()
        .find(|e| e.active)
        .map_or("Lumen", |e| e.title.as_str());
    let tb = topbar::show(root, active_title, input.profile, pal);
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
        .show_inside(root, |ui| sidebar_ui(ui, input.sessions, st, pal, &mut out));

    // 中间一栏：文件树（可折叠；树根跟随激活会话 cwd）。开合改变
    // 终端区宽度，沿用「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
    let ft = filetree::show(root, &mut st.filetree, input.cwd, input.shell_idle, pal);
    out.cd_dir = ft.cd_dir;
    out.open_file = ft.open_file;

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show_inside(root, |ui| {
            let rect = ui.available_rect_before_wrap();
            ui.put(
                rect,
                egui::Image::new(egui::load::SizedTexture::new(input.term_tex, rect.size())),
            );
            // 点击终端区 → 焦点交还终端。选区/块点击/滚轮仍走
            // window_event 按终端区矩形路由（见 main.rs）。
            let resp = ui.interact(rect, ui.id().with("terminal_area"), egui::Sense::click());
            out.term_clicked = resp.clicked();
            out.term_rect = rect;
        });

    // —— 设置覆盖层（盖住三栏；终端在其下照常消化输出与渲染）——
    // 齿轮按钮本帧点击 → 立即打开（同帧呈现，避免一帧裸跳）。
    if out.settings_opened && !st.settings.open {
        st.settings.open_with(app_settings);
    }
    if st.settings.open {
        let s_out =
            settings_ui::show(root.ctx(), &mut st.settings, app_settings, input.profile, pal);
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
    out
}

/// 侧栏内容：会话条目列表 + 底部设置/新建按钮。
fn sidebar_ui(
    ui: &mut egui::Ui,
    sessions: &[SessionEntry],
    st: &mut ShellState,
    pal: &theme::Palette,
    out: &mut ShellOutput,
) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new("会话").size(11.0).color(pal.fg_dim));
    ui.add_space(4.0);

    for entry in sessions {
        // 重命名中的条目：行内编辑框替代按钮。Enter 提交、Esc 或
        // 点击别处取消（egui 的 TextEdit 在这三种情况都会失焦）。
        let is_renaming = st.renaming.as_ref().is_some_and(|(id, _)| *id == entry.id);
        if is_renaming {
            if let Some((_, buf)) = st.renaming.as_mut() {
                let resp = ui.add(
                    egui::TextEdit::singleline(buf).desired_width(f32::INFINITY),
                );
                if st.rename_focus {
                    resp.request_focus();
                    st.rename_focus = false;
                }
                if resp.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        out.rename = Some((entry.id, buf.trim().to_owned()));
                    }
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
        let btn = egui::Button::new(
            egui::RichText::new(format!("● {}", entry.title)).color(pal.fg),
        )
        .fill(fill)
        .wrap_mode(egui::TextWrapMode::Truncate)
        .min_size(egui::vec2(ui.available_width(), 30.0));
        let resp = ui.add(btn);
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
