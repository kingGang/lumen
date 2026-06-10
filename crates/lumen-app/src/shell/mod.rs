//! 应用外壳 UI（egui）：侧栏 + 终端工作区布局。
//!
//! M3.1 阶段侧栏是占位壳（单个会话条目 + 底部新建按钮，均无功能，
//! M3.2 接多会话）；终端以离屏纹理嵌入 CentralPanel。

pub mod theme;

/// 左侧会话栏宽度（逻辑像素）。
pub const SIDEBAR_WIDTH: f32 = 180.0;

/// 一帧外壳 UI 的产出。
pub struct ShellOutput {
    /// 终端工作区矩形（egui 逻辑点坐标）。
    pub term_rect: egui::Rect,
    /// 本帧用户点击了终端区（焦点交还终端）。
    pub term_clicked: bool,
}

/// 绘制整个外壳：左侧会话栏 + 中央终端纹理。
pub fn show(root: &mut egui::Ui, term_tex: egui::TextureId, session_title: &str) -> ShellOutput {
    egui::Panel::left("lumen_sidebar")
        .exact_size(SIDEBAR_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DARK)
                .inner_margin(egui::Margin::symmetric(8, 10)),
        )
        .show_inside(root, |ui| sidebar_ui(ui, session_title));

    let mut out = ShellOutput {
        term_rect: egui::Rect::NOTHING,
        term_clicked: false,
    };
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show_inside(root, |ui| {
            let rect = ui.available_rect_before_wrap();
            ui.put(
                rect,
                egui::Image::new(egui::load::SizedTexture::new(term_tex, rect.size())),
            );
            // 点击终端区 → 焦点交还终端。选区/块点击/滚轮仍走
            // window_event 按终端区矩形路由（见 main.rs）。
            let resp = ui.interact(rect, ui.id().with("terminal_area"), egui::Sense::click());
            out.term_clicked = resp.clicked();
            out.term_rect = rect;
        });
    out
}

/// 侧栏内容（M3.1 占位：一个会话条目 + 底部新建按钮）。
fn sidebar_ui(ui: &mut egui::Ui, session_title: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new("会话").size(11.0).color(theme::FG_DIM));
    ui.add_space(4.0);

    // 会话条目占位（高亮表示激活态；标题跟随终端 OSC 标题）。
    let title = if session_title.is_empty() {
        "PowerShell"
    } else {
        session_title
    };
    let entry = egui::Button::new(egui::RichText::new(format!("● {title}")).color(theme::FG))
        .fill(theme::BG_HIGHLIGHT)
        .wrap_mode(egui::TextWrapMode::Truncate)
        .min_size(egui::vec2(ui.available_width(), 30.0));
    let _ = ui.add(entry); // M3.2 接会话切换

    // 底部「+」新建会话按钮占位。
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(2.0);
        let plus = egui::Button::new(egui::RichText::new("＋ 新建会话").color(theme::FG_DIM))
            .min_size(egui::vec2(ui.available_width(), 28.0));
        let _ = ui.add(plus); // M3.2 接新建会话
    });
}
