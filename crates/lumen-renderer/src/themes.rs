//! 内置终端主题注册表（P12 主题皮肤库）。
//!
//! 主题以字符串 id 注册（settings.json 持久化该 id），UI 画廊与
//! 设置加载都从 [`BUILTIN`] 取条目。命名约定：「黑白」只指**外壳**
//! （Lumen 的 P9 黑白化外壳色板）；终端 ANSI 16 色永远保留彩色语义，
//! 故主题按**终端配色**命名——默认主题 Lumen Dark = 现状组合
//! （Tokyo Night night 终端色 + 黑白化中性灰选区 + 黑白外壳）。
//!
//! 各主题色值 2026-06 自官方仓库联网抓取校对，来源 URL 标注在每个
//! 构造函数的文档注释里。外壳色板的联动派生见
//! lumen-app/src/shell/theme.rs（Lumen 双主题用 P9 手调值不走派生）。

use crate::theme::{Rgb, Theme};

/// Lumen Dark 的主题 id（默认主题；[`BUILTIN`] 首项 =
/// [`find_or_default`] 的回退项）。
pub const LUMEN_DARK: &str = "lumen-dark";
/// Lumen Light 的主题 id（Sync with OS 浅色槽位的默认值）。
pub const LUMEN_LIGHT: &str = "lumen-light";

/// 一个内置主题的注册条目。
pub struct ThemeInfo {
    /// 稳定 id（settings.json 持久化值；kebab-case，不随展示名变）。
    pub id: &'static str,
    /// 展示名（设置页画廊卡片标签）。
    pub name: &'static str,
    /// 终端底色是否浅色（外壳色板深浅档与 Sync with OS 槽位归类用）。
    pub light: bool,
    /// 主题构造函数（色值为编译期常量，调用零开销级别）。
    make: fn() -> Theme,
}

impl ThemeInfo {
    /// 构造该主题的终端配色。
    pub fn theme(&self) -> Theme {
        (self.make)()
    }
}

/// 全部内置主题（画廊展示顺序）。首项必须是 Lumen Dark
/// （[`find_or_default`] 的回退依赖此约定，见单测）。
pub static BUILTIN: &[ThemeInfo] = &[
    ThemeInfo {
        id: LUMEN_DARK,
        name: "Lumen Dark",
        light: false,
        make: lumen_dark,
    },
    ThemeInfo {
        id: LUMEN_LIGHT,
        name: "Lumen Light",
        light: true,
        make: lumen_light,
    },
    ThemeInfo {
        id: "tokyo-night",
        name: "Tokyo Night",
        light: false,
        make: tokyo_night,
    },
    ThemeInfo {
        id: "tokyo-night-day",
        name: "Tokyo Night Day",
        light: true,
        make: tokyo_night_day,
    },
    ThemeInfo {
        id: "dracula",
        name: "Dracula",
        light: false,
        make: dracula,
    },
    ThemeInfo {
        id: "nord",
        name: "Nord",
        light: false,
        make: nord,
    },
    ThemeInfo {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        light: false,
        make: gruvbox_dark,
    },
    ThemeInfo {
        id: "solarized-dark",
        name: "Solarized Dark",
        light: false,
        make: solarized_dark,
    },
    ThemeInfo {
        id: "solarized-light",
        name: "Solarized Light",
        light: true,
        make: solarized_light,
    },
    ThemeInfo {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        light: false,
        make: catppuccin_mocha,
    },
    ThemeInfo {
        id: "one-dark",
        name: "One Dark",
        light: false,
        make: one_dark,
    },
];

/// 按 id 查注册条目。
pub fn find(id: &str) -> Option<&'static ThemeInfo> {
    BUILTIN.iter().find(|t| t.id == id)
}

/// 按 id 查注册条目，未注册回退默认主题（Lumen Dark，BUILTIN 首项）。
pub fn find_or_default(id: &str) -> &'static ThemeInfo {
    find(id).unwrap_or(&BUILTIN[0])
}

/// Lumen Dark（默认）：Tokyo Night night ANSI 16 色 + P9 黑白化的中性灰
/// 选区 + P16 近黑终端底色（`#0d0d0d`，海风哥「要黑色」诉求；原
/// Tokyo Night bg `#1a1b26` 是蓝紫调）。亮色 ANSI 沿用「亮=常规」简化
/// 映射（纯正官方版见 [`tokyo_night`]）。
fn lumen_dark() -> Theme {
    Theme::default()
}

/// Lumen Light：Tokyo Night day 终端色 + P9 黑白化的中性灰选区。
///
/// ANSI/bg/fg 色值对齐 folke/tokyonight.nvim 官方 day 风格（来源同
/// [`tokyo_night_day`]，2026-06 校对）；selection 为 M3.7b 黑白化
/// 覆盖值（原 day bg_visual `#b7c1e3`）。
fn lumen_light() -> Theme {
    Theme {
        selection: Rgb(0xc6, 0xc6, 0xc6),
        ..tokyo_night_day()
    }
}

/// Tokyo Night（官方 night 风格，含官方蓝灰选区与独立调亮的亮色 ANSI）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - bg/fg/ANSI 16 色:
///   https://github.com/folke/tokyonight.nvim/blob/main/extras/alacritty/tokyonight_night.toml
/// - cursor/selection:
///   https://github.com/folke/tokyonight.nvim/blob/main/extras/windows_terminal/tokyonight_night.json
fn tokyo_night() -> Theme {
    Theme {
        background: Rgb(0x1a, 0x1b, 0x26),
        foreground: Rgb(0xc0, 0xca, 0xf5),
        cursor: Rgb(0xc0, 0xca, 0xf5),
        selection: Rgb(0x28, 0x34, 0x57),
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
            Rgb(0xff, 0x89, 0x9d), // 亮红
            Rgb(0x9f, 0xe0, 0x44), // 亮绿
            Rgb(0xfa, 0xba, 0x4a), // 亮黄
            Rgb(0x8d, 0xb0, 0xff), // 亮蓝
            Rgb(0xc7, 0xa9, 0xff), // 亮品红
            Rgb(0xa4, 0xda, 0xff), // 亮青
            Rgb(0xc0, 0xca, 0xf5), // 亮白
        ],
    }
}

/// Tokyo Night Day（官方 day 风格，浅色）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - bg/fg/ANSI 16 色:
///   https://github.com/folke/tokyonight.nvim/blob/main/extras/alacritty/tokyonight_day.toml
/// - cursor/selection:
///   https://github.com/folke/tokyonight.nvim/blob/main/extras/windows_terminal/tokyonight_day.json
fn tokyo_night_day() -> Theme {
    Theme {
        background: Rgb(0xe1, 0xe2, 0xe7),
        foreground: Rgb(0x37, 0x60, 0xbf),
        cursor: Rgb(0x37, 0x60, 0xbf),
        selection: Rgb(0xb7, 0xc1, 0xe3),
        ansi: [
            Rgb(0xb4, 0xb5, 0xb9), // 黑
            Rgb(0xf5, 0x2a, 0x65), // 红
            Rgb(0x58, 0x75, 0x39), // 绿
            Rgb(0x8c, 0x6c, 0x3e), // 黄
            Rgb(0x2e, 0x7d, 0xe9), // 蓝
            Rgb(0x98, 0x54, 0xf1), // 品红
            Rgb(0x00, 0x71, 0x97), // 青
            Rgb(0x61, 0x72, 0xb0), // 白
            Rgb(0xa1, 0xa6, 0xc5), // 亮黑
            Rgb(0xff, 0x47, 0x74), // 亮红
            Rgb(0x5c, 0x85, 0x24), // 亮绿
            Rgb(0xa2, 0x76, 0x29), // 亮黄
            Rgb(0x35, 0x8a, 0xff), // 亮蓝
            Rgb(0xa4, 0x63, 0xff), // 亮品红
            Rgb(0x00, 0x7e, 0xa8), // 亮青
            Rgb(0x37, 0x60, 0xbf), // 亮白
        ],
    }
}

/// Dracula（官方）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// https://github.com/dracula/alacritty/blob/master/dracula.toml
/// （色板规范 https://spec.draculatheme.com）
fn dracula() -> Theme {
    Theme {
        background: Rgb(0x28, 0x2a, 0x36),
        foreground: Rgb(0xf8, 0xf8, 0xf2),
        cursor: Rgb(0xf8, 0xf8, 0xf2),
        selection: Rgb(0x44, 0x47, 0x5a),
        ansi: [
            Rgb(0x21, 0x22, 0x2c), // 黑
            Rgb(0xff, 0x55, 0x55), // 红
            Rgb(0x50, 0xfa, 0x7b), // 绿
            Rgb(0xf1, 0xfa, 0x8c), // 黄
            Rgb(0xbd, 0x93, 0xf9), // 蓝（Dracula 官方以紫作蓝）
            Rgb(0xff, 0x79, 0xc6), // 品红
            Rgb(0x8b, 0xe9, 0xfd), // 青
            Rgb(0xf8, 0xf8, 0xf2), // 白
            Rgb(0x62, 0x72, 0xa4), // 亮黑
            Rgb(0xff, 0x6e, 0x6e), // 亮红
            Rgb(0x69, 0xff, 0x94), // 亮绿
            Rgb(0xff, 0xff, 0xa5), // 亮黄
            Rgb(0xd6, 0xac, 0xff), // 亮蓝
            Rgb(0xff, 0x92, 0xdf), // 亮品红
            Rgb(0xa4, 0xff, 0xff), // 亮青
            Rgb(0xff, 0xff, 0xff), // 亮白
        ],
    }
}

/// Nord（官方）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// https://github.com/nordtheme/alacritty/blob/main/src/nord.yaml
/// （色板规范 https://www.nordtheme.com/docs/colors-and-palettes）
fn nord() -> Theme {
    Theme {
        background: Rgb(0x2e, 0x34, 0x40), // nord0
        foreground: Rgb(0xd8, 0xde, 0xe9), // nord4
        cursor: Rgb(0xd8, 0xde, 0xe9),
        selection: Rgb(0x4c, 0x56, 0x6a), // nord3
        ansi: [
            Rgb(0x3b, 0x42, 0x52), // 黑 nord1
            Rgb(0xbf, 0x61, 0x6a), // 红 nord11
            Rgb(0xa3, 0xbe, 0x8c), // 绿 nord14
            Rgb(0xeb, 0xcb, 0x8b), // 黄 nord13
            Rgb(0x81, 0xa1, 0xc1), // 蓝 nord9
            Rgb(0xb4, 0x8e, 0xad), // 品红 nord15
            Rgb(0x88, 0xc0, 0xd0), // 青 nord8
            Rgb(0xe5, 0xe9, 0xf0), // 白 nord5
            Rgb(0x4c, 0x56, 0x6a), // 亮黑 nord3
            Rgb(0xbf, 0x61, 0x6a), // 亮红（官方亮色 = 常规）
            Rgb(0xa3, 0xbe, 0x8c), // 亮绿
            Rgb(0xeb, 0xcb, 0x8b), // 亮黄
            Rgb(0x81, 0xa1, 0xc1), // 亮蓝
            Rgb(0xb4, 0x8e, 0xad), // 亮品红
            Rgb(0x8f, 0xbc, 0xbb), // 亮青 nord7
            Rgb(0xec, 0xef, 0xf4), // 亮白 nord6
        ],
    }
}

/// Gruvbox Dark（官方 medium 对比档）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - ANSI/bg/fg:
///   https://github.com/alacritty/alacritty-theme/blob/master/themes/gruvbox_dark.toml
/// - cursor（= fg）与 selection（bg2，官方 Visual 选区底）:
///   https://github.com/morhetz/gruvbox （README 色板表）
fn gruvbox_dark() -> Theme {
    Theme {
        background: Rgb(0x28, 0x28, 0x28), // bg0
        foreground: Rgb(0xeb, 0xdb, 0xb2), // fg1
        cursor: Rgb(0xeb, 0xdb, 0xb2),
        selection: Rgb(0x50, 0x49, 0x45), // bg2
        ansi: [
            Rgb(0x28, 0x28, 0x28), // 黑
            Rgb(0xcc, 0x24, 0x1d), // 红
            Rgb(0x98, 0x97, 0x1a), // 绿
            Rgb(0xd7, 0x99, 0x21), // 黄
            Rgb(0x45, 0x85, 0x88), // 蓝
            Rgb(0xb1, 0x62, 0x86), // 品红
            Rgb(0x68, 0x9d, 0x6a), // 青
            Rgb(0xa8, 0x99, 0x84), // 白
            Rgb(0x92, 0x83, 0x74), // 亮黑
            Rgb(0xfb, 0x49, 0x34), // 亮红
            Rgb(0xb8, 0xbb, 0x26), // 亮绿
            Rgb(0xfa, 0xbd, 0x2f), // 亮黄
            Rgb(0x83, 0xa5, 0x98), // 亮蓝
            Rgb(0xd3, 0x86, 0x9b), // 亮品红
            Rgb(0x8e, 0xc0, 0x7c), // 亮青
            Rgb(0xeb, 0xdb, 0xb2), // 亮白
        ],
    }
}

/// Solarized Dark（官方）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - ANSI/bg/fg:
///   https://github.com/alacritty/alacritty-theme/blob/master/themes/solarized_dark.toml
/// - selection（base02，官方「background highlights」档）与色板定义:
///   https://github.com/altercation/solarized （README "The Values"）
fn solarized_dark() -> Theme {
    Theme {
        background: Rgb(0x00, 0x2b, 0x36), // base03
        foreground: Rgb(0x83, 0x94, 0x96), // base0
        cursor: Rgb(0x83, 0x94, 0x96),
        selection: Rgb(0x07, 0x36, 0x42), // base02
        ansi: [
            Rgb(0x07, 0x36, 0x42), // 黑 base02
            Rgb(0xdc, 0x32, 0x2f), // 红
            Rgb(0x85, 0x99, 0x00), // 绿
            Rgb(0xb5, 0x89, 0x00), // 黄
            Rgb(0x26, 0x8b, 0xd2), // 蓝
            Rgb(0xd3, 0x36, 0x82), // 品红 magenta
            Rgb(0x2a, 0xa1, 0x98), // 青
            Rgb(0xee, 0xe8, 0xd5), // 白 base2
            Rgb(0x00, 0x2b, 0x36), // 亮黑 base03
            Rgb(0xcb, 0x4b, 0x16), // 亮红 orange
            Rgb(0x58, 0x6e, 0x75), // 亮绿 base01
            Rgb(0x65, 0x7b, 0x83), // 亮黄 base00
            Rgb(0x83, 0x94, 0x96), // 亮蓝 base0
            Rgb(0x6c, 0x71, 0xc4), // 亮品红 violet
            Rgb(0x93, 0xa1, 0xa1), // 亮青 base1
            Rgb(0xfd, 0xf6, 0xe3), // 亮白 base3
        ],
    }
}

/// Solarized Light（官方；ANSI 16 色与 Dark 同一套，仅 bg/fg 互换档位）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - ANSI/bg/fg:
///   https://github.com/alacritty/alacritty-theme/blob/master/themes/solarized_light.toml
/// - selection（base2，浅色版「background highlights」档）:
///   https://github.com/altercation/solarized （README "The Values"）
fn solarized_light() -> Theme {
    Theme {
        background: Rgb(0xfd, 0xf6, 0xe3), // base3
        foreground: Rgb(0x58, 0x6e, 0x75), // base01
        cursor: Rgb(0x58, 0x6e, 0x75),
        selection: Rgb(0xee, 0xe8, 0xd5), // base2
        // ANSI 与 Solarized Dark 完全一致（官方设计：强调色共用）。
        ..solarized_dark()
    }
}

/// Catppuccin Mocha（官方）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - ANSI/bg/fg/cursor:
///   https://github.com/catppuccin/alacritty/blob/main/catppuccin-mocha.toml
/// - selection：官方 alacritty 端口用 rosewater 反白选区（同时反转
///   字色），本渲染器选区只换底色不换字色，rosewater 亮底会吞掉浅色
///   文字，按 Catppuccin 风格指南改用 surface2
///   （https://github.com/catppuccin/catppuccin 「Style Guide」）。
fn catppuccin_mocha() -> Theme {
    Theme {
        background: Rgb(0x1e, 0x1e, 0x2e), // base
        foreground: Rgb(0xcd, 0xd6, 0xf4), // text
        cursor: Rgb(0xf5, 0xe0, 0xdc),     // rosewater
        selection: Rgb(0x58, 0x5b, 0x70),  // surface2
        ansi: [
            Rgb(0x45, 0x47, 0x5a), // 黑 surface1
            Rgb(0xf3, 0x8b, 0xa8), // 红
            Rgb(0xa6, 0xe3, 0xa1), // 绿
            Rgb(0xf9, 0xe2, 0xaf), // 黄
            Rgb(0x89, 0xb4, 0xfa), // 蓝
            Rgb(0xf5, 0xc2, 0xe7), // 品红 pink
            Rgb(0x94, 0xe2, 0xd5), // 青 teal
            Rgb(0xba, 0xc2, 0xde), // 白 subtext1
            Rgb(0x58, 0x5b, 0x70), // 亮黑 surface2
            Rgb(0xf3, 0x8b, 0xa8), // 亮红（官方亮色 = 常规）
            Rgb(0xa6, 0xe3, 0xa1), // 亮绿
            Rgb(0xf9, 0xe2, 0xaf), // 亮黄
            Rgb(0x89, 0xb4, 0xfa), // 亮蓝
            Rgb(0xf5, 0xc2, 0xe7), // 亮品红
            Rgb(0x94, 0xe2, 0xd5), // 亮青
            Rgb(0xa6, 0xad, 0xc8), // 亮白 subtext0
        ],
    }
}

/// One Dark（Atom 官方配色的终端适配）。
///
/// 色值来源（2026-06 联网抓取校对）：
/// - ANSI/bg/fg:
///   https://github.com/alacritty/alacritty-theme/blob/master/themes/one_dark.toml
/// - cursor（Atom 编辑器光标蓝 `#528bff`）与 selection（选区灰
///   `#3e4452`）: https://github.com/atom/one-dark-syntax
///   （styles/colors.less）
fn one_dark() -> Theme {
    Theme {
        background: Rgb(0x28, 0x2c, 0x34),
        foreground: Rgb(0xab, 0xb2, 0xbf),
        cursor: Rgb(0x52, 0x8b, 0xff),
        selection: Rgb(0x3e, 0x44, 0x52),
        ansi: [
            Rgb(0x1e, 0x21, 0x27), // 黑
            Rgb(0xe0, 0x6c, 0x75), // 红
            Rgb(0x98, 0xc3, 0x79), // 绿
            Rgb(0xd1, 0x9a, 0x66), // 黄
            Rgb(0x61, 0xaf, 0xef), // 蓝
            Rgb(0xc6, 0x78, 0xdd), // 品红
            Rgb(0x56, 0xb6, 0xc2), // 青
            Rgb(0xab, 0xb2, 0xbf), // 白
            Rgb(0x5c, 0x63, 0x70), // 亮黑
            Rgb(0xe0, 0x6c, 0x75), // 亮红（官方亮色 = 常规）
            Rgb(0x98, 0xc3, 0x79), // 亮绿
            Rgb(0xd1, 0x9a, 0x66), // 亮黄
            Rgb(0x61, 0xaf, 0xef), // 亮蓝
            Rgb(0xc6, 0x78, 0xdd), // 亮品红
            Rgb(0x56, 0xb6, 0xc2), // 亮青
            Rgb(0xff, 0xff, 0xff), // 亮白
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 内置主题_id唯一且非空() {
        let mut seen = std::collections::HashSet::new();
        for info in BUILTIN {
            assert!(!info.id.is_empty() && !info.name.is_empty());
            assert!(seen.insert(info.id), "主题 id 重复: {}", info.id);
        }
    }

    #[test]
    fn 默认主题约定() {
        // find_or_default 的回退依赖「首项 = Lumen Dark」。
        assert_eq!(BUILTIN[0].id, LUMEN_DARK);
        assert!(find(LUMEN_DARK).is_some());
        assert!(find(LUMEN_LIGHT).is_some());
        assert_eq!(find_or_default("不存在的id").id, LUMEN_DARK);
        assert_eq!(find_or_default("nord").id, "nord");
    }

    #[test]
    fn lumen_dark_即现状默认主题() {
        // Lumen Dark 必须与 Theme::default() 完全一致（P9/P12 命名
        // 澄清：默认主题只是改名，观感零变化）。
        let a = find_or_default(LUMEN_DARK).theme();
        let b = Theme::default();
        assert_eq!(a.background, b.background);
        assert_eq!(a.selection, b.selection);
        assert_eq!(a.ansi, b.ansi);
    }

    #[test]
    fn 明暗标志与终端底色一致() {
        // 声明的 light 标志必须与 bg 相对亮度相符（防手填错档）。
        for info in BUILTIN {
            let bg = info.theme().background;
            fn lin(v: u8) -> f32 {
                let s = v as f32 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            }
            let lum = 0.2126 * lin(bg.0) + 0.7152 * lin(bg.1) + 0.0722 * lin(bg.2);
            assert_eq!(
                info.light,
                lum > 0.5,
                "主题 {} 的 light 标志与 bg 亮度不符（lum={lum}）",
                info.id
            );
        }
    }
}
