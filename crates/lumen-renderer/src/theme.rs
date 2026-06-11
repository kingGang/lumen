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

/// 终端主题（M3.4 起可在设置页切换；P12 起内置主题集见
/// [`crate::themes::BUILTIN`] 注册表，按 id 字符串选择）。
#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    /// 选区高亮背景（兼作选中命令块的 0.35α 整块淡蒙层，见 lib.rs）。
    ///
    /// M3.7b 黑白化：Lumen Dark/Light 双主题与外壳同向用中性灰
    /// （深 #404040 / 浅 #c6c6c6）——原 Tokyo Night 蓝灰是终端画面里
    /// 最大的蓝色来源。P12 起官方版主题（注册表 `tokyo-night` 等）
    /// 保留各自官方选区色，不喜欢中性灰直接切官方主题即可。
    pub selection: Rgb,
    /// ANSI 16 色（0-7 常规，8-15 高亮）。
    pub ansi: [Rgb; 16],
}

impl Default for Theme {
    /// 默认主题 = Lumen Dark（注册表 id `lumen-dark`，P12）：
    /// Tokyo Night night ANSI 16 色 + M3.7b 黑白化中性灰选区 +
    /// P16 近黑终端底色（海风哥：「要黑色」，改 #0d0d0d）。
    ///
    /// 选值理由（P16）：纯黑 #000 显死板、字符边缘抗锯齿发光；
    /// #0d0d0d 接近纯黑但保留一丝质感，与外壳近黑底 #161616 形成
    /// 终端区更深的内凹层次——Terminal 内容在整个画面最黑，符合
    /// 「命令行区黑底」诉求同时保持视觉协调。
    /// 中性灰选区 #404040 在 #0d0d0d 底对比约 3.0:1（选区为装饰底，
    /// 不要求 AA——选区内文字另有前景色保障可读性）。
    fn default() -> Self {
        Self {
            // P16：终端底色改近黑（原 Tokyo Night night bg #1a1b26 是蓝紫调）。
            background: Rgb(0x0d, 0x0d, 0x0d),
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
