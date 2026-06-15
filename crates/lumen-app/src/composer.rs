//! footer 输入区视图组装（M4.1 批D1，feature = "input-editor"）——设计稿 §7.1。
//!
//! app 层按 [`effective_mode`] 的结果组装 [`ComposerView`] 传给 renderer；
//! renderer 不依赖 lumen-editor，数据流：
//! `mode → compose_view_for_mode → Option<&ComposerView> → renderer.render(...)`。
//!
//! # 节拍纪律（设计稿 §7.4）
//! 编辑器变更直接 `request_redraw()`，不触碰 PTY debounce 字段。
//! 本模块函数是纯函数，不持有状态，每帧按需调用。
//!
//! # 设计稿对应章节
//! 设计稿 §2「输入区形态」列、§7.1「footer 输入区」、§7.4「节拍纪律」。
//!
//! # Ghost text（M4.1 批3）
//! PSReadLine 内建预测已在 shell 侧禁用（Set-PSReadLineOption -PredictionSource None），
//! 本模块实现历史前缀匹配 ghost text 作为替代，保留补全体验。
//! ghost = history.find_ghost_prefix(current_text)，由 main.rs 计算后作为参数传入。

#[cfg(feature = "input-editor")]
use crate::i18n;
use crate::mode::InputMode;
use lumen_renderer::composer_view::ComposerView;

#[cfg(feature = "input-editor")]
use lumen_editor::EditorView;
#[cfg(feature = "input-editor")]
use lumen_renderer::composer_view::{normalize_selection, ExitBadge, FooterSpan, PreeditState};

/// 把 lumen-editor 的高亮 [`lumen_editor::Token`] 翻译为 renderer 的 [`FooterSpan`]
/// （M4.2 批2）——renderer 不依赖 lumen-editor，跨 crate 边界在此收口（设计稿 §5）。
#[cfg(feature = "input-editor")]
fn token_to_footer_span(t: &lumen_editor::Token) -> FooterSpan {
    use lumen_editor::TokenKind as E;
    use lumen_renderer::composer_view::FooterTokenKind as F;
    let kind = match t.kind {
        E::Command => F::Command,
        E::Keyword => F::Keyword,
        E::Parameter => F::Parameter,
        E::Variable => F::Variable,
        E::Number => F::Number,
        E::StringLit => F::StringLit,
        E::Operator => F::Operator,
        E::Comment => F::Comment,
        E::Text => F::Text,
    };
    FooterSpan {
        start: t.start,
        end: t.end,
        kind,
    }
}

/// 按当前有效输入模式组装 [`ComposerView`]。
///
/// - [`InputMode::Compose`] → 完整编辑卡片，内容来自 `editor_view`（真实 EditorView 内容）
/// - [`InputMode::Running`] → 等高状态条（文案 i18n）
/// - [`InputMode::AltScreen`] → 隐藏（grid 收回全高）
/// - [`InputMode::Fallback`] → 隐藏（与 AltScreen 同路径；底部状态栏已有"经典直通"模式指示）
///
/// # 设计稿铁律
/// Compose ↔ Running 切换返回等高视图（均为 1 行），不改 footer 高度，
/// 不触发 `term.resize` / `pty.resize`（防 resize 风暴，设计稿 §7.1）。
/// Fallback 隐藏（footer_px=0），grid 收回全高，与 AltScreen 行为一致。
///
/// # Arguments
/// * `mode` - 当前有效输入模式（由 [`crate::mode::effective_mode`] 推导）。
/// * `editor_view` - Compose 态时的编辑器只读视图（仅 Compose 态读取内容）。
/// * `preedit` - IME 预编辑状态（M4.1 批D2，仅 Compose 态有效）。
/// * `exit_badge` - 退出码角标（M4.1 批D2，仅 Compose 态显示）。
/// * `ghost` - 历史联想后缀（M4.1 批3）：Compose 态 + 光标在文末时追加渲染；
///   None 表示无联想（缓冲为空、多行或无前缀匹配）。
#[cfg(feature = "input-editor")]
pub fn compose_view_for_mode(
    mode: InputMode,
    editor_view: EditorView<'_>,
    preedit: Option<PreeditState>,
    exit_badge: Option<ExitBadge>,
    ghost: Option<String>,
) -> ComposerView {
    let s = i18n::strings();
    match mode {
        InputMode::Compose => {
            // 批D1：从真实 EditorView 读取内容，填充 lines 和 cursor。
            let lines: Vec<String> = editor_view
                .lines()
                .map(|l| l.to_owned())
                .collect::<Vec<_>>()
                .into_iter()
                .collect();
            let lines = if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            };
            let cur = editor_view.cursor();
            // 占位提示（M4.1 批E）：lines 全空时显示 composer_placeholder。
            let all_empty = lines.iter().all(|l| l.is_empty());
            let placeholder = all_empty.then(|| s.composer_placeholder.to_owned());
            // M4.1 批F：从 EditorView 取选区，规范化后填入 ComposerView。
            // anchor ≠ cursor 时为非空选区；normalize_selection 返回 Some(FooterSelection)。
            // 有选区时 ghost text 不显示（视觉冲突，系统惯例选区时无 inline 补全）。
            let editor_sel = editor_view.selection();
            let selection = normalize_selection(
                (editor_sel.anchor.line, editor_sel.anchor.byte),
                (editor_sel.cursor.line, editor_sel.cursor.byte),
            );
            let ghost = if selection.is_some() { None } else { ghost };
            // M4.2 批2：语法高亮——editor tokenizer 逐行产出 token，翻译为 footer span。
            // 行序与 `lines` 一致（同源 doc.lines）；renderer 按 li 索引对齐着色。
            let highlight: Vec<Vec<FooterSpan>> = editor_view
                .highlight()
                .iter()
                .map(|toks| toks.iter().map(token_to_footer_span).collect())
                .collect();
            // Position { line, byte }——与 ComposerView.cursor (行, 字节偏移) 语义相同
            ComposerView {
                kind: lumen_renderer::composer_view::FooterKind::Composer,
                lines,
                cursor: (cur.line, cur.byte),
                selection,   // M4.1 批F：文本选区（规范化后，None=纯光标）
                preedit,     // M4.1 批D2：IME 预编辑
                exit_badge,  // M4.1 批D2：退出码角标
                placeholder, // M4.1 批E：占位提示（仅空编辑器显示）
                ghost,       // M4.1 批3：历史联想后缀（有选区时为 None）
                highlight,   // M4.2 批2：语法高亮 spans（逐行）
            }
        }
        // Running（命令运行中）：footer 隐藏（海风哥反馈——命令运行时不要那条
        // 「运行中…（直通模式）」横条，让终端内容占满到底）。footer_px=0、grid
        // 收回全高，与 AltScreen/Fallback 同路径。
        //
        // ⚠ 取舍（勿轻易回退）：此前 Running 刻意产 StatusBar（与 Compose 等高）
        // 以避免 Compose↔Running 切换触发 term.resize/pty.resize（设计稿 §7.1
        // 防 resize 风暴）。现按海风哥要求改隐藏，代价是每条命令起/止时 footer
        // 高度在「1 行 ↔ 0」间变化，底部约 1 行高度轻微抖动并触发一次 resize——
        // 这是有意接受的权衡（用户要彻底隐藏 > 避免抖动）。
        InputMode::Running => ComposerView::hidden(),
        // AltScreen：全屏 TUI 让位，footer 隐藏，grid 收回全高。
        InputMode::AltScreen => ComposerView::hidden(),
        // Fallback：shell integration 未生效，footer 隐藏（与 AltScreen 同路径，
        // footer_px=0，grid 收回全高；底部状态栏已有"经典直通"模式指示）。
        InputMode::Fallback => ComposerView::hidden(),
    }
}

/// 无 input-editor feature 时的退化版本（保持原批C 兼容行为）。
#[cfg(not(feature = "input-editor"))]
pub fn compose_view_for_mode(mode: InputMode) -> ComposerView {
    match mode {
        InputMode::Compose => ComposerView::compose_empty(),
        // Running 也隐藏（海风哥反馈，取舍详见 feature 版注释）——与
        // AltScreen/Fallback 同路径，footer_px=0、grid 收回全高。
        InputMode::Running | InputMode::AltScreen | InputMode::Fallback => {
            ComposerView::hidden()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_renderer::composer_view::FooterKind;

    // ── 四模式 → 形态映射（input-editor feature）────────────────────────

    #[cfg(feature = "input-editor")]
    mod with_editor {
        use super::*;

        /// 构造一个空 Editor，拿 EditorView 供测试。
        fn empty_view() -> lumen_editor::Editor {
            lumen_editor::Editor::default()
        }

        #[test]
        fn compose_模式_产出_composer_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Composer,
                "Compose 模式应产出 Composer 形态"
            );
            assert!(v.is_visible(), "Compose 形态应可见");
        }

        #[test]
        fn 高亮_compose态填充token_spans() {
            use lumen_editor::{EditAction, Editor};
            use lumen_renderer::composer_view::FooterTokenKind;

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText(
                "Get-ChildItem -Recurse".to_string(),
            ));
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);

            // highlight 行数与 lines 对齐，首行有 token spans（翻译链路打通）。
            assert_eq!(
                v.highlight.len(),
                v.lines.len(),
                "highlight 行数应与 lines 一致"
            );
            let first = &v.highlight[0];
            assert!(!first.is_empty(), "首行应产出 token spans");
            // 命令名翻译为 Command、参数翻译为 Parameter（editor→FooterSpan 映射正确）。
            assert!(
                first.iter().any(|s| s.kind == FooterTokenKind::Command
                    && v.lines[0].get(s.start..s.end) == Some("Get-ChildItem")),
                "Get-ChildItem 应翻译为 Command span"
            );
            assert!(
                first.iter().any(|s| s.kind == FooterTokenKind::Parameter
                    && v.lines[0].get(s.start..s.end) == Some("-Recurse")),
                "-Recurse 应翻译为 Parameter span"
            );
        }

        #[test]
        fn 高亮_非compose态不填充() {
            use lumen_editor::Editor;
            let editor = Editor::default();
            // Running 态走 ComposerView::running()，highlight 默认空。
            let v = compose_view_for_mode(InputMode::Running, editor.view(), None, None, None);
            assert!(v.highlight.is_empty(), "Running 态不应填充 highlight");
        }

        #[test]
        fn compose_空编辑器_一行() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(v.lines.len(), 1, "空编辑器应有 1 行");
            assert_eq!(v.cursor, (0, 0), "空编辑器光标在原点");
        }

        #[test]
        fn running_模式_产出_hidden_形态() {
            // 海风哥反馈：Running 态 footer 改为隐藏（不再显示「运行中…」横条），
            // 与 AltScreen/Fallback 同路径，footer_px=0、grid 收回全高。
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Running, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "Running 模式应产出 Hidden 形态（footer 隐藏）"
            );
            assert!(!v.is_visible(), "Running 形态应隐藏");
        }

        #[test]
        fn altscreen_模式_产出_hidden_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::AltScreen, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "AltScreen 模式应产出 Hidden 形态"
            );
            assert!(!v.is_visible(), "AltScreen 形态应隐藏");
        }

        #[test]
        fn fallback_模式_产出_hidden_形态() {
            // 第十四轮：Fallback 改为隐藏（与 AltScreen 同路径），
            // footer_px=0，grid 收回全高；底部状态栏已有"经典直通"模式指示，无需重复。
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Fallback, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "Fallback 模式应产出 Hidden 形态（底部状态栏已有模式指示）"
            );
            assert!(!v.is_visible(), "Fallback 形态应隐藏");
        }

        /// Running 态隐藏（海风哥反馈后变更）：Running footer 高度为 0（彻底
        /// 隐藏），Compose 仍有高度。此前为「Compose↔Running 等高」防 resize，
        /// 现按用户要求 Running 隐藏，等高约束作废，改钉新行为。
        #[test]
        fn running_隐藏_高度为零_compose_有高度() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_compose =
                compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            let v_running =
                compose_view_for_mode(InputMode::Running, editor.view(), None, None, None);
            let h_c = footer_height_px(Some(&v_compose), 20.0, 6.0, 1000.0);
            let h_r = footer_height_px(Some(&v_running), 20.0, 6.0, 1000.0);
            assert_eq!(h_r, 0.0, "Running footer 应隐藏（高度 0）");
            assert!(h_c > 0.0, "Compose footer 应有高度");
        }

        /// AltScreen 隐藏态高度为 0（grid 收回全高）。
        #[test]
        fn altscreen_高度为零() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_alt =
                compose_view_for_mode(InputMode::AltScreen, editor.view(), None, None, None);
            assert_eq!(
                footer_height_px(Some(&v_alt), 20.0, 6.0, 1000.0),
                0.0,
                "AltScreen 高度应为 0"
            );
        }

        /// Fallback 隐藏态高度为 0（第十四轮变更：与 AltScreen 同路径）。
        #[test]
        fn fallback_高度为零() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_fb = compose_view_for_mode(InputMode::Fallback, editor.view(), None, None, None);
            let h_fb = footer_height_px(Some(&v_fb), 20.0, 6.0, 1000.0);
            assert_eq!(h_fb, 0.0, "Fallback 隐藏态高度应为 0");
        }

        /// ghost text 传入 Compose 态时应被带入 ComposerView（M4.1 批3）。
        #[test]
        fn ghost_传入_compose_视图() {
            let editor = empty_view();
            let ghost = Some("tatus --short".to_owned());
            let v =
                compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost.clone());
            assert_eq!(v.ghost, ghost, "ghost 应被带入 ComposerView");
        }

        /// ghost text 在非 Compose 态（Running）时应不影响形态（Running 忽略 ghost）。
        #[test]
        fn ghost_非compose_态_不影响形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(
                InputMode::Running,
                editor.view(),
                None,
                None,
                Some("suffix".to_owned()),
            );
            // Running 态不消费 ghost，且现产 Hidden 形态（footer 隐藏）
            assert_eq!(
                v.kind,
                lumen_renderer::composer_view::FooterKind::Hidden,
                "Running 态形态不受 ghost 影响（且为 Hidden）"
            );
        }

        // ── 反馈B：Compose 态 ghost 全链路无头测试（第十七轮）─────────────
        //
        // 验收目标：HistoryStore（内存注入）→ find_ghost_prefix → compose_view_for_mode
        // → ComposerView.ghost 字段 Some；同时验证渲染层前置条件（cur_byte ≥ 行末）。
        // 渲染层需要 GPU 无法无头执行，此处钉死数据链路正确性。

        /// 全链路 ghost 基线：注入历史 "git status" → 输入 "git s" → ghost = "tatus"。
        #[test]
        fn ghost_全链路_history注入到composerview() {
            use crate::history::{HistoryEntry, HistoryStore};
            use lumen_editor::{EditAction, Editor};

            // 构造内存 HistoryStore（不落盘），注入 "git status" 条目。
            let mut store = HistoryStore::new_in_memory();
            store.inject_entry(HistoryEntry {
                text: "git status".into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: 1,
                count: 1,
            });

            // 编辑器输入 "git s"，光标在行末（byte=5）。
            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("git s".to_string()));

            // 步骤1：find_ghost_prefix 基线——应返回 "tatus"。
            let text = editor.view().text();
            let ghost = store.find_ghost_prefix(&text);
            assert_eq!(
                ghost.as_deref(),
                Some("tatus"),
                "find_ghost_prefix(\"git s\") 应返回 \"tatus\""
            );

            // 步骤2：compose_view_for_mode 断言 ghost 字段 Some。
            let v =
                compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost.clone());
            assert_eq!(
                v.ghost.as_deref(),
                Some("tatus"),
                "ComposerView.ghost 应为 Some(\"tatus\")"
            );
            assert_eq!(v.kind, lumen_renderer::composer_view::FooterKind::Composer);
            assert!(v.selection.is_none(), "无选区时 ghost 不应被清空");

            // 步骤3：验证渲染层前置条件。
            // 渲染层条件：cur_byte >= line_text.len()（光标在行末才追加 ghost）。
            let (cur_line, cur_byte) = v.cursor;
            let line_text = v.lines.get(cur_line).map(|s| s.as_str()).unwrap_or("");
            assert!(
                cur_byte >= line_text.len(),
                "光标字节偏移 ({cur_byte}) 应 >= 行末 ({})，否则渲染层不画 ghost",
                line_text.len()
            );
        }

        /// ghost 光标不在文末时渲染层不绘制：cur_byte < line.len()。
        #[test]
        fn ghost_光标不在文末_渲染条件不满足() {
            use lumen_editor::{EditAction, Editor, Motion};

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("git status".to_string()));
            // 光标移回行首（byte=0），文本仍为 "git status"。
            editor.apply(&EditAction::Move {
                motion: Motion::LineStart,
                extend: false,
            });

            let ghost = Some("uffix".to_owned()); // 假设有 ghost，但光标不在末
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost);

            // ComposerView 中 ghost 字段值仍为 Some（app 层不管渲染条件）。
            assert!(
                v.ghost.is_some(),
                "app 层传入 ghost 时 ComposerView.ghost 应为 Some"
            );

            // 渲染层条件检查：cur_byte < line.len() → 渲染层不会画 ghost。
            let (cur_line, cur_byte) = v.cursor;
            let line_text = v.lines.get(cur_line).map(|s| s.as_str()).unwrap_or("");
            assert!(
                cur_byte < line_text.len(),
                "光标在行首时 cur_byte ({cur_byte}) 应 < line.len() ({})；渲染层不绘制 ghost",
                line_text.len()
            );
        }

        /// ghost 空串不绘制：渲染层有 `if !ghost.is_empty()` 门控。
        #[test]
        fn ghost_空串_渲染门控() {
            use crate::history::{HistoryEntry, HistoryStore};
            use lumen_editor::{EditAction, Editor};

            // 注入 "git"，前缀 "git" 完全等于条目 → find_ghost_prefix 返回 None。
            let mut store = HistoryStore::new_in_memory();
            store.inject_entry(HistoryEntry {
                text: "git".into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: 1,
                count: 1,
            });
            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("git".to_string()));
            let text = editor.view().text();
            let ghost = store.find_ghost_prefix(&text);
            // 完全匹配时 ghost 应为 None（避免空 ghost）。
            assert!(ghost.is_none(), "前缀=条目时 find_ghost_prefix 返回 None");
        }

        // ── M4.1 批F：选区传递 ──────────────────────────────────────────

        /// 空编辑器（纯光标）：selection 应为 None。
        #[test]
        fn 空编辑器_无选区() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(v.selection, None, "空编辑器应无选区");
        }

        /// 编辑器有选区时：selection 应被填入 Some(FooterSelection)，且已规范化（start ≤ end）。
        #[test]
        fn 有选区_正向_传入_compose_视图() {
            use lumen_editor::{EditAction, Editor, Motion};
            use lumen_renderer::composer_view::FooterSelection;

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            editor.apply(&EditAction::Move {
                motion: Motion::LineStart,
                extend: false,
            });
            // Shift+Right × 3 → 正向选区 anchor=(0,0) cursor=(0,3)
            for _ in 0..3 {
                editor.apply(&EditAction::Move {
                    motion: Motion::GraphemeRight,
                    extend: true,
                });
            }
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(
                v.selection,
                Some(FooterSelection {
                    start: (0, 0),
                    end: (0, 3)
                }),
                "正向选区应被正确填入"
            );
        }

        /// 逆向选区（cursor < anchor）：规范化后 start ≤ end。
        #[test]
        fn 有选区_逆向_规范化() {
            use lumen_editor::{EditAction, Editor, Motion};
            use lumen_renderer::composer_view::FooterSelection;

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            // 光标在行尾，Shift+Left × 2 → 逆向选区 anchor=(0,5) cursor=(0,3)
            for _ in 0..2 {
                editor.apply(&EditAction::Move {
                    motion: Motion::GraphemeLeft,
                    extend: true,
                });
            }
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            // 规范化后：start=(0,3) end=(0,5)
            assert_eq!(
                v.selection,
                Some(FooterSelection {
                    start: (0, 3),
                    end: (0, 5)
                }),
                "逆向选区应被规范化为 start ≤ end"
            );
        }

        /// 有选区时：ghost text 应被清空（视觉冲突，选区时无 inline 补全）。
        #[test]
        fn 有选区_ghost_被清空() {
            use lumen_editor::{EditAction, Editor, Motion};

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            editor.apply(&EditAction::Move {
                motion: Motion::LineStart,
                extend: false,
            });
            editor.apply(&EditAction::Move {
                motion: Motion::GraphemeRight,
                extend: true,
            });
            // 有选区，传入 ghost
            let ghost = Some("suffix".to_owned());
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost);
            assert!(v.selection.is_some(), "应有选区");
            assert_eq!(v.ghost, None, "有选区时 ghost 应被清空");
        }
    }

    // ── 无 input-editor feature 时的退化版（保持批C 兼容）─────────────

    #[cfg(not(feature = "input-editor"))]
    mod without_editor {
        use super::*;

        #[test]
        fn compose_模式_产出_composer_形态() {
            let v = compose_view_for_mode(InputMode::Compose);
            assert_eq!(v.kind, FooterKind::Composer);
            assert!(v.is_visible());
        }

        #[test]
        fn running_模式_产出_statusbar_形态() {
            let v = compose_view_for_mode(InputMode::Running);
            assert_eq!(v.kind, FooterKind::StatusBar);
            assert!(v.is_visible());
        }

        #[test]
        fn altscreen_fallback_产出_hidden_形态() {
            let v_alt = compose_view_for_mode(InputMode::AltScreen);
            let v_fb = compose_view_for_mode(InputMode::Fallback);
            assert_eq!(v_alt.kind, FooterKind::Hidden);
            assert_eq!(v_fb.kind, FooterKind::Hidden);
        }

        #[test]
        fn compose_与_running_等高() {
            use lumen_renderer::composer_view::footer_height_px;
            let v_compose = compose_view_for_mode(InputMode::Compose);
            let v_running = compose_view_for_mode(InputMode::Running);
            let h_c = footer_height_px(Some(&v_compose), 20.0, 6.0, 1000.0);
            let h_r = footer_height_px(Some(&v_running), 20.0, 6.0, 1000.0);
            assert_eq!(h_c, h_r);
        }
    }
}
