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
    /// 选区高亮背景（兼作选中命令块的 0.35α 整块淡蒙层，见 lib.rs）。
    ///
    /// M3.7b 黑白化：与外壳同向改为中性灰（深 #404040 / 浅 #c6c6c6），
    /// 原 Tokyo Night 蓝灰是终端画面里最大的蓝色来源。**回退方式**：
    /// 把两处预设还原为 `Rgb(0x2e, 0x3c, 0x64)`（深）/
    /// `Rgb(0xb7, 0xc1, 0xe3)`（浅，day bg_visual）即可，无其他联动。
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
            // M3.7b：中性灰选区（原 Tokyo Night 蓝灰 0x2e,0x3c,0x64，
            // 回退见 selection 字段文档）。
            selection: Rgb(0x40, 0x40, 0x40),
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

    /// Tokyo Night Light（浅色主题）。
    ///
    /// 色值对齐 folke/tokyonight.nvim 官方 **day** 风格的终端色板
    /// （extras/lua/tokyonight_day.lua 的 `terminal` 表，与
    /// extras/alacritty/tokyonight_day.toml 一致，2026-06 校对）：
    /// bg = `bg`，fg/cursor = `fg`，ANSI 0-7 = normal、8-15 =
    /// bright（官方亮色为独立调亮值，非 normal 复用）。selection
    /// 自 M3.7b 起为黑白化覆盖值（非官方 bg_visual），见字段文档。
    pub fn tokyo_night_light() -> Self {
        Self {
            background: Rgb(0xe1, 0xe2, 0xe7),
            foreground: Rgb(0x37, 0x60, 0xbf),
            cursor: Rgb(0x37, 0x60, 0xbf),
            // M3.7b：中性灰选区（原 day bg_visual 0xb7,0xc1,0xe3，
            // 回退见 selection 字段文档）。
            selection: Rgb(0xc6, 0xc6, 0xc6),
            ansi: [
                Rgb(0xb4, 0xb5, 0xb9), // 黑 terminal.black
                Rgb(0xf5, 0x2a, 0x65), // 红 terminal.red
                Rgb(0x58, 0x75, 0x39), // 绿 terminal.green
                Rgb(0x8c, 0x6c, 0x3e), // 黄 terminal.yellow
                Rgb(0x2e, 0x7d, 0xe9), // 蓝 terminal.blue
                Rgb(0x98, 0x54, 0xf1), // 品红 terminal.magenta
                Rgb(0x00, 0x71, 0x97), // 青 terminal.cyan
                Rgb(0x61, 0x72, 0xb0), // 白 terminal.white
                Rgb(0xa1, 0xa6, 0xc5), // 亮黑 terminal.black_bright
                Rgb(0xff, 0x47, 0x74), // 亮红 terminal.red_bright
                Rgb(0x5c, 0x85, 0x24), // 亮绿 terminal.green_bright
                Rgb(0xa2, 0x76, 0x29), // 亮黄 terminal.yellow_bright
                Rgb(0x35, 0x8a, 0xff), // 亮蓝 terminal.blue_bright
                Rgb(0xa4, 0x63, 0xff), // 亮品红 terminal.magenta_bright
                Rgb(0x00, 0x7e, 0xa8), // 亮青 terminal.cyan_bright
                Rgb(0x37, 0x60, 0xbf), // 亮白 terminal.white_bright
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
