//! 设置界面（M3.4）：全屏覆盖层，左分类导航 + 右内容区（对标 Warp，
//! 参考截图 docs/截图/设置界面.png）。
//!
//! UI 只改 [`Settings`] 数据并产出变更标志（[`SettingsOutput`]），
//! 即时生效（renderer 字体/主题重配置、全会话 resize、egui 样式重设、
//! 写盘）由 main.rs 执行。覆盖层用 egui `Modal`：backdrop 阻断下层
//! 面板与终端区的鼠标交互（`mouse_in_term` 按层序判定自然失效），
//! Esc / 右上角 ✕ 关闭。设置页打开期间 PTY 消化与终端渲染照常进行
//! ——覆盖层只是 UI 层。

use crate::settings::{self, Settings, ThemeChoice};

use super::theme::Palette;

/// 左侧分类导航宽度（逻辑像素）。
const NAV_WIDTH: f32 = 220.0;
/// 顶栏高度（居中标题 + 关闭按钮）。
const TOP_BAR_HEIGHT: f32 = 44.0;

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
    /// 字体回退提示：main 在 reconfigure 后写入实际生效信息，
    /// None 表示请求的字体已生效（无回退）。
    pub font_hint: Option<String>,
}

impl SettingsUiState {
    /// 打开设置页，并按当前设置初始化字体下拉的自定义态。
    pub fn open_with(&mut self, settings: &Settings) {
        self.open = true;
        let fam = settings.appearance.font_family.as_str();
        self.custom_font_mode = !fam.is_empty() && !FONT_PRESETS.contains(&fam);
        self.custom_font_buf = fam.to_owned();
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
}

/// 绘制设置页覆盖层。调用方保证 `st.open == true` 时才调用。
pub fn show(
    ctx: &egui::Context,
    st: &mut SettingsUiState,
    settings: &mut Settings,
    pal: &Palette,
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
            let bar = egui::Rect::from_min_size(
                full.min,
                egui::vec2(full.width(), TOP_BAR_HEIGHT),
            );
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
            let close_btn = egui::Button::new(
                egui::RichText::new("✕").size(14.0).color(pal.fg_dim),
            )
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

            let nav_rect = egui::Rect::from_min_max(
                body.min,
                egui::pos2(body.min.x + NAV_WIDTH, body.max.y),
            )
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
                    Category::Account => account(ui, pal),
                    Category::Appearance => appearance(ui, st, settings, pal, &mut out),
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
        .fill(if selected {
            pal.bg_highlight
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

/// Account：本期占位（真登录 M3.5 接入）。
fn account(ui: &mut egui::Ui, pal: &Palette) {
    heading(ui, pal, "Account");
    ui.horizontal(|ui| {
        // 未登录占位头像：圆底 + 人形图标（egui 自带 emoji 字体覆盖）。
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 22.0, pal.bg_highlight);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "👤",
            egui::FontId::proportional(22.0),
            pal.fg_dim,
        );
        ui.vertical(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("未登录").color(pal.fg));
            ui.label(
                egui::RichText::new("登录功能将在后续版本提供")
                    .size(11.0)
                    .color(pal.fg_dim),
            );
        });
    });
}

/// Appearance：主题 / 字体 / 字号，全部即时生效。
fn appearance(
    ui: &mut egui::Ui,
    st: &mut SettingsUiState,
    settings: &mut Settings,
    pal: &Palette,
    out: &mut SettingsOutput,
) {
    heading(ui, pal, "Appearance");

    // —— 主题 ——
    ui.label(egui::RichText::new("主题").color(pal.fg));
    let before_theme = settings.appearance.theme;
    egui::ComboBox::from_id_salt("lumen_theme_combo")
        .width(240.0)
        .selected_text(settings.appearance.theme.display_name())
        .show_ui(ui, |ui| {
            for t in [ThemeChoice::TokyoNight, ThemeChoice::TokyoNightLight] {
                ui.selectable_value(&mut settings.appearance.theme, t, t.display_name());
            }
        });
    if settings.appearance.theme != before_theme {
        out.theme_changed = true;
    }
    ui.add_space(16.0);

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
            let auto_active =
                !st.custom_font_mode && settings.appearance.font_family.is_empty();
            if ui.selectable_label(auto_active, "自动（系统等宽）").clicked() {
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
            if ui.selectable_label(st.custom_font_mode, "自定义…").clicked() {
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
            let submitted =
                edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
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
    ui.label(egui::RichText::new("终端字号").color(pal.fg));
    let slider = egui::Slider::new(
        &mut settings.appearance.font_size,
        settings::FONT_SIZE_MIN..=settings::FONT_SIZE_MAX,
    )
    .step_by(1.0)
    .fixed_decimals(0);
    if ui.add(slider).changed() {
        out.font_changed = true;
    }
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
    ui.label(egui::RichText::new("Lumen").size(16.0).strong().color(pal.fg));
    ui.label(
        egui::RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION")))
            .color(pal.fg_dim),
    );
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new("Rust · winit · wgpu · egui · glyphon · ConPTY")
            .size(11.0)
            .color(pal.fg_dim),
    );
}
