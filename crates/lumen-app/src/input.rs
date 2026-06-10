//! 键盘事件 → VT 输入序列编码。

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// 把一次按键编码成要写入 PTY 的字节。返回 None 表示不产生输入。
pub fn encode_key(event: &KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();

    let mut bytes: Vec<u8> = match &event.logical_key {
        Key::Named(named) => encode_named(*named, mods)?,
        Key::Character(s) => {
            if ctrl {
                // Ctrl+字母 → C0 控制字节；Ctrl+其他常用组合按惯例映射。
                let c = s.chars().next()?;
                match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => vec![c as u8 - b'a' + 1],
                    ' ' | '@' => vec![0x00],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    _ => return None,
                }
            } else {
                event
                    .text
                    .as_ref()
                    .map(|t| t.as_bytes().to_vec())
                    .unwrap_or_else(|| s.as_bytes().to_vec())
            }
        }
        _ => return None,
    };

    // Alt 前缀（ESC）：终端惯例 Meta 键编码。
    if alt && !bytes.is_empty() && bytes[0] != 0x1b {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn encode_named(key: NamedKey, mods: ModifiersState) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        NamedKey::Enter => b"\r",
        NamedKey::Tab => {
            if mods.shift_key() {
                b"\x1b[Z"
            } else {
                b"\t"
            }
        }
        NamedKey::Space => b" ",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Escape => b"\x1b",
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::Insert => b"\x1b[2~",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::F1 => b"\x1bOP",
        NamedKey::F2 => b"\x1bOQ",
        NamedKey::F3 => b"\x1bOR",
        NamedKey::F4 => b"\x1bOS",
        NamedKey::F5 => b"\x1b[15~",
        NamedKey::F6 => b"\x1b[17~",
        NamedKey::F7 => b"\x1b[18~",
        NamedKey::F8 => b"\x1b[19~",
        NamedKey::F9 => b"\x1b[20~",
        NamedKey::F10 => b"\x1b[21~",
        NamedKey::F11 => b"\x1b[23~",
        NamedKey::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}
