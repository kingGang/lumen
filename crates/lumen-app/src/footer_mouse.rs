//! footer 输入框鼠标事件处理（M4.1 批E-鼠，第十一轮反馈）。
//!
//! 仅在 `input-editor` feature 开启时编译。
//! 纯函数部分均可独立单测，不依赖 winit/wgpu/egui。
//!
//! # 职责
//! - 像素坐标 → 编辑器 [`lumen_editor::Position`] 映射（宽字符对齐）。
//! - click-count 状态机（单击/双击/三击，含超时与位移重置）。
//! - 词边界选择（双击选词，与 lumen-editor 词边界一致）。
//! - 行选择（三击选行）。
//!
//! # 设计约定
//! - 所有坐标转换直接复用 `lumen_editor::display_col_to_byte`，不重复实现。
//! - 词边界复用 `lumen_editor::word_start_left` / `word_end_right`，保证一致。

#[cfg(feature = "input-editor")]
pub use inner::*;

#[cfg(feature = "input-editor")]
mod inner {
    use std::time::{Duration, Instant};

    use lumen_editor::{display_col_to_byte, word_end_right, word_start_left};
    use lumen_editor::{EditAction, Position, Selection};

    // ─── 常量 ────────────────────────────────────────────────────────────────

    /// 双击判定间隔（同位置连续点击的最大时间差）。
    const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
    /// 双击位移容差（列单位；超过此值视为移位，重置 click-count）。
    const CLICK_POSITION_TOLERANCE: usize = 1;

    // ─── ClickState 状态机 ──────────────────────────────────────────────────

    /// click-count 状态机（单击/双击/三击判定）。
    ///
    /// 调用方在每次按下时调用 [`ClickState::record_click`]，
    /// 根据返回的 [`ClickKind`] 决定选区语义。
    #[derive(Debug, Clone, Default)]
    pub struct ClickState {
        /// 上次点击位置（列，用于位移检测）。
        last_col: usize,
        /// 上次点击行（用于位移检测）。
        last_row: usize,
        /// 上次点击时刻。
        last_at: Option<Instant>,
        /// 当前连击计数（1=单击，2=双击，3=三击，之后循环回 1）。
        count: u32,
    }

    /// 一次点击的种类（由 click-count 决定）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ClickKind {
        /// 单击（定位光标）。
        Single,
        /// 双击（选词）。
        Double,
        /// 三击（选行）。
        Triple,
    }

    impl ClickState {
        /// 记录一次点击，返回点击类型。
        ///
        /// # Arguments
        /// * `row` - 点击所在逻辑行（0-based）。
        /// * `col` - 点击所在显示列（0-based，宽字符 2 列规则）。
        /// * `now` - 当前时刻（测试可注入；生产用 `Instant::now()`）。
        pub fn record_click(&mut self, row: usize, col: usize, now: Instant) -> ClickKind {
            let col_diff = (col as isize - self.last_col as isize).unsigned_abs();
            let same_pos = col_diff <= CLICK_POSITION_TOLERANCE && row == self.last_row;
            let within_time = self
                .last_at
                .map(|t| now.duration_since(t) <= DOUBLE_CLICK_INTERVAL)
                .unwrap_or(false);

            if same_pos && within_time && self.count > 0 {
                self.count = (self.count % 3) + 1;
            } else {
                self.count = 1;
            }

            self.last_col = col;
            self.last_row = row;
            self.last_at = Some(now);

            match self.count {
                1 => ClickKind::Single,
                2 => ClickKind::Double,
                3 => ClickKind::Triple,
                _ => ClickKind::Single,
            }
        }
    }

    // ─── 像素坐标 → Position 映射 ──────────────────────────────────────────

    /// footer 区域内相对坐标（物理像素，原点 = footer 左上角）→ 编辑器位置。
    ///
    /// 与渲染几何一致：
    /// - `cell_w` / `cell_h`：单元格物理像素宽/高（来自 `renderer.cell_size()`）。
    /// - `footer_padding`：footer 上下内边距（来自 `renderer.padding() * 0.4`）。
    /// - `lines`：各行文本（用于列→字节映射）。
    ///
    /// # 边界行为
    /// - 超出上边界：夹紧到第 0 行。
    /// - 超出下边界：夹紧到末行。
    /// - 超出行右边界：夹紧到行尾。
    /// - 空行：字节偏移 = 0。
    pub fn pixel_to_position(
        rel_x: f32,
        rel_y: f32,
        cell_w: f32,
        cell_h: f32,
        footer_padding: f32,
        lines: &[&str],
    ) -> Position {
        if lines.is_empty() {
            return Position { line: 0, byte: 0 };
        }
        // 扣除上内边距后计算行号
        let y_inner = (rel_y - footer_padding).max(0.0);
        let row_f = y_inner / cell_h.max(1.0);
        let row = (row_f.floor() as usize).min(lines.len().saturating_sub(1));

        let line_text = lines.get(row).copied().unwrap_or("");

        // 计算显示列
        let col = if rel_x <= 0.0 {
            0usize
        } else {
            (rel_x / cell_w.max(1.0)).floor() as usize
        };

        // display_col → 字节偏移（复用 lumen_editor 公开函数）
        let byte = display_col_to_byte(line_text, col);

        Position { line: row, byte }
    }

    /// 计算 footer 拖选时夹紧后的光标位置（超出边界时收在边缘）。
    ///
    /// 超出左/上边界 → 第 0 行行首；超出右/下边界 → 末行行尾。
    pub fn clamped_position(
        rel_x: f32,
        rel_y: f32,
        cell_w: f32,
        cell_h: f32,
        footer_padding: f32,
        lines: &[&str],
    ) -> Position {
        if lines.is_empty() {
            return Position { line: 0, byte: 0 };
        }

        // 行夹紧
        let y_inner = rel_y - footer_padding;
        let row = if y_inner < 0.0 {
            0
        } else {
            let r = (y_inner / cell_h.max(1.0)).floor() as usize;
            r.min(lines.len().saturating_sub(1))
        };

        let line_text = lines.get(row).copied().unwrap_or("");

        // 列夹紧：负 x 夹到行首
        if rel_x < 0.0 {
            return Position { line: row, byte: 0 };
        }

        let col = (rel_x / cell_w.max(1.0)).floor() as usize;
        let byte = display_col_to_byte(line_text, col);

        Position { line: row, byte }
    }

    // ─── 选区计算 ────────────────────────────────────────────────────────────

    /// 计算双击时的词选区（anchor=词左, cursor=词右）。
    ///
    /// # Arguments
    /// * `pos` - 双击命中位置（已换算为 editor Position）。
    /// * `line_text` - 命中行的文本内容。
    ///
    /// # Errors
    /// 此函数不返回 Result。
    pub fn word_selection(pos: Position, line_text: &str) -> Selection {
        let anchor_byte = word_start_left(line_text, pos.byte);
        let cursor_byte = word_end_right(line_text, pos.byte);
        Selection {
            anchor: Position {
                line: pos.line,
                byte: anchor_byte,
            },
            cursor: Position {
                line: pos.line,
                byte: cursor_byte,
            },
        }
    }

    /// 计算三击时的整行选区（anchor=行首, cursor=行尾）。
    ///
    /// # Arguments
    /// * `pos` - 三击命中位置（已换算为 editor Position）。
    /// * `line_text` - 命中行的文本内容。
    ///
    /// # Errors
    /// 此函数不返回 Result。
    pub fn line_selection(pos: Position, line_text: &str) -> Selection {
        Selection {
            anchor: Position {
                line: pos.line,
                byte: 0,
            },
            cursor: Position {
                line: pos.line,
                byte: line_text.len(),
            },
        }
    }

    /// 单击按下时产生的 EditAction（定位光标或 Shift+单击扩展选区）。
    ///
    /// # Arguments
    /// * `pos` - 命中位置。
    /// * `shift` - 是否按住 Shift（保留现有 anchor）。
    /// * `current_anchor` - 当前选区锚点（Shift 模式下保留）。
    ///
    /// # Errors
    /// 此函数不返回 Result。
    pub fn single_click_action(pos: Position, shift: bool, current_anchor: Position) -> EditAction {
        if shift {
            // Shift+单击：保留 anchor，cursor 移到命中处
            EditAction::SetSelection(Selection {
                anchor: current_anchor,
                cursor: pos,
            })
        } else {
            // 普通单击：anchor == cursor（纯光标）
            EditAction::SetSelection(Selection {
                anchor: pos,
                cursor: pos,
            })
        }
    }

    // ─── 单元测试 ────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;

        // ── pixel_to_position ────────────────────────────────────────────

        #[test]
        fn 单行ascii_像素转位置() {
            // "hello"，cell_w=8，cell_h=18，padding=0
            // x=16 → col 2 → byte 2
            let pos = pixel_to_position(16.0, 0.0, 8.0, 18.0, 0.0, &["hello"]);
            assert_eq!(pos.line, 0);
            assert_eq!(pos.byte, 2);
        }

        #[test]
        fn 单行cjk_像素转位置() {
            // "中文"：中=3字节UTF-8，显示宽度2；文=3字节，显示宽度2
            // cell_w=8，列 2 → col=2 → display_col_to_byte("中文", 2) = 3
            let pos = pixel_to_position(16.0, 0.0, 8.0, 18.0, 0.0, &["中文"]);
            assert_eq!(pos.line, 0);
            assert_eq!(pos.byte, 3); // 第二个汉字的字节起始
        }

        #[test]
        fn 像素越界_夹紧行首() {
            // 负 x → 行首
            let pos = pixel_to_position(-10.0, 0.0, 8.0, 18.0, 0.0, &["hello"]);
            assert_eq!(pos.byte, 0);
        }

        #[test]
        fn 像素越界_夹紧行尾() {
            // x 超出 "hello"（5字节）宽度 → 行尾
            let pos = pixel_to_position(999.0, 0.0, 8.0, 18.0, 0.0, &["hello"]);
            assert_eq!(pos.byte, 5);
        }

        #[test]
        fn 多行_行号正确() {
            // 两行，cell_h=18，padding=4
            // y=22 → y_inner = 22-4 = 18 → row = floor(18/18) = 1 → 行1
            let pos = pixel_to_position(0.0, 22.0, 8.0, 18.0, 4.0, &["first", "second"]);
            assert_eq!(pos.line, 1);
        }

        #[test]
        fn 空行_字节偏移为零() {
            let pos = pixel_to_position(0.0, 0.0, 8.0, 18.0, 0.0, &[""]);
            assert_eq!(pos.byte, 0);
        }

        #[test]
        fn 行越界_夹紧到末行() {
            // y 非常大 → 末行
            let pos = pixel_to_position(0.0, 9999.0, 8.0, 18.0, 0.0, &["first", "second"]);
            assert_eq!(pos.line, 1);
        }

        // ── ClickState 状态机 ────────────────────────────────────────────

        #[test]
        fn 单击() {
            let mut cs = ClickState::default();
            let t = Instant::now();
            assert_eq!(cs.record_click(0, 5, t), ClickKind::Single);
        }

        #[test]
        fn 双击() {
            let mut cs = ClickState::default();
            let t = Instant::now();
            cs.record_click(0, 5, t);
            assert_eq!(cs.record_click(0, 5, t), ClickKind::Double);
        }

        #[test]
        fn 三击() {
            let mut cs = ClickState::default();
            let t = Instant::now();
            cs.record_click(0, 5, t);
            cs.record_click(0, 5, t);
            assert_eq!(cs.record_click(0, 5, t), ClickKind::Triple);
        }

        #[test]
        fn 超时重置为单击() {
            let mut cs = ClickState::default();
            let t1 = Instant::now();
            cs.record_click(0, 5, t1);
            // 超时后：600ms 后再次点击应重置
            let t2 = t1 + Duration::from_millis(600);
            assert_eq!(cs.record_click(0, 5, t2), ClickKind::Single);
        }

        #[test]
        fn 位移重置为单击() {
            let mut cs = ClickState::default();
            let t = Instant::now();
            cs.record_click(0, 5, t);
            // 移位超过容差（>1 列）→ 重置为单击
            assert_eq!(cs.record_click(0, 10, t), ClickKind::Single);
        }

        #[test]
        fn 容差内不重置() {
            let mut cs = ClickState::default();
            let t = Instant::now();
            cs.record_click(0, 5, t);
            // 容差内（±1 列）仍算双击
            assert_eq!(cs.record_click(0, 6, t), ClickKind::Double);
        }

        // ── 词边界 ────────────────────────────────────────────────────────

        #[test]
        fn 选词_ascii() {
            // "hello world" 中，pos.byte=7（'o' 之后）
            let pos = Position { line: 0, byte: 7 };
            let sel = word_selection(pos, "hello world");
            assert_eq!(sel.anchor.byte, 6); // "world" 起始
            assert_eq!(sel.cursor.byte, 11); // "world" 结束
        }

        #[test]
        fn 选词_cjk连续() {
            // "你好世界" 全部是同一类（字母数字/汉字），应选全部
            let pos = Position { line: 0, byte: 3 }; // 第二个汉字
            let sel = word_selection(pos, "你好世界");
            assert_eq!(sel.anchor.byte, 0);
            assert_eq!(sel.cursor.byte, 12); // 4个汉字各3字节
        }

        #[test]
        fn 选行() {
            let pos = Position { line: 0, byte: 3 };
            let sel = line_selection(pos, "hello");
            assert_eq!(sel.anchor.byte, 0);
            assert_eq!(sel.cursor.byte, 5);
        }
    }
}
