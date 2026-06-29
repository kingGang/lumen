//! 鼠标上报（X10 / Normal / Button / Any 协议 + 默认 / SGR 编码）。
//!
//! 终端侧只保存「当前开启了哪种上报协议 + 哪种坐标编码」——由 DECSET
//! `9 / 1000 / 1002 / 1003`（协议）与 `1006`（SGR 编码，未开则默认 X10）
//! 设置，见 [`crate::Terminal`] 的 DECSET 解析。某次鼠标操作**要不要上报、编码成
//! 什么字节**则交给本模块的纯函数 [`encode_mouse`]：无 IO、无状态、可独立
//! 单测，上层（app）拿到协议状态后调用，再把返回字节写进 PTY。
//!
//! 这样设计的原因：上报是输入驱动的（用户动鼠标 → 编码 → 写 PTY），与
//! 终端主动回写的 DSR/DA 应答（`take_responses`）不同源，不该混在解析里。

/// 鼠标上报协议级别（互斥；DECSET `9 / 1000 / 1002 / 1003`）。
///
/// 级别决定**哪些事件上报**：按下/释放/移动/滚轮的取舍随级别递增。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseProtocol {
    /// 未开启上报（默认）：鼠标归终端本地（选区 / 滚 scrollback）。
    #[default]
    Off,
    /// X10 兼容（DEC 9）：仅上报按键**按下**，无释放、无修饰、无移动。
    X10,
    /// Normal（DEC 1000）：上报按下与释放，含修饰键；不报移动。
    Normal,
    /// Button-event（DEC 1002）：Normal + **按住拖动**时上报移动。
    Button,
    /// Any-event（DEC 1003）：Button + 无按键时的纯移动也上报。
    Any,
}

impl MouseProtocol {
    /// 是否开启了任何形式的上报。
    pub fn is_on(self) -> bool {
        self != MouseProtocol::Off
    }
}

/// 坐标 / 按钮编码方式（DECSET `1006` = SGR，其余走默认 X10 字节编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// 传统 X10 编码：`ESC [ M Cb Cx Cy`，每字节 +32，坐标上限 223。
    #[default]
    Default,
    /// SGR 扩展（DEC 1006）：`ESC [ < b ; x ; y M|m`，无坐标上限、区分按下/释放。
    Sgr,
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// X10 协议位低 2 位的按钮码（左 0 / 中 1 / 右 2）。
    fn code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }
}

/// 修饰键状态（编码进协议位：shift +4 / alt(meta) +8 / ctrl +16）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl MouseMods {
    fn bits(self) -> u8 {
        (if self.shift { 4 } else { 0 })
            + (if self.alt { 8 } else { 0 })
            + (if self.ctrl { 16 } else { 0 })
    }
}

/// 一次鼠标动作的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    /// 某键按下。
    Press(MouseButton),
    /// 某键释放。
    Release(MouseButton),
    /// 指针移动；`Some(btn)` 表示移动时该键处于按下（拖动）。
    Move(Option<MouseButton>),
    /// 滚轮上滚（按钮 64）。
    WheelUp,
    /// 滚轮下滚（按钮 65）。
    WheelDown,
}

/// 一次待编码的鼠标事件；坐标为 **0 基视口** 列 / 行（左上角为 0,0）。
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub col: usize,
    pub row: usize,
    pub mods: MouseMods,
}

/// 按当前协议判断某事件是否应上报。
fn should_report(proto: MouseProtocol, kind: MouseEventKind) -> bool {
    match kind {
        // 滚轮在任何已开启的协议下都上报（xterm 行为）。
        MouseEventKind::WheelUp | MouseEventKind::WheelDown => proto.is_on(),
        MouseEventKind::Press(_) => proto.is_on(),
        // X10 不报释放；其余协议报。
        MouseEventKind::Release(_) => matches!(
            proto,
            MouseProtocol::Normal | MouseProtocol::Button | MouseProtocol::Any
        ),
        // 移动：Button 仅在拖动（带按键）时报；Any 任何移动都报；其余不报。
        MouseEventKind::Move(held) => match proto {
            MouseProtocol::Button => held.is_some(),
            MouseProtocol::Any => true,
            _ => false,
        },
    }
}

/// 计算协议位低字节 `cb`（不含 SGR 释放需要的「按钮还原」差异）。
///
/// 返回 `(cb, is_release)`：`cb` 已叠加修饰键与 motion 位；`is_release`
/// 供 SGR 选择结尾 `M`/`m`、供默认编码把按钮位替换为释放码 3。
fn compute_cb(kind: MouseEventKind, mods: MouseMods) -> (u8, bool) {
    let (base, is_release) = match kind {
        MouseEventKind::Press(b) => (b.code(), false),
        MouseEventKind::Release(b) => (b.code(), true),
        // motion：低 2 位为按下的键（无则 3），叠加 motion 位 32。
        MouseEventKind::Move(held) => {
            let btn = held.map(MouseButton::code).unwrap_or(3);
            (btn + 32, false)
        }
        MouseEventKind::WheelUp => (64, false),
        MouseEventKind::WheelDown => (65, false),
    };
    (base + mods.bits(), is_release)
}

/// 把一次鼠标事件编码为待写入 PTY 的字节；不应上报时返回 `None`。
///
/// - `Default` 编码：`ESC [ M Cb Cx Cy`，三字节各 +32；坐标 0 基 → 编码为
///   `32 + 1 + coord`，超出单字节（坐标 > 222）时**夹紧到 255**而非丢弃。
///   释放事件按钮位用通用释放码 3。
/// - `Sgr` 编码：`ESC [ < cb ; col+1 ; row+1 M|m`，释放用小写 `m` 且保留
///   真实按钮位（不还原成 3）。
pub fn encode_mouse(
    proto: MouseProtocol,
    enc: MouseEncoding,
    ev: MouseEvent,
) -> Option<Vec<u8>> {
    if !should_report(proto, ev.kind) {
        return None;
    }
    // X10（DEC 9）不编码任何修饰位：xterm 的修饰位仅在 Normal 及以上协议
    // 才发（EditorButton 中修饰叠加被 `mouse_mode != X10` 守卫）。
    let mods = if proto == MouseProtocol::X10 {
        MouseMods::default()
    } else {
        ev.mods
    };
    let (cb, is_release) = compute_cb(ev.kind, mods);
    match enc {
        MouseEncoding::Sgr => {
            let tail = if is_release { 'm' } else { 'M' };
            Some(
                format!("\x1b[<{};{};{}{}", cb, ev.col + 1, ev.row + 1, tail)
                    .into_bytes(),
            )
        }
        MouseEncoding::Default => {
            // 默认编码不区分释放的按钮：释放统一用按钮位 3。
            let cb_x10 = if is_release {
                3 + mods.bits()
            } else {
                cb
            };
            let byte = |v: usize| -> u8 { (v + 1 + 32).min(255) as u8 };
            Some(vec![
                0x1b,
                b'[',
                b'M',
                (cb_x10 as u32 + 32).min(255) as u8,
                byte(ev.col),
                byte(ev.row),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MouseEventKind, col: usize, row: usize) -> MouseEvent {
        MouseEvent {
            kind,
            col,
            row,
            mods: MouseMods::default(),
        }
    }

    #[test]
    fn off_协议不报任何事件() {
        let e = ev(MouseEventKind::Press(MouseButton::Left), 0, 0);
        assert_eq!(encode_mouse(MouseProtocol::Off, MouseEncoding::Sgr, e), None);
    }

    #[test]
    fn sgr_滚轮上下滚() {
        // 滚轮上滚 = 按钮 64，SGR 按下用大写 M；列行 1 基。
        let up = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::WheelUp, 4, 9),
        )
        .unwrap();
        assert_eq!(up, b"\x1b[<64;5;10M");
        let down = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::WheelDown, 0, 0),
        )
        .unwrap();
        assert_eq!(down, b"\x1b[<65;1;1M");
    }

    #[test]
    fn sgr_按下与释放区分大小写尾字母() {
        let press = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Press(MouseButton::Left), 2, 3),
        )
        .unwrap();
        assert_eq!(press, b"\x1b[<0;3;4M");
        let release = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Release(MouseButton::Left), 2, 3),
        )
        .unwrap();
        // 释放保留真实按钮位 0，尾字母小写 m。
        assert_eq!(release, b"\x1b[<0;3;4m");
    }

    #[test]
    fn sgr_右键中键按钮码() {
        let right = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Press(MouseButton::Right), 0, 0),
        )
        .unwrap();
        assert_eq!(right, b"\x1b[<2;1;1M");
        let mid = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Press(MouseButton::Middle), 0, 0),
        )
        .unwrap();
        assert_eq!(mid, b"\x1b[<1;1;1M");
    }

    #[test]
    fn sgr_修饰键叠加() {
        let e = MouseEvent {
            kind: MouseEventKind::Press(MouseButton::Left),
            col: 0,
            row: 0,
            mods: MouseMods {
                shift: true,
                alt: false,
                ctrl: true,
            },
        };
        // 0 + shift(4) + ctrl(16) = 20。
        let out = encode_mouse(MouseProtocol::Normal, MouseEncoding::Sgr, e).unwrap();
        assert_eq!(out, b"\x1b[<20;1;1M");
    }

    #[test]
    fn x10_不编码修饰位() {
        // X10(DEC 9) 下 Ctrl+左键按下：cb 不应叠加 ctrl(16)，仍为 0。
        let e = MouseEvent {
            kind: MouseEventKind::Press(MouseButton::Left),
            col: 0,
            row: 0,
            mods: MouseMods {
                shift: false,
                alt: true,
                ctrl: true,
            },
        };
        let sgr = encode_mouse(MouseProtocol::X10, MouseEncoding::Sgr, e).unwrap();
        assert_eq!(sgr, b"\x1b[<0;1;1M", "X10 不带修饰位");
        // 对照：Normal 下同样输入应叠加 alt(8)+ctrl(16)=24。
        let normal = encode_mouse(MouseProtocol::Normal, MouseEncoding::Sgr, e).unwrap();
        assert_eq!(normal, b"\x1b[<24;1;1M");
    }

    #[test]
    fn x10_协议只报按下() {
        // X10：按下报、释放不报、移动不报。
        assert!(encode_mouse(
            MouseProtocol::X10,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Press(MouseButton::Left), 0, 0)
        )
        .is_some());
        assert!(encode_mouse(
            MouseProtocol::X10,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Release(MouseButton::Left), 0, 0)
        )
        .is_none());
        assert!(encode_mouse(
            MouseProtocol::X10,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Move(Some(MouseButton::Left)), 0, 0)
        )
        .is_none());
    }

    #[test]
    fn normal_不报移动_button_仅报拖动_any_全报() {
        let drag = ev(MouseEventKind::Move(Some(MouseButton::Left)), 1, 1);
        let hover = ev(MouseEventKind::Move(None), 1, 1);
        // Normal：移动一律不报。
        assert!(encode_mouse(MouseProtocol::Normal, MouseEncoding::Sgr, drag).is_none());
        // Button：拖动报、纯移动不报。
        assert!(encode_mouse(MouseProtocol::Button, MouseEncoding::Sgr, drag).is_some());
        assert!(encode_mouse(MouseProtocol::Button, MouseEncoding::Sgr, hover).is_none());
        // Any：都报。
        assert!(encode_mouse(MouseProtocol::Any, MouseEncoding::Sgr, drag).is_some());
        assert!(encode_mouse(MouseProtocol::Any, MouseEncoding::Sgr, hover).is_some());
    }

    #[test]
    fn sgr_拖动带_motion_位() {
        // 拖动左键：低位 0 + motion(32) = 32。
        let out = encode_mouse(
            MouseProtocol::Button,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Move(Some(MouseButton::Left)), 5, 6),
        )
        .unwrap();
        assert_eq!(out, b"\x1b[<32;6;7M");
        // 无键移动（Any）：低位 3 + motion(32) = 35。
        let out2 = encode_mouse(
            MouseProtocol::Any,
            MouseEncoding::Sgr,
            ev(MouseEventKind::Move(None), 0, 0),
        )
        .unwrap();
        assert_eq!(out2, b"\x1b[<35;1;1M");
    }

    #[test]
    fn default_编码字节格式() {
        // 默认编码：ESC [ M, cb+32, col+1+32, row+1+32。
        // 左键按下 col=0,row=0 → cb=0 → [1b 5b 4d 20 21 21]。
        let out = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Default,
            ev(MouseEventKind::Press(MouseButton::Left), 0, 0),
        )
        .unwrap();
        assert_eq!(out, vec![0x1b, b'[', b'M', 32, 33, 33]);
        // 释放：按钮位还原成 3 → cb=3 → 第 4 字节 35。
        let rel = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Default,
            ev(MouseEventKind::Release(MouseButton::Left), 0, 0),
        )
        .unwrap();
        assert_eq!(rel, vec![0x1b, b'[', b'M', 35, 33, 33]);
    }

    #[test]
    fn default_编码坐标超界夹紧不panic() {
        // 坐标极大：编码字节夹紧到 255，不 panic、不 None。
        let out = encode_mouse(
            MouseProtocol::Normal,
            MouseEncoding::Default,
            ev(MouseEventKind::Press(MouseButton::Left), 10_000, 10_000),
        )
        .unwrap();
        assert_eq!(out[0..3], [0x1b, b'[', b'M']);
        assert_eq!(out[4], 255);
        assert_eq!(out[5], 255);
    }
}
