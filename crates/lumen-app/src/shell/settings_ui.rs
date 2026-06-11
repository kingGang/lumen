//! 设置界面（M3.4）：全屏覆盖层，左分类导航 + 右内容区（对标 Warp，
//! 参考截图 docs/截图/设置界面.png）。
//!
//! UI 只改 [`Settings`] 数据并产出变更标志（[`SettingsOutput`]），
//! 即时生效（renderer 字体/主题重配置、全会话 resize、egui 样式重设、
//! 写盘）由 main.rs 执行。覆盖层用 egui `Modal`：backdrop 阻断下层
//! 面板与终端区的鼠标交互（`mouse_in_term` 按层序判定自然失效），
//! Esc / 右上角 ✕ 关闭。设置页打开期间 PTY 消化与终端渲染照常进行
//! ——覆盖层只是 UI 层。

use crate::profile::Profile;
use crate::settings::{self, Settings};

use super::theme::Palette;

// rfd 文件对话框（P13）：Windows 原生 IFileOpenDialog，同步调用。
use rfd;

/// 左侧分类导航宽度（逻辑像素）。
const NAV_WIDTH: f32 = 220.0;
/// 顶栏高度（居中标题 + 关闭按钮）。
const TOP_BAR_HEIGHT: f32 = 44.0;

/// 主题画廊卡片宽度（P12，对照 Warp 截图的左栏预览卡形态）。
const THEME_CARD_W: f32 = 148.0;
/// 卡片内迷你终端预览图高度。
const THEME_CARD_PREVIEW_H: f32 = 76.0;
/// 卡片总高（预览图 + 下方名字标签行）。
const THEME_CARD_H: f32 = THEME_CARD_PREVIEW_H + 22.0;
/// 画廊卡片间距。
const THEME_CARD_GAP: f32 = 12.0;

/// 字体下拉的常见等宽字体预设（系统未装某项时选择后会回退并提示）。
const FONT_PRESETS: &[&str] = &[
    "Cascadia Mono",
    "Cascadia Code",
    "Consolas",
    "Courier New",
    "JetBrains Mono",
    "Fira Code",
    "Source Code Pro",
];

/// 快捷键说明（表驱动的只读列表；新增快捷键在此补一行）。
const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+T", "新建会话"),
    ("Ctrl+W", "关闭当前会话"),
    ("Ctrl+Tab / Ctrl+Shift+Tab", "下一个 / 上一个会话"),
    ("Ctrl+B", "文件树开合"),
    ("Ctrl+,", "打开 / 关闭设置"),
    ("Ctrl+↑ / Ctrl+↓", "命令块间跳转"),
    ("Ctrl+C", "复制选区或选中块输出；无选择时发送中断"),
    ("Ctrl+V / Shift+Insert", "粘贴"),
    ("Shift+PgUp / PgDn", "上下翻屏"),
    ("Esc", "关闭设置页"),
];

/// 设置页分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Category {
    #[default]
    Account,
    Appearance,
    Shortcuts,
    About,
}

impl Category {
    const ALL: [Self; 4] = [
        Self::Account,
        Self::Appearance,
        Self::Shortcuts,
        Self::About,
    ];

    /// 导航与标题文案（对标 Warp 用英文分类名）。
    fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Appearance => "Appearance",
            Self::Shortcuts => "Keyboard shortcuts",
            Self::About => "About",
        }
    }
}

/// 设置页的跨帧 UI 状态。
#[derive(Default)]
pub struct SettingsUiState {
    /// 设置页是否打开（打开期间终端聚焦布尔置 false，键盘归 egui）。
    pub open: bool,
    /// 当前分类。
    category: Category,
    /// 字体下拉处于「自定义」模式（settings 中的字体名不在预设列表）。
    custom_font_mode: bool,
    /// 自定义字体输入框的编辑缓冲（按「应用」才写入 settings）。
    custom_font_buf: String,
    /// 字号滑块拖动中的预览值：拖动期间只更新它（不写 settings、不
    /// 触发重配置/写盘），松手才提交——防抖（M3 审查项：每步进都
    /// 写盘 + 全会话 resize）。
    font_size_drag: Option<f32>,
    /// 字体回退提示：main 在 reconfigure 后写入实际生效信息，
    /// None 表示请求的字体已生效（无回退）。
    pub font_hint: Option<String>,
    /// 背景图不透明度滑块拖动中的预览值（松手才写盘）。
    bg_opacity_drag: Option<f32>,
    /// 背景图暗化滑块拖动中的预览值（松手才写盘）。
    bg_dim_drag: Option<f32>,
}

impl SettingsUiState {
    /// 打开设置页，并按当前设置初始化字体下拉的自定义态。
    pub fn open_with(&mut self, settings: &Settings) {
        self.open = true;
        let fam = settings.appearance.font_family.as_str();
        self.custom_font_mode = !fam.is_empty() && !FONT_PRESETS.contains(&fam);
        self.custom_font_buf = fam.to_owned();
        // 上次会话可能在拖动中途关页，残留预览值作废。
        self.font_size_drag = None;
    }

    /// 打开设置页并定位到 Keyboard shortcuts 分类（头像菜单入口）。
    pub fn open_with_shortcuts(&mut self, settings: &Settings) {
        self.open_with(settings);
        self.category = Category::Shortcuts;
    }
}

/// 一帧设置页 UI 的产出。
#[derive(Default)]
pub struct SettingsOutput {
    /// 用户请求关闭（Esc / ✕）。
    pub closed: bool,
    /// 字体或字号变更（main 据此重配置 renderer + 全会话 resize）。
    pub font_changed: bool,
    /// 主题变更（main 据此切终端 Theme + egui 样式）。
    pub theme_changed: bool,
    /// Account：点击了 Log out（main 删 profile 并清全局登录态）。
    pub log_out: bool,
    /// Account：未登录态点击了 Log in（打开登录覆盖层）。
    pub open_login: bool,
    /// 背景图参数（opacity/dim/enabled 开关）变更——仅需刷新绘制，
    /// 不需要重载纹理（main 写盘 + 刷新渲染器透明状态）。
    pub background_params_changed: bool,
    /// 背景图路径变更（选新图/清除）——需重载纹理（main 调
    /// `apply_background_image` + 写盘）。
    pub background_image_changed: bool,
}

/// 绘制设置页覆盖层。调用方保证 `st.open == true` 时才调用。
/// `os_dark` = 系统当前深浅模式（P12：「当前主题」展示卡与画廊
/// 高亮按它解析 Sync with OS 的生效主题）。
pub fn show(
    ctx: &egui::Context,
    st: &mut SettingsUiState,
    settings: &mut Settings,
    profile: Option<&Profile>,
    pal: &Palette,
    os_dark: bool,
) -> SettingsOutput {
    let mut out = SettingsOutput::default();
    let screen = ctx.content_rect();

    let modal = egui::Modal::new(egui::Id::new("lumen_settings_modal"))
        // 内容铺满整窗，backdrop 不可见但仍负责阻断下层输入。
        .backdrop_color(egui::Color32::TRANSPARENT)
        .frame(egui::Frame::new().fill(pal.bg_dark))
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            let full = ui.min_rect();

            // —— 顶栏：居中 Settings 标题 + 右上 ✕ ——
            let bar = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), TOP_BAR_HEIGHT));
            ui.painter().text(
                bar.center(),
                egui::Align2::CENTER_CENTER,
                "Settings",
                egui::FontId::proportional(13.0),
                pal.fg_dim,
            );
            let close_rect = egui::Rect::from_center_size(
                egui::pos2(bar.right() - 26.0, bar.center().y),
                egui::vec2(26.0, 26.0),
            );
            let close_btn =
                egui::Button::new(egui::RichText::new("✕").size(14.0).color(pal.fg_dim))
                    .fill(egui::Color32::TRANSPARENT);
            if ui.put(close_rect, close_btn).clicked() {
                out.closed = true;
            }

            // —— 主体：左导航 | 分隔线 | 右内容 ——
            let body = egui::Rect::from_min_max(
                egui::pos2(full.min.x, full.min.y + TOP_BAR_HEIGHT),
                full.max,
            );
            ui.painter().vline(
                body.min.x + NAV_WIDTH,
                body.y_range(),
                egui::Stroke::new(1.0, pal.bg_highlight),
            );

            let nav_rect =
                egui::Rect::from_min_max(body.min, egui::pos2(body.min.x + NAV_WIDTH, body.max.y))
                    .shrink2(egui::vec2(12.0, 8.0));
            let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_rect));
            nav(&mut nav_ui, st, pal);

            let content_rect = egui::Rect::from_min_max(
                egui::pos2(body.min.x + NAV_WIDTH + 1.0, body.min.y),
                body.max,
            )
            .shrink2(egui::vec2(48.0, 16.0));
            let mut content_ui = ui.new_child(egui::UiBuilder::new().max_rect(content_rect));
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(&mut content_ui, |ui| match st.category {
                    Category::Account => account(ui, profile, pal, &mut out),
                    Category::Appearance => appearance(ui, st, settings, pal, &mut out, os_dark),
                    Category::Shortcuts => shortcuts(ui, pal),
                    Category::About => about(ui, pal),
                });
        });
    // Esc（顶层 modal 且无弹层打开时）或 backdrop 点击 → 关闭。
    if modal.should_close() {
        out.closed = true;
    }
    out
}

/// 左侧分类导航。
fn nav(ui: &mut egui::Ui, st: &mut SettingsUiState, pal: &Palette) {
    ui.add_space(4.0);
    for cat in Category::ALL {
        let selected = st.category == cat;
        let btn = egui::Button::new(egui::RichText::new(cat.label()).color(if selected {
            pal.fg
        } else {
            pal.fg_dim
        }))
        // 选中分类用 selection 档：与 hover 拉开一档（M3.7b 高对比）。
        .fill(if selected {
            pal.selection
        } else {
            egui::Color32::TRANSPARENT
        })
        .min_size(egui::vec2(ui.available_width(), 30.0));
        if ui.add(btn).clicked() {
            st.category = cat;
        }
    }
}

/// 内容区标题行。
fn heading(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(text).size(20.0).strong().color(pal.fg));
    ui.add_space(16.0);
}

/// Account（M3.5，参照截图 docs/截图/设置界面.png）：已登录展示
/// 圆头像、展示名、邮箱与 Log out；未登录展示占位头像与 Log in 入口。
/// 登录态与顶栏头像、头像菜单同源 main 的 `Option<Profile>`。
fn account(ui: &mut egui::Ui, profile: Option<&Profile>, pal: &Palette, out: &mut SettingsOutput) {
    heading(ui, pal, "Account");
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
        match profile {
            Some(p) => {
                // 已登录头像：accent 实底 + 反相首字母（与顶栏一致，
                // M3.7b 黑白化：深色白底黑字 / 浅色近黑底白字）。
                ui.painter().circle_filled(rect.center(), 22.0, pal.accent);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    p.avatar_letter(),
                    egui::FontId::proportional(20.0),
                    pal.accent_fg,
                );
            }
            None => {
                // 未登录占位头像：圆底 + 人形图标（egui 自带 emoji 字体）。
                ui.painter()
                    .circle_filled(rect.center(), 22.0, pal.bg_highlight);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "👤",
                    egui::FontId::proportional(22.0),
                    pal.fg_dim,
                );
            }
        }
        ui.vertical(|ui| {
            ui.add_space(4.0);
            match profile {
                Some(p) => {
                    ui.label(egui::RichText::new(&p.display_name).color(pal.fg));
                    ui.label(egui::RichText::new(&p.email).size(11.0).color(pal.fg_dim));
                }
                None => {
                    ui.label(egui::RichText::new("未登录").color(pal.fg));
                    ui.label(
                        egui::RichText::new("本地模拟登录，真账号后续版本接入")
                            .size(11.0)
                            .color(pal.fg_dim),
                    );
                }
            }
        });
    });
    ui.add_space(20.0);
    match profile {
        Some(_) => {
            let btn = egui::Button::new(egui::RichText::new("Log out").color(pal.fg))
                .min_size(egui::vec2(120.0, 30.0));
            if ui.add(btn).clicked() {
                out.log_out = true;
            }
        }
        None => {
            let btn = egui::Button::new(egui::RichText::new("Log in").color(pal.fg))
                .min_size(egui::vec2(120.0, 30.0));
            if ui.add(btn).clicked() {
                out.open_login = true;
            }
        }
    }
}

/// Appearance（P12 Warp 版式）：Themes 组（Sync with OS + 当前主题
/// 展示卡 + 主题画廊）+ Text 组（字体/字号），全部即时生效。
fn appearance(
    ui: &mut egui::Ui,
    st: &mut SettingsUiState,
    settings: &mut Settings,
    pal: &Palette,
    out: &mut SettingsOutput,
    os_dark: bool,
) {
    heading(ui, pal, "Appearance");

    // —— Themes 组 ——
    ui.label(
        egui::RichText::new("Themes")
            .size(14.0)
            .strong()
            .color(pal.fg),
    );
    ui.add_space(8.0);

    // Sync with OS：文字左、开关右（对照 Warp 截图的行布局）。
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Sync with OS").color(pal.fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if toggle_switch(ui, &mut settings.appearance.sync_with_os, pal).changed() {
                // 开/关都可能改变生效主题（手选 ↔ 槽位），交 main
                // 重应用 + 写盘。
                out.theme_changed = true;
            }
        });
    });
    ui.label(
        egui::RichText::new("跟随系统深浅模式自动切换主题")
            .size(11.0)
            .color(pal.fg_dim),
    );
    if settings.appearance.sync_with_os {
        let dark_name = settings::theme_info(&settings.appearance.dark_theme_id).name;
        let light_name = settings::theme_info(&settings.appearance.light_theme_id).name;
        ui.label(
            egui::RichText::new(format!(
                "深色：{dark_name} ｜ 浅色：{light_name}——点击下方深/浅色主题卡片分别指定"
            ))
            .size(11.0)
            .color(pal.fg_dim),
        );
    }
    ui.add_space(10.0);

    // 当前主题展示卡：迷你预览 + 名字（Sync 开启时展示按系统深浅
    // 解析后的生效主题）。
    let eff_info = settings::theme_info(settings.effective_theme_id(os_dark));
    egui::Frame::new()
        .fill(pal.bg_panel)
        .corner_radius(8)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(420.0));
            ui.horizontal(|ui| {
                let (prect, _) =
                    ui.allocate_exact_size(egui::vec2(96.0, 56.0), egui::Sense::hover());
                paint_theme_preview(ui.painter(), prect, &eff_info.theme());
                ui.painter().rect_stroke(
                    prect,
                    6.0,
                    egui::Stroke::new(1.0, pal.bg_highlight),
                    egui::StrokeKind::Inside,
                );
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Current theme")
                            .size(11.0)
                            .color(pal.fg_dim),
                    );
                    ui.label(egui::RichText::new(eff_info.name).color(pal.fg));
                });
            });
        });
    ui.add_space(12.0);

    // 主题画廊：网格卡片（迷你终端配色缩略图 + 名字），点卡片即
    // 切换（终端 + 外壳整套即时生效）。列数随内容区宽度自适应。
    let cols = (((ui.available_width() + THEME_CARD_GAP) / (THEME_CARD_W + THEME_CARD_GAP)).floor()
        as usize)
        .max(1);
    for row in lumen_renderer::themes::BUILTIN.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = THEME_CARD_GAP;
            for info in row {
                theme_card(ui, info, settings, pal, out, os_dark);
            }
        });
        ui.add_space(THEME_CARD_GAP - 4.0);
    }
    ui.add_space(16.0);

    // —— Text 组 ——
    ui.label(
        egui::RichText::new("Text")
            .size(14.0)
            .strong()
            .color(pal.fg),
    );
    ui.add_space(8.0);

    // —— 终端字体 ——
    ui.label(egui::RichText::new("终端字体").color(pal.fg));
    let before_family = settings.appearance.font_family.clone();
    let selected_label = if st.custom_font_mode {
        "自定义…".to_owned()
    } else if settings.appearance.font_family.is_empty() {
        "自动（系统等宽）".to_owned()
    } else {
        settings.appearance.font_family.clone()
    };
    egui::ComboBox::from_id_salt("lumen_font_combo")
        .width(240.0)
        .selected_text(selected_label)
        .show_ui(ui, |ui| {
            let auto_active = !st.custom_font_mode && settings.appearance.font_family.is_empty();
            if ui
                .selectable_label(auto_active, "自动（系统等宽）")
                .clicked()
            {
                st.custom_font_mode = false;
                settings.appearance.font_family.clear();
            }
            for f in FONT_PRESETS {
                let active = !st.custom_font_mode && settings.appearance.font_family == *f;
                if ui.selectable_label(active, *f).clicked() {
                    st.custom_font_mode = false;
                    settings.appearance.font_family = (*f).to_owned();
                }
            }
            if ui
                .selectable_label(st.custom_font_mode, "自定义…")
                .clicked()
            {
                st.custom_font_mode = true;
                st.custom_font_buf = settings.appearance.font_family.clone();
            }
        });
    if st.custom_font_mode {
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut st.custom_font_buf)
                    .hint_text("字体家族名，如 Sarasa Mono SC")
                    .desired_width(240.0),
            );
            let submitted = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("应用").clicked() || submitted {
                settings.appearance.font_family = st.custom_font_buf.trim().to_owned();
            }
        });
    }
    if settings.appearance.font_family != before_family {
        out.font_changed = true;
    }
    // 字体回退提示（请求的字体系统中不存在时由 main 写入）。
    if let Some(hint) = &st.font_hint {
        ui.label(egui::RichText::new(hint).size(11.0).color(pal.warn));
    }
    ui.add_space(16.0);

    // —— 终端字号 ——
    // 滑块绑定预览值而非 settings：拖动中只更新数字显示，松手
    // （drag_stopped）才提交真实字号——否则每跨一个步进就触发一次
    // 「全会话 term/PTY resize + 同步写盘」风暴（M3 审查项）。
    ui.label(egui::RichText::new("终端字号").color(pal.fg));
    let mut preview = st.font_size_drag.unwrap_or(settings.appearance.font_size);
    let resp = ui.add(
        egui::Slider::new(
            &mut preview,
            settings::FONT_SIZE_MIN..=settings::FONT_SIZE_MAX,
        )
        .step_by(1.0)
        .fixed_decimals(0),
    );
    if resp.dragged() {
        // 拖动进行中：暂存预览值（下一帧滑块继续显示它）。
        st.font_size_drag = Some(preview);
    }
    // 提交时机：拖动结束，或非拖动的离散修改（数值框键入/方向键）。
    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
        st.font_size_drag = None;
        // 拖一圈回到原值不算变更：不重配置、不写盘。
        if settings.appearance.font_size != preview {
            settings.appearance.font_size = preview;
            out.font_changed = true;
        }
    }

    ui.add_space(16.0);

    // —— 背景图片组（P13）——
    background_group(ui, st, settings, pal, out);
}

/// 背景图片设置组（P13）：启用开关 + 选图按钮 + 路径展示 +
/// 清除按钮 + 不透明度/暗化滑块。
///
/// 开关/选图/清除：即时生效并写盘（`background_image_changed`）。
/// 滑块：拖动中预览（改 settings 字段但不写盘），松手落盘（`background_params_changed`）。
fn background_group(
    ui: &mut egui::Ui,
    st: &mut SettingsUiState,
    settings: &mut Settings,
    pal: &Palette,
    out: &mut SettingsOutput,
) {
    ui.label(
        egui::RichText::new("背景图片")
            .size(14.0)
            .strong()
            .color(pal.fg),
    );
    ui.add_space(8.0);

    // —— 启用开关 ——
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("启用背景图片").color(pal.fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if toggle_switch(ui, &mut settings.appearance.background.enabled, pal).changed() {
                out.background_image_changed = true;
            }
        });
    });
    ui.add_space(6.0);

    // —— 选图按钮 + 路径展示 + 清除 ——
    ui.horizontal(|ui| {
        if ui
            .button(egui::RichText::new("选择图片…").color(pal.fg))
            .clicked()
        {
            // 同步对话框：阻塞事件循环，PTY 输出在通道中堆积，
            // 对话框关闭后恢复消化。阻塞时长通常 <1s，可接受。
            // rfd 0.15 在 Windows 使用原生 IFileOpenDialog（COM），
            // 零外部 DLL 依赖。
            let result = rfd::FileDialog::new()
                .set_title("选择背景图片")
                .add_filter("图片文件", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            if let Some(path) = result {
                let path_str = path.to_string_lossy().into_owned();
                settings.appearance.background.path = Some(path_str);
                settings.appearance.background.enabled = true;
                out.background_image_changed = true;
            }
        }

        // 当前路径展示（文件名，hover 显示完整路径）。
        if let Some(p) = &settings.appearance.background.path {
            let name = std::path::Path::new(p)
                .file_name()
                .map_or(p.as_str(), |n| n.to_str().unwrap_or(p.as_str()));
            let label = ui.add(
                egui::Label::new(egui::RichText::new(name).color(pal.fg_dim))
                    .truncate()
                    .sense(egui::Sense::hover()),
            );
            label.on_hover_text(p.clone());

            // 清除按钮。
            if ui
                .button(egui::RichText::new("清除").color(pal.fg_dim))
                .clicked()
            {
                settings.appearance.background.path = None;
                settings.appearance.background.enabled = false;
                out.background_image_changed = true;
            }
        } else {
            ui.label(egui::RichText::new("未选择图片").color(pal.fg_dim));
        }
    });
    ui.add_space(10.0);

    // —— 不透明度滑块 ——
    // 拖动中预览（settings 字段实时更新），松手才写盘（background_params_changed）。
    ui.label(egui::RichText::new("不透明度").color(pal.fg));
    let before_opacity = settings.appearance.background.opacity;
    let mut opacity_preview = st
        .bg_opacity_drag
        .unwrap_or(settings.appearance.background.opacity);
    let resp_opacity = ui.add(
        egui::Slider::new(
            &mut opacity_preview,
            settings::BACKGROUND_OPACITY_MIN..=settings::BACKGROUND_OPACITY_MAX,
        )
        .step_by(0.05)
        .fixed_decimals(2),
    );
    if resp_opacity.dragged() {
        st.bg_opacity_drag = Some(opacity_preview);
        // 拖动中即时预览（不写盘）。
        settings.appearance.background.opacity = opacity_preview;
    }
    let opacity_committed =
        resp_opacity.drag_stopped() || (resp_opacity.changed() && !resp_opacity.dragged());
    if opacity_committed {
        st.bg_opacity_drag = None;
        settings.appearance.background.opacity = opacity_preview;
        if (before_opacity - opacity_preview).abs() > f32::EPSILON {
            out.background_params_changed = true;
        }
    }

    ui.add_space(8.0);

    // —— 暗化滑块 ——
    ui.label(egui::RichText::new("暗化").color(pal.fg));
    let before_dim = settings.appearance.background.dim;
    let mut dim_preview = st.bg_dim_drag.unwrap_or(settings.appearance.background.dim);
    let resp_dim = ui.add(
        egui::Slider::new(
            &mut dim_preview,
            settings::BACKGROUND_DIM_MIN..=settings::BACKGROUND_DIM_MAX,
        )
        .step_by(0.05)
        .fixed_decimals(2),
    );
    if resp_dim.dragged() {
        st.bg_dim_drag = Some(dim_preview);
        settings.appearance.background.dim = dim_preview;
    }
    let dim_committed = resp_dim.drag_stopped() || (resp_dim.changed() && !resp_dim.dragged());
    if dim_committed {
        st.bg_dim_drag = None;
        settings.appearance.background.dim = dim_preview;
        if (before_dim - dim_preview).abs() > f32::EPSILON {
            out.background_params_changed = true;
        }
    }

    ui.label(
        egui::RichText::new("0% = 不暗化；90% = 最暗（可增强文字可读性）")
            .size(11.0)
            .color(pal.fg_dim),
    );
}

/// 一张主题画廊卡片（P12）：迷你终端预览 + 名字标签，整卡可点。
///
/// 点击语义：Sync with OS 关闭时设为当前主题；开启时按卡片明暗写
/// 入对应槽位（点深色卡 = 指定深色槽，浅色同理），两个槽位卡片带
/// 「深色/浅色」徽标。当前生效主题 accent 描边高亮。
fn theme_card(
    ui: &mut egui::Ui,
    info: &lumen_renderer::themes::ThemeInfo,
    settings: &mut Settings,
    pal: &Palette,
    out: &mut SettingsOutput,
    os_dark: bool,
) {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(THEME_CARD_W, THEME_CARD_H), egui::Sense::click());
    let sync = settings.appearance.sync_with_os;
    let effective = settings.effective_theme_id(os_dark) == info.id;
    // Sync 开启时的槽位归属（徽标 + 非生效槽位的次级描边）。
    let slot = sync.then(|| {
        if info.light {
            settings.appearance.light_theme_id == info.id
        } else {
            settings.appearance.dark_theme_id == info.id
        }
    });

    if ui.is_rect_visible(rect) {
        let prev =
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), THEME_CARD_PREVIEW_H));
        paint_theme_preview(ui.painter(), prev, &info.theme());
        // 描边：生效主题 accent 2px > 槽位/悬停 fg_dim 1px > 平时分隔灰。
        let stroke = if effective {
            egui::Stroke::new(2.0, pal.accent)
        } else if slot == Some(true) || resp.hovered() {
            egui::Stroke::new(1.0, pal.fg_dim)
        } else {
            egui::Stroke::new(1.0, pal.bg_highlight)
        };
        ui.painter()
            .rect_stroke(prev, 6.0, stroke, egui::StrokeKind::Inside);
        // 槽位徽标（仅 Sync 开启时）：右上角小标签。
        if slot == Some(true) {
            let text = if info.light { "浅色" } else { "深色" };
            let galley = ui.painter().layout_no_wrap(
                text.to_owned(),
                egui::FontId::proportional(10.0),
                pal.accent_fg,
            );
            let pad = egui::vec2(5.0, 2.0);
            let badge = egui::Rect::from_min_size(
                egui::pos2(
                    prev.max.x - galley.size().x - pad.x * 2.0 - 5.0,
                    prev.min.y + 5.0,
                ),
                galley.size() + pad * 2.0,
            );
            ui.painter().rect_filled(badge, 4.0, pal.accent);
            ui.painter().galley(badge.min + pad, galley, pal.accent_fg);
        }
        // 名字标签（生效主题用主文字色突出）。
        ui.painter().text(
            egui::pos2(rect.min.x + 2.0, rect.max.y - 10.0),
            egui::Align2::LEFT_CENTER,
            info.name,
            egui::FontId::proportional(12.0),
            if effective { pal.fg } else { pal.fg_dim },
        );
    }

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        let ap = &mut settings.appearance;
        let target = if sync {
            if info.light {
                &mut ap.light_theme_id
            } else {
                &mut ap.dark_theme_id
            }
        } else {
            &mut ap.theme
        };
        if *target != info.id {
            *target = info.id.to_owned();
            out.theme_changed = true;
        }
    }
}

/// 在 `rect` 内画主题的迷你终端缩略图：bg 底 + 几行模拟的彩色
/// 文字色条 + 光标块（对照 Warp 截图左栏的预览卡形态）。空间不足
/// （小尺寸预览）时尾部行自动省略。
fn paint_theme_preview(painter: &egui::Painter, rect: egui::Rect, t: &lumen_renderer::Theme) {
    use super::theme::c32;
    painter.rect_filled(rect, 6.0, c32(t.background));
    let pad = 9.0;
    let line_h = 12.0;
    let bar_h = 5.0;
    let x = rect.min.x + pad;
    let mut y = rect.min.y + pad;
    let fits = |y: f32, h: f32| y + h <= rect.max.y - pad + 1.0;
    let bar = |x0: f32, y0: f32, w: f32, c: lumen_renderer::Rgb| {
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(w, bar_h)),
            2.0,
            c32(c),
        );
    };
    // 行1：提示符（绿）+ 命令（fg）。
    if fits(y, bar_h) {
        bar(x, y, 8.0, t.ansi[2]);
        bar(x + 12.0, y, 42.0, t.foreground);
        y += line_h;
    }
    // 行2：路径（蓝）+ 参数（黄）。
    if fits(y, bar_h) {
        bar(x, y, 26.0, t.ansi[4]);
        bar(x + 30.0, y, 18.0, t.ansi[3]);
        y += line_h;
    }
    // 行3：错误（红）+ 输出（品红）+ 备注（青）。
    if fits(y, bar_h) {
        bar(x, y, 14.0, t.ansi[1]);
        bar(x + 18.0, y, 22.0, t.ansi[5]);
        bar(x + 44.0, y, 12.0, t.ansi[6]);
        y += line_h;
    }
    // 行4：光标块。
    if fits(y, 8.0) {
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(5.0, 8.0)),
            1.0,
            c32(t.cursor),
        );
    }
}

/// Warp 风格开关（egui 无内置 switch）：圆角轨道 + 滑动圆点，开 =
/// accent 轨道（黑白化：深色板白轨黑点 / 浅色板黑轨白点）。
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool, pal: &Palette) -> egui::Response {
    let size = egui::vec2(36.0, 19.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let radius = rect.height() / 2.0;
    let track = if *on { pal.accent } else { pal.btn_bg };
    ui.painter().rect_filled(rect, radius, track);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, pal.bg_highlight),
        egui::StrokeKind::Inside,
    );
    let cx = egui::lerp((rect.min.x + radius)..=(rect.max.x - radius), t);
    let knob = if *on { pal.accent_fg } else { pal.fg_dim };
    ui.painter()
        .circle_filled(egui::pos2(cx, rect.center().y), radius - 3.5, knob);
    resp
}

/// Keyboard shortcuts：只读列表（表驱动）。
fn shortcuts(ui: &mut egui::Ui, pal: &Palette) {
    heading(ui, pal, "Keyboard shortcuts");
    egui::Grid::new("lumen_shortcut_grid")
        .num_columns(2)
        .spacing([32.0, 8.0])
        .show(ui, |ui| {
            for (keys, desc) in SHORTCUTS {
                ui.label(egui::RichText::new(*keys).monospace().color(pal.fg));
                ui.label(egui::RichText::new(*desc).color(pal.fg_dim));
                ui.end_row();
            }
        });
}

/// About：产品名 / 版本 / 技术栈。
fn about(ui: &mut egui::Ui, pal: &Palette) {
    heading(ui, pal, "About");
    ui.label(
        egui::RichText::new("Lumen")
            .size(16.0)
            .strong()
            .color(pal.fg),
    );
    ui.label(egui::RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION"))).color(pal.fg_dim));
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new("Rust · winit · wgpu · egui · glyphon · ConPTY")
            .size(11.0)
            .color(pal.fg_dim),
    );
}
