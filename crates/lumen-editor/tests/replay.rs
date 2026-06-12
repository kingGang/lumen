//! Replay 测试（设计稿不变量）：
//! EditAction 序列 serde 序列化 → 反序列化 → 在新 Editor 重放 → 终态与原 Editor 完全一致。
//!
//! 覆盖三种风格序列：
//! 1. 纯 ASCII 打字 + 选区 + Undo
//! 2. 中文多行 + DeleteBackward + SetText
//! 3. Emoji ZWJ 序列 + Move + Undo 交错

use lumen_editor::{EditAction, Editor, Motion, Position, Selection};

/// 对 Editor 状态做全量比较（文本 + 光标 + 选区 + revision）。
fn assert_editors_eq(original: &Editor, replayed: &Editor, label: &str) {
    let ov = original.view();
    let rv = replayed.view();
    assert_eq!(
        ov.text(),
        rv.text(),
        "[{label}] 文本不一致：original={:?} replayed={:?}",
        ov.text(),
        rv.text()
    );
    assert_eq!(
        ov.cursor(),
        rv.cursor(),
        "[{label}] 光标不一致：original={:?} replayed={:?}",
        ov.cursor(),
        rv.cursor()
    );
    assert_eq!(
        ov.selection(),
        rv.selection(),
        "[{label}] 选区不一致：original={:?} replayed={:?}",
        ov.selection(),
        rv.selection()
    );
    assert_eq!(
        original.revision(),
        replayed.revision(),
        "[{label}] revision 不一致：original={} replayed={}",
        original.revision(),
        replayed.revision()
    );
}

/// 序列化 → 反序列化 → 重放，返回重放后的 Editor。
fn serde_replay(actions: &[EditAction]) -> Editor {
    // 序列化
    let json = serde_json::to_string(actions).expect("序列化失败");
    // 反序列化
    let decoded: Vec<EditAction> = serde_json::from_str(&json).expect("反序列化失败");
    assert_eq!(actions, decoded.as_slice(), "serde 往返后 Action 不一致");
    // 重放
    let mut editor = Editor::default();
    for a in &decoded {
        editor.apply(a);
    }
    editor
}

// ── 序列 1：ASCII 打字 + 选区扩展 + Undo ──────────────────────────────────────

#[test]
fn test_replay_ascii打字选区undo() {
    let actions = vec![
        EditAction::InsertText("hello".to_string()),
        EditAction::InsertText(" ".to_string()),
        EditAction::InsertText("world".to_string()),
        EditAction::Move {
            motion: Motion::WordLeft,
            extend: false,
        },
        EditAction::Move {
            motion: Motion::WordLeft,
            extend: false,
        },
        // Shift+End 选到行尾
        EditAction::Move {
            motion: Motion::LineEnd,
            extend: true,
        },
        EditAction::DeleteBackward, // 删除选区
        EditAction::InsertText("Rust".to_string()),
        EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        },
        // Undo 撤销 InsertText("Rust") 之前的 DeleteBackward
        EditAction::Undo,
        EditAction::Undo,
    ];

    let mut original = Editor::default();
    for a in &actions {
        original.apply(a);
    }

    let replayed = serde_replay(&actions);
    assert_editors_eq(&original, &replayed, "ascii打字选区undo");
}

// ── 序列 2：中文多行 + DeleteBackward + SetText ────────────────────────────────

#[test]
fn test_replay_中文多行settext() {
    let actions = vec![
        EditAction::InsertText("你好，".to_string()),
        EditAction::InsertNewline,
        EditAction::InsertText("世界！".to_string()),
        EditAction::Move {
            motion: Motion::Up,
            extend: false,
        },
        EditAction::Move {
            motion: Motion::LineEnd,
            extend: false,
        },
        EditAction::DeleteBackward, // 删掉 "，"
        EditAction::InsertText("！".to_string()),
        EditAction::Move {
            motion: Motion::DocEnd,
            extend: false,
        },
        EditAction::DeleteBackward,
        EditAction::DeleteBackward,
        EditAction::SetText("最终文本".to_string()),
        EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        },
        EditAction::Undo, // 撤销 SetText
    ];

    let mut original = Editor::default();
    for a in &actions {
        original.apply(a);
    }

    let replayed = serde_replay(&actions);
    assert_editors_eq(&original, &replayed, "中文多行settext");
}

// ── 序列 3：Emoji ZWJ + Move + Undo 交错 ─────────────────────────────────────

#[test]
fn test_replay_emoji_zwj_undo交错() {
    let family_emoji = "👨‍👩‍👧‍👦"; // ZWJ 序列
    let actions = vec![
        EditAction::InsertText(format!("A{family_emoji}B")),
        // 光标在末尾，向左移一 grapheme → 跳过 B
        EditAction::Move {
            motion: Motion::GraphemeLeft,
            extend: false,
        },
        // Shift+Left 选中 emoji
        EditAction::Move {
            motion: Motion::GraphemeLeft,
            extend: true,
        },
        // 删除选中 emoji
        EditAction::DeleteForward,
        EditAction::InsertText("C".to_string()),
        // Undo × 2
        EditAction::Undo,
        EditAction::Undo,
        EditAction::Move {
            motion: Motion::DocStart,
            extend: false,
        },
        EditAction::SelectAll,
        EditAction::InsertText("重置".to_string()),
    ];

    let mut original = Editor::default();
    for a in &actions {
        original.apply(a);
    }

    let replayed = serde_replay(&actions);
    assert_editors_eq(&original, &replayed, "emoji_zwj_undo交错");
}

// ── 额外：SetSelection 越界 replay 确定性 ────────────────────────────────────

#[test]
fn test_replay_越界选区夹紧确定性() {
    let actions = vec![
        EditAction::InsertText("abc".to_string()),
        EditAction::SetSelection(Selection {
            anchor: Position { line: 0, byte: 100 }, // 越界
            cursor: Position { line: 0, byte: 1 },
        }),
        EditAction::DeleteBackward,
        EditAction::InsertText("X".to_string()),
    ];

    let mut original = Editor::default();
    for a in &actions {
        original.apply(a);
    }

    let replayed = serde_replay(&actions);
    assert_editors_eq(&original, &replayed, "越界选区夹紧确定性");
}

// ── 空序列 replay ─────────────────────────────────────────────────────────────

#[test]
fn test_replay_空序列() {
    let actions: Vec<EditAction> = vec![];
    let original = Editor::default();
    let replayed = serde_replay(&actions);
    assert_editors_eq(&original, &replayed, "空序列");
}
