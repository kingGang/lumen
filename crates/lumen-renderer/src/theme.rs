//! 主题与 256 色调色板解析。

use lumen_term::{Cell, CellFlags, Color};

/// RGB 颜色（线性化前的 sRGB 字节值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// 转 wgpu 清屏色（sRGB→线性近似交给 surface 的 sRGB 格式处理，这里直接归一化）。
    pub fn to_wgpu(self) -> wgpu::Color {
        // surface 用 sRGB 格式时，clear color 需要线性值。
        fn lin(v: u8) -> f64 {
            let s = v as f64 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        wgpu::Color {
            r: lin(self.0),
            g: lin(self.1),
            b: lin(self.2),
            a: 1.0,
        }
    }

    pub fn to_glyphon(self) -> glyphon::Color {
        glyphon::Color::rgb(self.0, self.1, self.2)
    }

    /// 归一化到 [0,1] 的线性 RGBA（矩形管线用，目标是 sRGB surface）。
    pub fn to_linear_f32(self, alpha: f32) -> [f32; 4] {
        fn lin(v: u8) -> f32 {
            let s = v as f32 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        [lin(self.0), lin(self.1), lin(self.2), alpha]
    }
}

/// 终端主题（M3.4 起可在设置页切换，预设见 [`Theme::tokyo_night`] /
/// [`Theme::tokyo_night_light`]）。
#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    /// 选区高亮背景。
    pub selection: Rgb,
    /// ANSI 16 色（0-7 常规，8-15 高亮）。
    pub ansi: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Rgb(0x1a, 0x1b, 0x26),
            foreground: Rgb(0xc0, 0xca, 0xf5),
            cursor: Rgb(0xc0, 0xca, 0xf5),
            selection: Rgb(0x2e, 0x3c, 0x64),
            ansi: [
                Rgb(0x15, 0x16, 0x1e), // 黑
                Rgb(0xf7, 0x76, 0x8e), // 红
                Rgb(0x9e, 0xce, 0x6a), // 绿
                Rgb(0xe0, 0xaf, 0x68), // 黄
                Rgb(0x7a, 0xa2, 0xf7), // 蓝
                Rgb(0xbb, 0x9a, 0xf7), // 品红
                Rgb(0x7d, 0xcf, 0xff), // 青
                Rgb(0xa9, 0xb1, 0xd6), // 白
                Rgb(0x41, 0x48, 0x68), // 亮黑
                Rgb(0xf7, 0x76, 0x8e),
                Rgb(0x9e, 0xce, 0x6a),
                Rgb(0xe0, 0xaf, 0x68),
                Rgb(0x7a, 0xa2, 0xf7),
                Rgb(0xbb, 0x9a, 0xf7),
                Rgb(0x7d, 0xcf, 0xff),
                Rgb(0xc0, 0xca, 0xf5), // 亮白
            ],
        }
    }
}

impl Theme {
    /// Tokyo Night（默认深色主题）。
    pub fn tokyo_night() -> Self {
        Self::default()
    }

    /// Tokyo Night Light（浅色备选；色板取自 tokyonight 的 day 风格
    /// 终端色，黑/亮黑在浅底上取浅灰保证可读）。
    pub fn tokyo_night_light() -> Self {
        Self {
            background: Rgb(0xe1, 0xe2, 0xe7),
            foreground: Rgb(0x37, 0x60, 0xbf),
            cursor: Rgb(0x37, 0x60, 0xbf),
            selection: Rgb(0xb7, 0xc1, 0xe3),
            ansi: [
                Rgb(0xb4, 0xb5, 0xb9), // 黑（浅底上的浅灰）
                Rgb(0xf5, 0x2a, 0x65), // 红
                Rgb(0x58, 0x75, 0x39), // 绿
                Rgb(0x8c, 0x6c, 0x3e), // 黄
                Rgb(0x2e, 0x7d, 0xe9), // 蓝
                Rgb(0x98, 0x54, 0xf1), // 品红
                Rgb(0x00, 0x71, 0x97), // 青
                Rgb(0x61, 0x72, 0xb0), // 白
                Rgb(0xa1, 0xa6, 0xc5), // 亮黑
                Rgb(0xf5, 0x2a, 0x65),
                Rgb(0x58, 0x75, 0x39),
                Rgb(0x8c, 0x6c, 0x3e),
                Rgb(0x2e, 0x7d, 0xe9),
                Rgb(0x98, 0x54, 0xf1),
                Rgb(0x00, 0x71, 0x97),
                Rgb(0x37, 0x60, 0xbf), // 亮白
            ],
        }
    }

    /// 解析单元格颜色到 RGB。`is_fg` 决定 Default 落到前景还是背景。
    pub fn resolve(&self, color: Color, is_fg: bool) -> Rgb {
        match color {
            Color::Default => {
                if is_fg {
                    self.foreground
                } else {
                    self.background
                }
            }
            Color::Indexed(i) => self.indexed(i),
            Color::Rgb(r, g, b) => Rgb(r, g, b),
        }
    }

    /// xterm 256 色表：0-15 主题色，16-231 6×6×6 色立方，232-255 灰阶。
    pub fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                Rgb(step(i / 36), step((i / 6) % 6), step(i % 6))
            }
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                Rgb(v, v, v)
            }
        }
    }

    /// 计算单元格最终的（前景, 背景），处理反显与变暗。
    pub fn cell_colors(&self, cell: &Cell) -> (Rgb, Rgb) {
        let mut fg = self.resolve(cell.fg, true);
        let mut bg = self.resolve(cell.bg, false);
        if cell.flags.contains(CellFlags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(CellFlags::DIM) {
            fg = Rgb(
                (fg.0 as u16 * 2 / 3) as u8,
                (fg.1 as u16 * 2 / 3) as u8,
                (fg.2 as u16 * 2 / 3) as u8,
            );
        }
        (fg, bg)
    }
}
