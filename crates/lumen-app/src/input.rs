//! 键盘事件 → VT 输入序列编码。

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// ConPTY win32-input-mode 编码：`CSI Vk;Sc;Uc;Kd;Cs;Rc _`。
///
/// 按键直达 conhost 输入队列，绕过「VT 字符流→猜按键」的兼容
/// 解析路径（该路径在高频输入下是吞吐瓶颈）。Windows Terminal
/// 即以此方式投递输入。按下与抬起都应调用（Kd 区分）。
pub fn encode_key_win32(event: &KeyEvent, mods: ModifiersState, down: bool) -> Option<Vec<u8>> {
    let (vk, uc) = match &event.logical_key {
        Key::Character(s) => {
            let ch = s.chars().next()?;
            let vk = if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase() as u16
            } else {
                0 // conhost 接受 Vk=0 的纯字符事件
            };
            (vk, ch as u32)
        }
        Key::Named(named) => {
            let vk: u16 = match named {
                NamedKey::Enter => 0x0D,
                NamedKey::Tab => 0x09,
                NamedKey::Backspace => 0x08,
                NamedKey::Escape => 0x1B,
                NamedKey::Space => 0x20,
                NamedKey::PageUp => 0x21,
                NamedKey::PageDown => 0x22,
                NamedKey::End => 0x23,
                NamedKey::Home => 0x24,
                NamedKey::ArrowLeft => 0x25,
                NamedKey::ArrowUp => 0x26,
                NamedKey::ArrowRight => 0x27,
                NamedKey::ArrowDown => 0x28,
                NamedKey::Insert => 0x2D,
                NamedKey::Delete => 0x2E,
                NamedKey::F1 => 0x70,
                NamedKey::F2 => 0x71,
                NamedKey::F3 => 0x72,
                NamedKey::F4 => 0x73,
                NamedKey::F5 => 0x74,
                NamedKey::F6 => 0x75,
                NamedKey::F7 => 0x76,
                NamedKey::F8 => 0x77,
                NamedKey::F9 => 0x78,
                NamedKey::F10 => 0x79,
                NamedKey::F11 => 0x7A,
                NamedKey::F12 => 0x7B,
                _ => return None,
            };
            let uc = match named {
                NamedKey::Enter => 0x0D,
                NamedKey::Tab => 0x09,
                NamedKey::Backspace => 0x08,
                NamedKey::Escape => 0x1B,
                NamedKey::Space => 0x20,
                _ => 0,
            };
            (vk, uc)
        }
        _ => return None,
    };

    // dwControlKeyState 位。
    let mut cs = 0u32;
    if mods.shift_key() {
        cs |= 0x0010; // SHIFT_PRESSED
    }
    if mods.control_key() {
        cs |= 0x0008; // LEFT_CTRL_PRESSED
    }
    if mods.alt_key() {
        cs |= 0x0002; // LEFT_ALT_PRESSED
    }

    let kd = if down { 1 } else { 0 };
    Some(format!("\x1b[{vk};0;{uc};{kd};{cs};1_").into_bytes())
}

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
