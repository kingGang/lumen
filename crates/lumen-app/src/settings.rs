//! 设置数据层（M3.4）：serde 结构 + JSON 持久化。
//!
//! 持久化位置：`%LOCALAPPDATA%/Lumen/settings.json`。启动加载——缺失
//! 或损坏时降级默认值并记日志警告，绝不 panic（损坏文件保留原样，
//! 直到下一次设置变更才被覆盖）。写盘走「先写同目录临时文件再改名
//! 覆盖」：Windows 的 `fs::rename` 带 MOVEFILE_REPLACE_EXISTING 语义，
//! 进程在写一半被杀也不会留下半截 JSON。
//!
//! 加载是字段级容错的（M3 审查项）：模块本就邀请用户手改文件，单个
//! 字段值非法（如 theme 拼错）只降级该字段并记日志指明字段名，不
//! 连坐整份配置；UTF-8 BOM 前缀（PowerShell 5.1 写文件的默认行为）
//! 在解析前剥掉。
//!
//! 结构按节扩展：后续 account / keyboard 等加新字段即可，
//! `#[serde(default)]` 保证旧文件平滑升级（缺字段补默认值）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// 终端字号下限（设置页滑块范围；加载时同样夹紧）。
pub const FONT_SIZE_MIN: f32 = 8.0;
/// 终端字号上限。
pub const FONT_SIZE_MAX: f32 = 32.0;
/// 默认终端字号（与 lumen-renderer 的初始值一致）。
pub const FONT_SIZE_DEFAULT: f32 = 15.0;

/// 左侧会话 tab 栏宽度下限（逻辑像素；P10 拖宽范围与加载夹紧共用）。
pub const SIDEBAR_WIDTH_MIN: f32 = 140.0;
/// 左侧会话 tab 栏宽度上限。
pub const SIDEBAR_WIDTH_MAX: f32 = 320.0;
/// 左侧会话 tab 栏默认宽度。
pub const SIDEBAR_WIDTH_DEFAULT: f32 = 180.0;
/// 中间文件树栏宽度下限（逻辑像素；展开态，收起窄条不在此列）。
pub const FILETREE_WIDTH_MIN: f32 = 160.0;
/// 中间文件树栏宽度上限。
pub const FILETREE_WIDTH_MAX: f32 = 480.0;
/// 中间文件树栏默认宽度。
pub const FILETREE_WIDTH_DEFAULT: f32 = 220.0;

/// 主题选择（设置页下拉项；新增主题在此扩展枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// Tokyo Night（默认深色）。
    #[default]
    TokyoNight,
    /// Tokyo Night Light（浅色备选）。
    TokyoNightLight,
}

impl ThemeChoice {
    /// 设置页展示名。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::TokyoNightLight => "Tokyo Night Light",
        }
    }

    /// 是否浅色主题（外壳 egui 色板联动用）。
    pub fn is_light(self) -> bool {
        matches!(self, Self::TokyoNightLight)
    }

    /// 对应的终端配色主题（lumen-renderer 侧）。
    pub fn terminal_theme(self) -> lumen_renderer::Theme {
        match self {
            Self::TokyoNight => lumen_renderer::Theme::tokyo_night(),
            Self::TokyoNightLight => lumen_renderer::Theme::tokyo_night_light(),
        }
    }
}

/// 外观设置（Appearance 节）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// 终端与外壳的配色主题。
    pub theme: ThemeChoice,
    /// 终端字体家族名；空串 = 自动挑选系统等宽字体
    /// （Cascadia Mono → Consolas → 任意 Monospace）。
    pub font_family: String,
    /// 终端字号（逻辑像素，DPI 缩放由渲染器处理）。
    pub font_size: f32,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            font_family: String::new(),
            font_size: FONT_SIZE_DEFAULT,
        }
    }
}

/// 外壳布局设置（Layout 节，P10）：侧栏宽度由拖动调整、松手落盘，
/// 重启还原。旧文件无此节时 `#[serde(default)]` 补默认值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// 左侧会话 tab 栏宽度（逻辑像素）。
    pub sidebar_width: f32,
    /// 中间文件树栏宽度（逻辑像素，展开态；收起窄条宽度固定不存）。
    pub filetree_width: f32,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_WIDTH_DEFAULT,
            filetree_width: FILETREE_WIDTH_DEFAULT,
        }
    }
}

/// 应用设置根结构。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: AppearanceSettings,
    pub layout: LayoutSettings,
}

impl Settings {
    /// 设置文件路径：`%LOCALAPPDATA%/Lumen/settings.json`。
    /// 环境变量缺失（极端定制环境）返回 None，设置仅在内存生效。
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join("Lumen").join("settings.json"))
    }

    /// 启动加载：缺失/损坏降级默认值（记日志），不 panic。
    pub fn load() -> Self {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => {
                log::warn!("LOCALAPPDATA 未设置，使用默认设置（本次运行不持久化）");
                Self::default()
            }
        }
    }

    /// 从指定路径加载（拆出来供单测注入临时路径）。
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                log::info!("设置文件不存在，使用默认设置: {}", path.display());
                return Self::default();
            }
            Err(e) => {
                log::warn!("读设置文件失败，使用默认设置 {}: {e}", path.display());
                return Self::default();
            }
        };
        // PowerShell 5.1 的 Set-Content/重定向默认写 UTF-8 BOM，serde
        // 不认 BOM 会令解析失败，先剥掉（M3 审查追加项）。
        let text = text.trim_start_matches('\u{feff}');
        // 整文件 JSON 语法错误才整体降级；语法合法时逐字段宽松解析，
        // 单字段非法只降级该字段（M3 审查项：theme 拼错不连坐字号字体）。
        let root = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "设置文件解析失败，使用默认设置（原文件保留，下次变更写盘时才覆盖）{}: {e}",
                    path.display()
                );
                return Self::default();
            }
        };
        let mut s = Self::from_value_lenient(&root, path);
        s.sanitize();
        s
    }

    /// 字段级宽松解析：缺字段静默用默认值（旧版本升级路径），字段
    /// 存在但值非法记 warn 指明字段名后单独降级。
    fn from_value_lenient(root: &serde_json::Value, path: &Path) -> Self {
        let mut s = Self::default();
        if !root.is_object() {
            log::warn!(
                "设置文件顶层不是 JSON 对象，使用默认设置: {}",
                path.display()
            );
            return s;
        }
        if let Some(ap) = root.get("appearance") {
            if ap.is_object() {
                let d = AppearanceSettings::default();
                s.appearance.theme = lenient_field(ap, "theme", "appearance.theme", d.theme, path);
                s.appearance.font_family = lenient_field(
                    ap,
                    "font_family",
                    "appearance.font_family",
                    d.font_family,
                    path,
                );
                s.appearance.font_size =
                    lenient_field(ap, "font_size", "appearance.font_size", d.font_size, path);
            } else {
                log::warn!(
                    "设置节 appearance 不是对象，整节降级默认值: {}",
                    path.display()
                );
            }
        }
        if let Some(ly) = root.get("layout") {
            if ly.is_object() {
                let d = LayoutSettings::default();
                s.layout.sidebar_width = lenient_field(
                    ly,
                    "sidebar_width",
                    "layout.sidebar_width",
                    d.sidebar_width,
                    path,
                );
                s.layout.filetree_width = lenient_field(
                    ly,
                    "filetree_width",
                    "layout.filetree_width",
                    d.filetree_width,
                    path,
                );
            } else {
                log::warn!("设置节 layout 不是对象，整节降级默认值: {}", path.display());
            }
        }
        s
    }

    /// 写盘（设置变更即调用）。失败记日志并返回错误描述（调用方据此
    /// 弹 toast 告知用户）——写不进盘不应影响终端使用。无持久化路径
    /// （LOCALAPPDATA 缺失）时静默返回 None（加载时已警告过）。
    pub fn save(&self) -> Option<String> {
        let p = Self::path()?;
        match self.save_to(&p) {
            Ok(()) => None,
            Err(e) => {
                log::error!("写设置文件失败 {}: {e:#}", p.display());
                Some(format!("{e:#}"))
            }
        }
    }

    /// 原子写盘：先写同目录临时文件再改名覆盖，防半写损坏。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("设置路径无父目录")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建设置目录失败: {}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).context("序列化设置失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("写设置临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换设置文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 加载后规整：字号/侧栏宽度夹紧到合法范围、字体名去首尾空白
    /// （越界/NaN/空白来自用户手改文件）。
    fn sanitize(&mut self) {
        /// 有限值夹紧到 [min, max]，非有限（NaN/Inf）回默认值。
        fn clamp_or(v: f32, min: f32, max: f32, default: f32) -> f32 {
            if v.is_finite() {
                v.clamp(min, max)
            } else {
                default
            }
        }
        self.appearance.font_size = clamp_or(
            self.appearance.font_size,
            FONT_SIZE_MIN,
            FONT_SIZE_MAX,
            FONT_SIZE_DEFAULT,
        );
        let trimmed = self.appearance.font_family.trim();
        if trimmed.len() != self.appearance.font_family.len() {
            self.appearance.font_family = trimmed.to_owned();
        }
        self.layout.sidebar_width = clamp_or(
            self.layout.sidebar_width,
            SIDEBAR_WIDTH_MIN,
            SIDEBAR_WIDTH_MAX,
            SIDEBAR_WIDTH_DEFAULT,
        );
        self.layout.filetree_width = clamp_or(
            self.layout.filetree_width,
            FILETREE_WIDTH_MIN,
            FILETREE_WIDTH_MAX,
            FILETREE_WIDTH_DEFAULT,
        );
    }
}

/// 单字段宽松取值：缺失 → 静默用 `fallback`（与 `#[serde(default)]`
/// 行为一致）；存在但反序列化失败 → 记 warn 指明字段路径后用
/// `fallback`（M3 审查项：坏字段单独降级，不连坐整份配置）。
fn lenient_field<T: DeserializeOwned>(
    section: &serde_json::Value,
    key: &str,
    field_path: &str,
    fallback: T,
    file: &Path,
) -> T {
    match section.get(key) {
        None => fallback,
        Some(v) => match T::deserialize(v) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "设置字段 {field_path} 值非法，仅该字段降级为默认值 {}: {e}",
                    file.display()
                );
                fallback
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立文件名，避免并行测试互踩。
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumen_settings_test_{}_{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn 默认值() {
        let s = Settings::default();
        assert_eq!(s.appearance.theme, ThemeChoice::TokyoNight);
        assert!(s.appearance.font_family.is_empty());
        assert_eq!(s.appearance.font_size, FONT_SIZE_DEFAULT);
        assert_eq!(s.layout.sidebar_width, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(s.layout.filetree_width, FILETREE_WIDTH_DEFAULT);
    }

    #[test]
    fn 序列化往返() {
        let s = Settings {
            appearance: AppearanceSettings {
                theme: ThemeChoice::TokyoNightLight,
                font_family: "JetBrains Mono".to_owned(),
                font_size: 18.0,
            },
            layout: LayoutSettings {
                sidebar_width: 260.0,
                filetree_width: 320.0,
            },
        };
        let p = temp_path("roundtrip");
        s.save_to(&p).expect("写盘失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, s);
    }

    #[test]
    fn 损坏文件降级默认() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{ 这不是 json !!!").expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn 缺失文件降级默认() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(Settings::load_from(&p), Settings::default());
    }

    #[test]
    fn 旧文件缺字段平滑升级() {
        let p = temp_path("partial");
        std::fs::write(&p, r#"{ "appearance": { "font_size": 20.0 } }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.font_size, 20.0);
        assert_eq!(loaded.appearance.theme, ThemeChoice::TokyoNight);
        assert!(loaded.appearance.font_family.is_empty());
    }

    #[test]
    fn 字号越界夹紧() {
        let p = temp_path("clamp");
        std::fs::write(&p, r#"{ "appearance": { "font_size": 100.0 } }"#).expect("写测试文件失败");
        assert_eq!(Settings::load_from(&p).appearance.font_size, FONT_SIZE_MAX);
        std::fs::write(&p, r#"{ "appearance": { "font_size": 1.0 } }"#).expect("写测试文件失败");
        assert_eq!(Settings::load_from(&p).appearance.font_size, FONT_SIZE_MIN);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 字段级容错_theme非法不连坐() {
        // theme 拼错（缺连字符）只降级 theme 自己，字体与字号保留。
        let p = temp_path("lenient_theme");
        std::fs::write(
            &p,
            r#"{ "appearance": { "theme": "tokyonight", "font_family": "JetBrains Mono", "font_size": 18.0 } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            loaded.appearance.theme,
            ThemeChoice::TokyoNight,
            "坏字段降级默认"
        );
        assert_eq!(
            loaded.appearance.font_family, "JetBrains Mono",
            "好字段应保留"
        );
        assert_eq!(loaded.appearance.font_size, 18.0, "好字段应保留");
    }

    #[test]
    fn 字段级容错_字号类型非法不连坐() {
        // font_size 写成字符串：仅字号降级默认，theme 保留。
        let p = temp_path("lenient_size");
        std::fs::write(
            &p,
            r#"{ "appearance": { "theme": "tokyo-night-light", "font_size": "big" } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            loaded.appearance.theme,
            ThemeChoice::TokyoNightLight,
            "好字段应保留"
        );
        assert_eq!(
            loaded.appearance.font_size, FONT_SIZE_DEFAULT,
            "坏字段降级默认"
        );
    }

    #[test]
    fn 字段级容错_appearance非对象整节降级() {
        let p = temp_path("lenient_section");
        std::fs::write(&p, r#"{ "appearance": 42 }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn 侧栏宽度_旧文件缺节补默认() {
        // P10 之前的旧 settings.json 没有 layout 节：平滑升级补默认值。
        let p = temp_path("layout_missing");
        std::fs::write(&p, r#"{ "appearance": { "font_size": 20.0 } }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.font_size, 20.0);
        assert_eq!(loaded.layout, LayoutSettings::default());
    }

    #[test]
    fn 侧栏宽度_越界与非法夹紧() {
        let p = temp_path("layout_clamp");
        // 越界夹紧到范围端点。
        std::fs::write(
            &p,
            r#"{ "layout": { "sidebar_width": 9999.0, "filetree_width": 10.0 } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        assert_eq!(loaded.layout.sidebar_width, SIDEBAR_WIDTH_MAX);
        assert_eq!(loaded.layout.filetree_width, FILETREE_WIDTH_MIN);
        // 字段级容错：类型非法只降级该字段，另一字段保留。
        std::fs::write(
            &p,
            r#"{ "layout": { "sidebar_width": "wide", "filetree_width": 300.0 } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.layout.sidebar_width, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(loaded.layout.filetree_width, 300.0);
    }

    #[test]
    fn bom前缀_正常解析() {
        // PowerShell 5.1 写文件默认带 UTF-8 BOM，加载时应剥掉再解析。
        let p = temp_path("bom");
        std::fs::write(
            &p,
            "\u{feff}{ \"appearance\": { \"theme\": \"tokyo-night-light\", \"font_size\": 20.0 } }",
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.theme, ThemeChoice::TokyoNightLight);
        assert_eq!(loaded.appearance.font_size, 20.0);
    }
}
