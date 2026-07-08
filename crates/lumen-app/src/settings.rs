//! 设置数据层（M3.4）：serde 结构 + JSON 持久化。
//!
//! 持久化位置：应用数据目录下的 `settings.json`（目录按构建类型隔离，
//! 见 [`crate::paths`]——release `Lumen/`、debug `Lumen-dev/`）。启动加载——缺失
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
/// 远程设备列表栏宽度范围与默认（M5.2，会话栏左侧第三列）。
pub const REMOTE_LIST_WIDTH_MIN: f32 = 150.0;
pub const REMOTE_LIST_WIDTH_MAX: f32 = 360.0;
pub const REMOTE_LIST_WIDTH_DEFAULT: f32 = 200.0;

/// 当前 settings.json 模式版本：v2 起（P12）`appearance.theme` 为
/// 主题注册表 id 字符串（旧版为 ThemeChoice 枚举的 kebab-case 序列
/// 化值，加载时按版本迁移，见 [`migrate_theme_id`]）。
pub const SETTINGS_VERSION: u32 = 2;

/// 按 id 取主题注册条目（P12）：未注册回退默认主题 Lumen Dark——
/// 加载侧 [`Settings::load_from`] 的 sanitize 已把非法 id 降级，
/// 这里的回退只是运行期改值的防御。
pub fn theme_info(id: &str) -> &'static lumen_renderer::themes::ThemeInfo {
    lumen_renderer::themes::find_or_default(id)
}

/// 终端背景图片设置（P13）。
///
/// 背景图仅作用于终端工作区整体（跨窗格共一张图）；侧栏/顶栏/
/// 文件树/设置页不透图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundSettings {
    /// 是否启用背景图片功能。
    pub enabled: bool,
    /// 图片文件绝对路径；`None` 表示未选图。
    pub path: Option<String>,
    /// 背景图不透明度：0.05（几乎透明）～1.0（完全不透明）。
    /// 默认 0.4 保留足够可读性。
    pub opacity: f32,
    /// 额外暗化蒙层强度：0.0（不暗化）～0.9（深暗）。
    /// 与 opacity 叠用；深色终端内容建议调低此值。
    pub dim: f32,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            opacity: 0.4,
            dim: 0.0,
        }
    }
}

/// 背景图不透明度下限（设置页滑块范围；加载时同样夹紧）。
pub const BACKGROUND_OPACITY_MIN: f32 = 0.05;
/// 背景图不透明度上限。
pub const BACKGROUND_OPACITY_MAX: f32 = 1.0;
/// 背景图暗化强度下限。
pub const BACKGROUND_DIM_MIN: f32 = 0.0;
/// 背景图暗化强度上限。
pub const BACKGROUND_DIM_MAX: f32 = 0.9;
/// 图片边长最大值（超出则拒绝加载，防显存溢出）。
pub const BACKGROUND_MAX_DIM: u32 = 8192;

/// 外观设置（Appearance 节）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// 终端与外壳的配色主题 id（注册表见 lumen_renderer::themes；
    /// Sync with OS 开启时不读本字段，走深浅槽位二选）。
    pub theme: String,
    /// 跟随系统深浅模式自动切换主题（P12 Sync with OS）。
    pub sync_with_os: bool,
    /// 系统深色模式时使用的主题 id（Sync with OS 深色槽位）。
    pub dark_theme_id: String,
    /// 系统浅色模式时使用的主题 id（Sync with OS 浅色槽位）。
    pub light_theme_id: String,
    /// 终端字体家族名；空串 = 自动挑选系统等宽字体
    /// （Cascadia Mono → Consolas → 任意 Monospace）。
    pub font_family: String,
    /// 终端字号（逻辑像素，DPI 缩放由渲染器处理）。
    pub font_size: f32,
    /// 终端背景图片（P13）。
    pub background: BackgroundSettings,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: lumen_renderer::themes::LUMEN_DARK.to_owned(),
            sync_with_os: false,
            dark_theme_id: lumen_renderer::themes::LUMEN_DARK.to_owned(),
            light_theme_id: lumen_renderer::themes::LUMEN_LIGHT.to_owned(),
            font_family: String::new(),
            font_size: FONT_SIZE_DEFAULT,
            background: BackgroundSettings::default(),
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
    /// 左侧会话栏是否可见（问题7）：true = 显示（默认），false = 隐藏。
    /// `#[serde(default)]` 保证旧文件缺字段时补 true（与 Default 一致）。
    #[serde(default = "default_sidebar_visible")]
    pub sidebar_visible: bool,
    /// 中间文件树栏是否可见（第十九轮持久化）：true = 显示（默认），false = 隐藏。
    ///
    /// 变更来源：顶栏②按钮（`toggle_filetree` 信号）与 Ctrl+B 快捷键
    /// 共享同一 `ShellState::filetree.visible` 状态源；两入口切换时均写盘，
    /// 重启后从本字段恢复 `ShellState::filetree.visible` 初值。
    ///
    /// `#[serde(default)]` 保证旧文件（第十九轮前）缺字段时补 `true`
    /// （与 [`Default`] 一致），平滑升级无感知。
    #[serde(default = "default_filetree_visible")]
    pub filetree_visible: bool,
    /// 当前视图（M5.2）：false = 本地（默认），true = 远程（设备栏）。
    /// 顶栏「本地/远程」tab 切换；`#[serde(default)]` 旧文件缺字段补 false。
    #[serde(default = "default_view_mode")]
    pub view_mode: bool,
}

/// sidebar_visible 字段的 serde default 函数（旧文件缺字段时补 true）。
fn default_sidebar_visible() -> bool {
    true
}

/// filetree_visible 字段的 serde default 函数（旧文件缺字段时补 true）。
fn default_filetree_visible() -> bool {
    true
}

/// view_mode 字段的 serde default 函数（旧文件缺字段时补 false = 本地）。
fn default_view_mode() -> bool {
    false
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_WIDTH_DEFAULT,
            filetree_width: FILETREE_WIDTH_DEFAULT,
            sidebar_visible: true,
            filetree_visible: true,
            view_mode: false,
        }
    }
}

/// 热更（F3）设置。旧文件无此节时 `#[serde(default)]` 补默认值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// 启动时自动检查更新（默认 true）。
    pub auto_check: bool,
    /// 用户「跳过」的版本 tag：该 tag 不再提示；出现更新的版本仍会提示。
    pub skip_version: Option<String>,
    /// 上次检查的 Unix 毫秒时间戳（节流：间隔不足时跳过后台自动检查）。
    pub last_check_ms: Option<u64>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            skip_version: None,
            last_check_ms: None,
        }
    }
}

/// 网络代理设置（用于检查 / 下载更新等出网请求）。
///
/// 单字段 URL：支持 `http://host:port`、`https://host:port`、
/// `socks5://host:port`（最灵活，覆盖 clash/v2ray 等常见代理）。
/// 空白 URL 视为未配置——即使 `enabled` 也不生效。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    /// 是否启用代理。
    pub enabled: bool,
    /// 代理 URL（含协议前缀）。
    pub url: String,
}

impl ProxySettings {
    /// 生效的代理 URL：启用且去空白后非空时返回，否则 `None`
    /// （供 update 等网络请求按需挂 `ureq::Proxy`）。
    pub fn effective_url(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        let u = self.url.trim();
        (!u.is_empty()).then_some(u)
    }
}

/// 应用设置根结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 模式版本（P12 引入；旧文件缺省视作 0，触发主题 id 迁移，
    /// 重新写盘即升级到 [`SETTINGS_VERSION`]）。
    pub version: u32,
    pub appearance: AppearanceSettings,
    pub layout: LayoutSettings,
    /// 界面语言（F6）：`#[serde(default)]` 旧文件无此字段时补默认值
    /// [`crate::i18n::Language::ZhCn`]（简体中文）。
    pub language: crate::i18n::Language,
    /// 经典直通模式开关（第十八轮持久化）：对应运行时 `AppState::force_fallback`。
    ///
    /// `true` 时启动即进入经典直通态（`force_fallback = true`），
    /// 所有按键直通 PTY，不走 shell integration / Compose 态编辑器。
    /// 默认 `false`（正常 AI-native 模式）。
    ///
    /// 变更来源：`TermAction::ToggleFallback`（Ctrl+Shift+E 快捷键
    /// 与状态栏切换按钮同路径）——切换时同步写盘，重启后恢复。
    #[serde(default)]
    pub classic_mode: bool,
    /// 远程服务端地址（M5.2；空 = 用环境变量/默认）。两机互联时填运行
    /// server 那台的 IP:端口（自动补 `http://`）。
    #[serde(default)]
    pub server_url: String,
    /// 热更设置（F3）：`#[serde(default)]` 旧文件无此节时补默认值。
    #[serde(default)]
    pub update: UpdateSettings,
    /// 网络代理：`#[serde(default)]` 旧文件无此节时补默认值（关闭）。
    #[serde(default)]
    pub proxy: ProxySettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings::default(),
            layout: LayoutSettings::default(),
            language: crate::i18n::Language::default(),
            classic_mode: false,
            server_url: String::new(),
            update: UpdateSettings::default(),
            proxy: ProxySettings::default(),
        }
    }
}

impl Settings {
    /// 设置文件路径：应用数据目录下的 `settings.json`（目录按构建类型
    /// 隔离，见 [`crate::paths`]——release `Lumen/`、debug `Lumen-dev/`）。
    /// 数据目录不可用（极端定制环境）返回 None，设置仅在内存生效。
    pub fn path() -> Option<PathBuf> {
        crate::paths::data_file("settings.json")
    }

    /// 启动加载：缺失/损坏降级默认值（记日志），不 panic。
    pub fn load() -> Self {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => {
                log::warn!("数据目录不可用（HOME/LOCALAPPDATA 均未解析到？），使用默认设置（本次运行不持久化）");
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
        // 模式版本：旧文件（P12 前）无此字段视作 0，主题字段按旧值
        // 迁移；加载结果恒为当前版本（下次写盘即落 v2 格式）。
        let file_version = root
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        s.version = SETTINGS_VERSION;
        if let Some(ap) = root.get("appearance") {
            if ap.is_object() {
                let d = AppearanceSettings::default();
                let raw_theme: String =
                    lenient_field(ap, "theme", "appearance.theme", d.theme, path);
                s.appearance.theme = migrate_theme_id(file_version, raw_theme, path);
                s.appearance.sync_with_os = lenient_field(
                    ap,
                    "sync_with_os",
                    "appearance.sync_with_os",
                    d.sync_with_os,
                    path,
                );
                s.appearance.dark_theme_id = lenient_field(
                    ap,
                    "dark_theme_id",
                    "appearance.dark_theme_id",
                    d.dark_theme_id,
                    path,
                );
                s.appearance.light_theme_id = lenient_field(
                    ap,
                    "light_theme_id",
                    "appearance.light_theme_id",
                    d.light_theme_id,
                    path,
                );
                s.appearance.font_family = lenient_field(
                    ap,
                    "font_family",
                    "appearance.font_family",
                    d.font_family,
                    path,
                );
                s.appearance.font_size =
                    lenient_field(ap, "font_size", "appearance.font_size", d.font_size, path);
                // 背景图子结构：整节宽松解析。
                if let Some(bg) = ap.get("background") {
                    if bg.is_object() {
                        let dg = BackgroundSettings::default();
                        s.appearance.background.enabled = lenient_field(
                            bg,
                            "enabled",
                            "appearance.background.enabled",
                            dg.enabled,
                            path,
                        );
                        s.appearance.background.path =
                            lenient_field(bg, "path", "appearance.background.path", dg.path, path);
                        s.appearance.background.opacity = lenient_field(
                            bg,
                            "opacity",
                            "appearance.background.opacity",
                            dg.opacity,
                            path,
                        );
                        s.appearance.background.dim =
                            lenient_field(bg, "dim", "appearance.background.dim", dg.dim, path);
                    } else {
                        log::warn!(
                            "设置节 appearance.background 不是对象，整节降级默认值: {}",
                            path.display()
                        );
                    }
                }
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
                // sidebar_visible（问题7）：旧文件缺字段静默补 true。
                s.layout.sidebar_visible = lenient_field(
                    ly,
                    "sidebar_visible",
                    "layout.sidebar_visible",
                    d.sidebar_visible,
                    path,
                );
                // filetree_visible（第十九轮）：旧文件缺字段静默补 true。
                s.layout.filetree_visible = lenient_field(
                    ly,
                    "filetree_visible",
                    "layout.filetree_visible",
                    d.filetree_visible,
                    path,
                );
                // view_mode（M5.2）：旧文件缺字段静默补 false（本地）。
                s.layout.view_mode = lenient_field(
                    ly,
                    "view_mode",
                    "layout.view_mode",
                    d.view_mode,
                    path,
                );
            } else {
                log::warn!("设置节 layout 不是对象，整节降级默认值: {}", path.display());
            }
        }
        // language：旧文件缺字段时静默补默认值（ZhCn）；值非法记 warn
        // 后降级——与其余字段同款字段级容错。
        s.language = lenient_field(
            root,
            "language",
            "language",
            crate::i18n::Language::default(),
            path,
        );
        // classic_mode：旧文件（第十八轮前）无此字段时静默补 false（默认正常模式）。
        s.classic_mode = lenient_field(root, "classic_mode", "classic_mode", false, path);
        // server_url（M5.2）：旧文件缺字段补空串（回退环境变量/默认）。
        s.server_url = lenient_field(root, "server_url", "server_url", String::new(), path);
        // update（F3）：旧文件缺整节时静默补默认值；逐字段宽松解析。
        if let Some(up) = root.get("update") {
            if up.is_object() {
                let d = UpdateSettings::default();
                s.update.auto_check =
                    lenient_field(up, "auto_check", "update.auto_check", d.auto_check, path);
                s.update.skip_version = lenient_field(
                    up,
                    "skip_version",
                    "update.skip_version",
                    d.skip_version,
                    path,
                );
                s.update.last_check_ms = lenient_field(
                    up,
                    "last_check_ms",
                    "update.last_check_ms",
                    d.last_check_ms,
                    path,
                );
            } else {
                log::warn!("设置节 update 不是对象，整节降级默认值: {}", path.display());
            }
        }
        // proxy（网络代理）：旧文件缺整节时静默补默认值；逐字段宽松解析。
        if let Some(px) = root.get("proxy") {
            if px.is_object() {
                let d = ProxySettings::default();
                s.proxy.enabled =
                    lenient_field(px, "enabled", "proxy.enabled", d.enabled, path);
                s.proxy.url = lenient_field(px, "url", "proxy.url", d.url, path);
            } else {
                log::warn!("设置节 proxy 不是对象，整节降级默认值: {}", path.display());
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
            Ok(()) => {
                // 写盘日志带 PID（F8 纵深防御）：多开放行（测试场景）
                // 时配置仍可能互踩，便于排查「后写者赢」来自哪个进程。
                log::debug!("设置写盘 pid={}: {}", std::process::id(), p.display());
                None
            }
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

    /// 当前生效的主题 id（P12）：Sync with OS 开启时按系统深浅模式
    /// 取对应槽位，否则取手选主题。
    pub fn effective_theme_id(&self, os_dark: bool) -> &str {
        let ap = &self.appearance;
        if ap.sync_with_os {
            if os_dark {
                &ap.dark_theme_id
            } else {
                &ap.light_theme_id
            }
        } else {
            &ap.theme
        }
    }

    /// 加载后规整：字号/侧栏宽度夹紧到合法范围、字体名去首尾空白、
    /// 主题 id 未注册降级默认（越界/NaN/空白/坏 id 来自用户手改文件）。
    fn sanitize(&mut self) {
        /// 有限值夹紧到 [min, max]，非有限（NaN/Inf）回默认值。
        fn clamp_or(v: f32, min: f32, max: f32, default: f32) -> f32 {
            if v.is_finite() {
                v.clamp(min, max)
            } else {
                default
            }
        }
        /// 主题 id 校验：未注册记 warn 后降级 `fallback`（字段级容错
        /// 的延伸——id 是开放字符串，注册表查不到才算非法）。
        fn valid_theme_or(id: &mut String, fallback: &str, field: &str) {
            if lumen_renderer::themes::find(id).is_none() {
                log::warn!("设置字段 {field} 主题 id「{id}」未注册，降级「{fallback}」");
                *id = fallback.to_owned();
            }
        }
        valid_theme_or(
            &mut self.appearance.theme,
            lumen_renderer::themes::LUMEN_DARK,
            "appearance.theme",
        );
        valid_theme_or(
            &mut self.appearance.dark_theme_id,
            lumen_renderer::themes::LUMEN_DARK,
            "appearance.dark_theme_id",
        );
        valid_theme_or(
            &mut self.appearance.light_theme_id,
            lumen_renderer::themes::LUMEN_LIGHT,
            "appearance.light_theme_id",
        );
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
        // 背景图参数夹紧：NaN/Inf 归默认，有限值夹到合法范围。
        let bg = &mut self.appearance.background;
        bg.opacity = clamp_or(
            bg.opacity,
            BACKGROUND_OPACITY_MIN,
            BACKGROUND_OPACITY_MAX,
            BackgroundSettings::default().opacity,
        );
        bg.dim = clamp_or(
            bg.dim,
            BACKGROUND_DIM_MIN,
            BACKGROUND_DIM_MAX,
            BackgroundSettings::default().dim,
        );
        // 代理 URL 去首尾空白（用户手改文件 / 粘贴带空格）。
        let trimmed = self.proxy.url.trim();
        if trimmed.len() != self.proxy.url.len() {
            self.proxy.url = trimmed.to_owned();
        }
    }
}

/// 旧版主题值迁移（P12）：v2 之前 `appearance.theme` 是 ThemeChoice
/// 枚举的序列化值——旧 "tokyo-night" 实为 M3.7b 改色版（中性灰选区），
/// 即现在的 Lumen Dark，故映射到 `lumen-dark`（保住用户原观感；纯正
/// 官方版 Tokyo Night 是 P12 新增的另一条目）。v2 起的文件 id 原样
/// 保留（"tokyo-night" 此时指官方版）。未知旧值原样返回，交由
/// sanitize 的注册表校验降级。
fn migrate_theme_id(file_version: u64, id: String, file: &Path) -> String {
    if file_version >= u64::from(SETTINGS_VERSION) {
        return id;
    }
    // "TokyoNight"/"TokyoNightLight" 为防御映射：枚举序列化历史上
    // 恒为 kebab-case，但手改文件可能照抄枚举名。
    let mapped = match id.as_str() {
        "tokyo-night" | "TokyoNight" => lumen_renderer::themes::LUMEN_DARK,
        "tokyo-night-light" | "TokyoNightLight" => lumen_renderer::themes::LUMEN_LIGHT,
        _ => return id,
    };
    log::info!(
        "设置迁移：旧主题值「{id}」→ 主题 id「{mapped}」: {}",
        file.display()
    );
    mapped.to_owned()
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
        assert_eq!(s.version, SETTINGS_VERSION);
        assert_eq!(s.appearance.theme, "lumen-dark");
        assert!(!s.appearance.sync_with_os);
        assert_eq!(s.appearance.dark_theme_id, "lumen-dark");
        assert_eq!(s.appearance.light_theme_id, "lumen-light");
        assert!(s.appearance.font_family.is_empty());
        assert_eq!(s.appearance.font_size, FONT_SIZE_DEFAULT);
        assert_eq!(s.layout.sidebar_width, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(s.layout.filetree_width, FILETREE_WIDTH_DEFAULT);
        // classic_mode 默认 false（正常 AI-native 模式，非经典直通）。
        assert!(!s.classic_mode, "classic_mode 默认值应为 false");
    }

    #[test]
    fn 序列化往返() {
        let s = Settings {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings {
                theme: "nord".to_owned(),
                sync_with_os: true,
                dark_theme_id: "gruvbox-dark".to_owned(),
                light_theme_id: "solarized-light".to_owned(),
                font_family: "JetBrains Mono".to_owned(),
                font_size: 18.0,
                background: BackgroundSettings::default(),
            },
            layout: LayoutSettings {
                sidebar_width: 260.0,
                filetree_width: 320.0,
                sidebar_visible: true,
                filetree_visible: false,
                view_mode: true,
            },
            language: crate::i18n::Language::ZhTw,
            classic_mode: false,
            server_url: String::new(),
            update: UpdateSettings {
                auto_check: false,
                skip_version: Some("v9.9.9".to_owned()),
                last_check_ms: Some(123_456),
            },
            proxy: ProxySettings {
                enabled: true,
                url: "http://127.0.0.1:7890".to_owned(),
            },
        };
        let p = temp_path("roundtrip");
        s.save_to(&p).expect("写盘失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, s);
    }

    #[test]
    fn 代理_默认关闭且effective为none() {
        let s = Settings::default();
        assert!(!s.proxy.enabled);
        assert!(s.proxy.url.is_empty());
        assert_eq!(s.proxy.effective_url(), None, "默认无代理");
    }

    #[test]
    fn 代理_effective_url规则() {
        // 启用 + 非空 → Some（去空白）；关闭或空白 → None。
        let mut p = ProxySettings {
            enabled: true,
            url: "  socks5://127.0.0.1:1080  ".to_owned(),
        };
        assert_eq!(p.effective_url(), Some("socks5://127.0.0.1:1080"));
        p.enabled = false;
        assert_eq!(p.effective_url(), None, "关闭即不生效");
        p.enabled = true;
        p.url = "   ".to_owned();
        assert_eq!(p.effective_url(), None, "纯空白视为未配置");
    }

    #[test]
    fn 代理_旧文件缺节补默认() {
        // 旧 settings.json 无 proxy 节：加载后补默认值（关闭），其余字段不受影响。
        let p = temp_path("proxy_missing");
        std::fs::write(&p, r#"{ "appearance": { "font_size": 18.0 } }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.proxy, ProxySettings::default(), "缺 proxy 节补默认值");
        assert_eq!(loaded.appearance.font_size, 18.0, "其余字段不受影响");
    }

    #[test]
    fn 代理_url去空白() {
        let p = temp_path("proxy_trim");
        std::fs::write(
            &p,
            r#"{ "proxy": { "enabled": true, "url": "  http://h:1  " } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.proxy.url, "http://h:1", "加载 sanitize 去首尾空白");
        assert!(loaded.proxy.enabled);
    }

    #[test]
    fn classic_mode_serde默认false() {
        // #[serde(default)] 保证旧文件无此字段时加载补 false（不破坏已有用户）。
        let p = temp_path("classic_mode_default");
        // 旧格式 settings.json：无 classic_mode 字段。
        std::fs::write(&p, r#"{ "appearance": { "font_size": 16.0 } }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            !loaded.classic_mode,
            "旧文件缺 classic_mode 时应降级为 false（正常模式）"
        );
        assert_eq!(loaded.appearance.font_size, 16.0, "其余字段不受影响");
    }

    #[test]
    fn classic_mode_序列化往返() {
        // classic_mode=true 序列化再加载后应保持 true。
        let p = temp_path("classic_mode_roundtrip");
        let s = Settings {
            classic_mode: true,
            ..Settings::default()
        };
        s.save_to(&p).expect("写盘失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(loaded.classic_mode, "classic_mode=true 写盘后加载应为 true");
    }

    #[test]
    fn classic_mode_旧文件兼容() {
        // 显式写 classic_mode=false 的"新"文件，加载后也应为 false。
        let p = temp_path("classic_mode_explicit_false");
        std::fs::write(&p, r#"{ "classic_mode": false }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            !loaded.classic_mode,
            "classic_mode=false 显式写盘后加载应为 false"
        );
    }

    #[test]
    fn 迁移_旧主题值映射到lumen双主题() {
        // P12 前的旧文件（无 version 字段）：旧 "tokyo-night" 实为
        // M3.7b 改色版 = 现 Lumen Dark；浅色同理。
        let p = temp_path("migrate_old");
        std::fs::write(&p, r#"{ "appearance": { "theme": "tokyo-night" } }"#)
            .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        assert_eq!(loaded.appearance.theme, "lumen-dark");
        assert_eq!(loaded.version, SETTINGS_VERSION, "加载即升级版本");
        std::fs::write(&p, r#"{ "appearance": { "theme": "tokyo-night-light" } }"#)
            .expect("写测试文件失败");
        assert_eq!(Settings::load_from(&p).appearance.theme, "lumen-light");
        // 防御：手改文件照抄枚举名的大写变体同样迁移。
        std::fs::write(&p, r#"{ "appearance": { "theme": "TokyoNight" } }"#)
            .expect("写测试文件失败");
        assert_eq!(Settings::load_from(&p).appearance.theme, "lumen-dark");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 迁移_v2文件的官方tokyo_night不迁移() {
        // v2 起 "tokyo-night" 指 P12 新增的纯正官方版，原样保留。
        let p = temp_path("migrate_v2");
        std::fs::write(
            &p,
            r#"{ "version": 2, "appearance": { "theme": "tokyo-night" } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.theme, "tokyo-night");
    }

    #[test]
    fn 主题id_未注册降级默认() {
        let p = temp_path("bad_theme_id");
        std::fs::write(
            &p,
            r#"{ "version": 2, "appearance": { "theme": "没有这个主题", "dark_theme_id": "x", "light_theme_id": "y", "font_size": 18.0 } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.theme, "lumen-dark");
        assert_eq!(loaded.appearance.dark_theme_id, "lumen-dark");
        assert_eq!(loaded.appearance.light_theme_id, "lumen-light");
        assert_eq!(loaded.appearance.font_size, 18.0, "好字段应保留");
    }

    #[test]
    fn 生效主题id_随sync与系统深浅() {
        let mut s = Settings::default();
        s.appearance.theme = "dracula".to_owned();
        assert_eq!(s.effective_theme_id(true), "dracula", "未开 sync 走手选");
        assert_eq!(s.effective_theme_id(false), "dracula");
        s.appearance.sync_with_os = true;
        assert_eq!(s.effective_theme_id(true), "lumen-dark", "深色槽位");
        assert_eq!(s.effective_theme_id(false), "lumen-light", "浅色槽位");
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
        assert_eq!(loaded.appearance.theme, "lumen-dark");
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
        // theme 不是字符串（类型非法）只降级 theme 自己，字体与字号保留。
        let p = temp_path("lenient_theme");
        std::fs::write(
            &p,
            r#"{ "appearance": { "theme": 42, "font_family": "JetBrains Mono", "font_size": 18.0 } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.theme, "lumen-dark", "坏字段降级默认");
        assert_eq!(
            loaded.appearance.font_family, "JetBrains Mono",
            "好字段应保留"
        );
        assert_eq!(loaded.appearance.font_size, 18.0, "好字段应保留");
    }

    #[test]
    fn 字段级容错_字号类型非法不连坐() {
        // font_size 写成字符串：仅字号降级默认，theme 保留（旧文件
        // 无 version，旧浅色值迁移到 lumen-light）。
        let p = temp_path("lenient_size");
        std::fs::write(
            &p,
            r#"{ "appearance": { "theme": "tokyo-night-light", "font_size": "big" } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.theme, "lumen-light", "好字段应保留");
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
    fn 背景图_默认值() {
        let s = Settings::default();
        assert!(!s.appearance.background.enabled);
        assert!(s.appearance.background.path.is_none());
        assert_eq!(s.appearance.background.opacity, 0.4);
        assert_eq!(s.appearance.background.dim, 0.0);
    }

    #[test]
    fn 背景图_旧文件缺字段平滑升级() {
        // 旧文件无 appearance.background 节：加载后补默认值。
        let p = temp_path("bg_missing");
        std::fs::write(&p, r#"{ "appearance": { "font_size": 18.0 } }"#).expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.font_size, 18.0, "好字段保留");
        assert_eq!(
            loaded.appearance.background,
            BackgroundSettings::default(),
            "缺背景图节时补默认值"
        );
    }

    #[test]
    fn 背景图_opacity_dim_夹紧() {
        let p = temp_path("bg_clamp");
        // opacity 越界（>1.0）夹到上限，dim 越界（>0.9）夹到上限。
        std::fs::write(
            &p,
            r#"{ "appearance": { "background": { "enabled": true, "opacity": 9.9, "dim": 5.0 } } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        assert_eq!(loaded.appearance.background.opacity, BACKGROUND_OPACITY_MAX);
        assert_eq!(loaded.appearance.background.dim, BACKGROUND_DIM_MAX);
        // opacity 过小（<0.05）夹到下限。
        std::fs::write(
            &p,
            r#"{ "appearance": { "background": { "opacity": 0.0, "dim": -1.0 } } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.background.opacity, BACKGROUND_OPACITY_MIN);
        assert_eq!(loaded.appearance.background.dim, BACKGROUND_DIM_MIN);
    }

    #[test]
    fn 背景图_opacity_dim_边界值不被过度夹紧() {
        // 恰好等于下限/上限的合法值，加载后应原值保留，不被继续夹紧。
        // 替换原重叠测试（opacity=0.0 + dim=-0.5 已由 背景图_opacity_dim_夹紧 覆盖）。
        let p = temp_path("bg_boundary");
        std::fs::write(
            &p,
            format!(
                r#"{{ "appearance": {{ "background": {{ "opacity": {opacity_min}, "dim": {dim_min} }} }} }}"#,
                opacity_min = BACKGROUND_OPACITY_MIN,
                dim_min = BACKGROUND_DIM_MIN,
            ),
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            loaded.appearance.background.opacity, BACKGROUND_OPACITY_MIN,
            "下限值不被进一步夹紧"
        );
        assert_eq!(
            loaded.appearance.background.dim, BACKGROUND_DIM_MIN,
            "dim 下限值不被进一步夹紧"
        );
        // 上限同样守恒。
        let p2 = temp_path("bg_boundary_max");
        std::fs::write(
            &p2,
            format!(
                r#"{{ "appearance": {{ "background": {{ "opacity": {opacity_max}, "dim": {dim_max} }} }} }}"#,
                opacity_max = BACKGROUND_OPACITY_MAX,
                dim_max = BACKGROUND_DIM_MAX,
            ),
        )
        .expect("写测试文件失败");
        let loaded2 = Settings::load_from(&p2);
        let _ = std::fs::remove_file(&p2);
        assert_eq!(
            loaded2.appearance.background.opacity, BACKGROUND_OPACITY_MAX,
            "上限值不被进一步夹紧"
        );
        assert_eq!(
            loaded2.appearance.background.dim, BACKGROUND_DIM_MAX,
            "dim 上限值不被进一步夹紧"
        );
    }

    #[test]
    fn 背景图_序列化往返() {
        let p = temp_path("bg_roundtrip");
        let mut s = Settings::default();
        s.appearance.background = BackgroundSettings {
            enabled: true,
            path: Some("/home/test/wallpaper.png".to_owned()),
            opacity: 0.6,
            dim: 0.2,
        };
        s.save_to(&p).expect("写盘失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.appearance.background, s.appearance.background);
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
        assert_eq!(loaded.appearance.theme, "lumen-light", "旧值迁移");
        assert_eq!(loaded.appearance.font_size, 20.0);
    }

    // ── 第十九轮：文件树可见性持久化测试 ────────────────────────────────────

    #[test]
    fn 文件树可见性_默认值为true() {
        // LayoutSettings::default() 和 Settings::default() 均应 filetree_visible=true。
        let s = Settings::default();
        assert!(
            s.layout.filetree_visible,
            "filetree_visible 默认值应为 true（展开态）"
        );
        assert!(
            s.layout.sidebar_visible,
            "sidebar_visible 默认值应为 true（已有断言，顺带保护）"
        );
    }

    #[test]
    fn 文件树可见性_序列化往返_隐藏态() {
        // filetree_visible=false 写盘后加载应保持 false。
        let p = temp_path("ft_roundtrip_hidden");
        let s = Settings {
            layout: LayoutSettings {
                filetree_visible: false,
                ..LayoutSettings::default()
            },
            ..Settings::default()
        };
        s.save_to(&p).expect("写盘失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            !loaded.layout.filetree_visible,
            "filetree_visible=false 写盘后加载应为 false"
        );
    }

    #[test]
    fn 文件树可见性_旧文件缺字段补true() {
        // 第十九轮前的旧 settings.json 无 filetree_visible 字段：
        // 加载后应静默补 true（平滑升级，不影响已有用户体验）。
        let p = temp_path("ft_compat_old");
        std::fs::write(
            &p,
            r#"{ "layout": { "sidebar_width": 200.0, "filetree_width": 240.0, "sidebar_visible": true } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            loaded.layout.filetree_visible,
            "旧文件缺 filetree_visible 字段时应补 true"
        );
        assert_eq!(loaded.layout.sidebar_width, 200.0, "其余字段不受影响");
        assert_eq!(loaded.layout.filetree_width, 240.0, "其余字段不受影响");
    }

    #[test]
    fn 文件树可见性_字段级容错_类型非法降级true() {
        // filetree_visible 写成非布尔类型时仅该字段降级 true，其余字段保留。
        let p = temp_path("ft_lenient");
        std::fs::write(
            &p,
            r#"{ "layout": { "sidebar_width": 200.0, "filetree_visible": "yes" } }"#,
        )
        .expect("写测试文件失败");
        let loaded = Settings::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(
            loaded.layout.filetree_visible,
            "类型非法字段降级为默认值 true"
        );
        assert_eq!(loaded.layout.sidebar_width, 200.0, "好字段应保留");
    }
}
