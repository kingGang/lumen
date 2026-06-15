//! 会话列表持久化（F4 / F5 分屏升级）：应用数据目录下的 `sessions.json`
//! （目录按构建类型隔离，见 [`crate::paths`]——release `Lumen/`、
//! debug `Lumen-dev/`）。
//!
//! M3.7 起为 v2 嵌套结构 `{version: 2, active_tab, tabs: [{custom_title,
//! focused, panes: [{cwd}]}]}`：每个 tab 保存自定义名、窗格列表（各窗格
//! 最后上报的 cwd，OSC 9;9）与焦点窗格下标，外加激活 tab 下标；启动时
//! 按结构逐窗格重开 shell（初始工作目录用保存的 cwd，已失效则回退
//! 默认目录并提示）。M3.7c（F7③）在 tab 条目上增量扩展布局比例
//! `row_weights`/`col_weights`（serde default：旧 v2 文件无字段自动
//! 均分，纯可选字段不 bump 版本号，新旧双向兼容）。屏幕内容/滚动
//! 历史不持久化——重启是新 shell，这是预期行为。读侧兼容两种旧格式
//! （写侧只写 v2）：
//! - M3.6b 的 v1 平铺格式（`entries` 字段）自动迁移为「每条目一个
//!   单窗格 tab」；
//! - M3.7 批1 的过渡格式（嵌套 tabs 但根字段叫 `active`、无 version）
//!   经 serde alias 直接读取。
//!
//! 写盘时机（main.rs）：结构性变更（新建/关闭/重命名/切换激活/切换
//! 焦点窗格/增删窗格）即写；cwd 随提示符上报变化时与上次快照比对后
//! 按需写（写频≈用户 cd 频率）。原子写盘模式与 settings.rs 一致
//! （同目录临时文件 + rename 覆盖）。缺失/损坏 → 启动回退单默认
//! 会话，损坏记日志警告、不 panic。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session::MAX_PANES;

/// 当前写盘格式版本（F5 嵌套结构 = 2；M3.6b 平铺 = 1，无该字段）。
const FORMAT_VERSION: u32 = 2;

/// 文件中缺 `version` 字段时的读侧默认值（v1 平铺与批1 过渡格式都
/// 没有该字段）。结构识别不依赖版本号（按字段形态），它只用于日志
/// 与未来格式演进的兼容判断。
fn legacy_format_version() -> u32 {
    1
}

/// 单个窗格的持久化条目。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneEntry {
    /// 最后上报的 cwd（OSC 9;9）；恢复时作为 shell 初始工作目录。
    pub cwd: Option<PathBuf>,
    /// 窗格用户自定义名（需求2）：双击/右键标题栏重命名后的名字；空白
    /// /缺失（旧配置）反序列化为 None（容器级 #[serde(default)] 保证向后
    /// 兼容）。非空时窗格标题优先显示它。
    pub custom_title: Option<String>,
}

impl PaneEntry {
    /// 恢复时可用的初始 cwd：仅当保存的路径仍是存在的目录。失效
    /// （目录被删/重命名/网络盘离线）返回 None，调用方回退默认
    /// 目录并 toast 提示。
    pub fn usable_cwd(&self) -> Option<&Path> {
        self.cwd.as_deref().filter(|p| p.is_dir())
    }
}

/// 单个 tab 的持久化条目（F5：分屏后每窗格保存自己的 cwd；F7：外加
/// 布局比例权重）。含 f32 权重后不再可派生 Eq（PartialEq 足够：快照
/// 比对与单测都用它）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabEntry {
    /// 用户重命名的标题（None = 跟随默认标题规则：焦点窗格 cwd >
    /// OSC 标题）。
    pub custom_title: Option<String>,
    /// 窗格条目（布局顺序：先上排后下排、行内自左向右）。
    pub panes: Vec<PaneEntry>,
    /// 焦点窗格下标（加载时已夹紧到合法范围）。
    pub focused: usize,
    /// 每排高度权重（F7③ 可调比例）。形状须与窗格数推导的网格结构
    /// 一致才生效（layout::PaneLayout::from_weights 校验），否则恢复
    /// 时回退均分；旧 v2 文件无此字段 → serde default 空 = 均分，
    /// **无需 bump 版本号**（纯增量可选字段，新旧双向兼容）。
    pub row_weights: Vec<f32>,
    /// 每排内各列宽度权重（同上）。
    pub col_weights: Vec<Vec<f32>>,
    /// 最大化窗格下标（P14，重启保持）。读侧夹紧：越界（手改文件/
    /// 恢复时窗格 spawn 失败致数量变化）降级 None；旧文件无字段 →
    /// serde default None，纯增量可选字段不 bump 版本号。
    pub maximized: Option<usize>,
}

/// 旧版平铺条目（M3.6b 格式，仅读侧迁移用）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
struct LegacyEntry {
    custom_title: Option<String>,
    cwd: Option<PathBuf>,
}

/// sessions.json 根结构。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsFile {
    /// 格式版本：写盘恒为 [`FORMAT_VERSION`]（=2）；读侧加载后也归一
    /// 为当前版本（迁移完成即是新格式）。缺字段（v1/批1 过渡）默认 1。
    #[serde(default = "legacy_format_version")]
    pub version: u32,
    /// tab 条目（侧栏自上而下的顺序）。
    pub tabs: Vec<TabEntry>,
    /// 旧平铺格式字段（M3.6b）：读侧迁移为单窗格 tab；写侧恒空、
    /// 不序列化。
    #[serde(skip_serializing)]
    entries: Vec<LegacyEntry>,
    /// 激活 tab 下标（加载时已夹紧到合法范围）。批1 过渡格式的字段
    /// 名 `active` 经 alias 兼容读取，写盘按规格名 `active_tab`。
    #[serde(alias = "active")]
    pub active_tab: usize,
}

impl SessionsFile {
    /// 以当前版本格式构造（main 构造持久化快照用；`entries` 为模块
    /// 私有的迁移残留字段，外部不可见）。
    pub fn new(tabs: Vec<TabEntry>, active_tab: usize) -> Self {
        Self {
            version: FORMAT_VERSION,
            tabs,
            entries: Vec::new(),
            active_tab,
        }
    }

    /// 持久化路径：应用数据目录下的 `sessions.json`（目录按构建类型
    /// 隔离，见 [`crate::paths`]——release `Lumen/`、debug `Lumen-dev/`）。
    /// 数据目录不可用（极端定制环境）返回 None，本次运行不持久化。
    pub fn path() -> Option<PathBuf> {
        crate::paths::data_file("sessions.json")
    }

    /// 启动加载：缺失/损坏/空条目返回 None（回退单默认会话），损坏
    /// 记警告日志，绝不 panic。
    pub fn load() -> Option<Self> {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => {
                log::warn!("LOCALAPPDATA 未设置，会话列表不持久化");
                None
            }
        }
    }

    /// 从指定路径加载（拆出来供单测注入临时路径）。
    pub fn load_from(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                log::info!("会话列表文件不存在，按单默认会话启动: {}", path.display());
                return None;
            }
            Err(e) => {
                log::warn!("读会话列表失败，按单默认会话启动 {}: {e}", path.display());
                return None;
            }
        };
        // 与 settings.rs 同款 BOM 防御（用户用 PowerShell 重定向手改
        // 文件时的常见产物）。
        let text = text.trim_start_matches('\u{feff}');
        let mut file = match serde_json::from_str::<Self>(text) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "会话列表解析失败，按单默认会话启动（原文件保留，下次写盘才覆盖）{}: {e}",
                    path.display()
                );
                return None;
            }
        };
        if file.version > FORMAT_VERSION {
            // 未来版本写的文件（降级运行场景）：尽力按当前结构加载
            // （serde 忽略未知字段），提示一次。
            log::warn!(
                "会话列表为较新格式版本 v{}（当前支持 v{FORMAT_VERSION}），尽力加载",
                file.version
            );
        }
        // 旧平铺格式迁移：每个条目变成一个单窗格 tab。新旧字段同时
        // 出现（异常手改）时以新格式为准。
        if file.tabs.is_empty() && !file.entries.is_empty() {
            file.tabs = file
                .entries
                .drain(..)
                .map(|e| TabEntry {
                    custom_title: e.custom_title,
                    panes: vec![PaneEntry {
                        cwd: e.cwd,
                        custom_title: None,
                    }],
                    focused: 0,
                    // 旧平铺条目无布局概念：空权重 = 恢复侧均分。
                    ..Default::default()
                })
                .collect();
            log::info!(
                "会话列表：旧平铺格式已迁移为 {} 个单窗格 tab",
                file.tabs.len()
            );
        }
        file.entries.clear();
        // 结构清洗（手改文件/旧版本残留的非法值）：空窗格列表的 tab
        // 丢弃；窗格数超上限截断；焦点下标夹紧；空白自定义名视同未
        // 命名（重命名路径不会写空名）。
        file.tabs.retain(|t| !t.panes.is_empty());
        for tab in &mut file.tabs {
            tab.panes.truncate(MAX_PANES);
            tab.focused = tab.focused.min(tab.panes.len() - 1);
            // 最大化下标越界（手改/截断后）降级 None；单窗格无最大化
            // 语义也归 None（P14）。
            if tab
                .maximized
                .is_some_and(|m| m >= tab.panes.len() || tab.panes.len() == 1)
            {
                tab.maximized = None;
            }
            if tab
                .custom_title
                .as_ref()
                .is_some_and(|t| t.trim().is_empty())
            {
                tab.custom_title = None;
            }
        }
        if file.tabs.is_empty() {
            // 空列表 = 上次退出前关掉了全部 tab：与缺失同义。
            return None;
        }
        file.active_tab = file.active_tab.min(file.tabs.len() - 1);
        // 加载即归一为当前版本（迁移/清洗完成后内存中就是 v2 结构；
        // 快照比对与下次写盘都以当前版本为准）。
        file.version = FORMAT_VERSION;
        Some(file)
    }

    /// 写盘（结构性变更/cwd 上报变化时由 main 调用）。失败只记日志
    /// ——会话簿记不应打扰终端使用；无持久化路径时静默跳过。
    pub fn save(&self) {
        let Some(p) = Self::path() else {
            return;
        };
        match self.save_to(&p) {
            // 写盘日志带 PID（F8 纵深防御）：多开放行（测试场景）时
            // 会话快照仍可能互踩，便于排查「后写者赢」来自哪个进程。
            Ok(()) => log::debug!("会话列表写盘 pid={}: {}", std::process::id(), p.display()),
            Err(e) => log::error!("写会话列表失败 {}: {e:#}", p.display()),
        }
    }

    /// 原子写盘：先写同目录临时文件再改名覆盖，防半写损坏
    /// （settings.rs 同款模式）。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("会话列表路径无父目录")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建持久化目录失败: {}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).context("序列化会话列表失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("写会话列表临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换会话列表文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立文件名，避免并行测试互踩。
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumen_sessions_test_{}_{name}.json",
            std::process::id()
        ))
    }

    fn pane(cwd: Option<&str>) -> PaneEntry {
        PaneEntry {
            cwd: cwd.map(PathBuf::from),
            custom_title: None,
        }
    }

    #[test]
    fn 嵌套格式序列化往返() {
        let f = SessionsFile::new(
            vec![
                TabEntry {
                    custom_title: Some("构建机".to_owned()),
                    panes: vec![
                        pane(Some(r"C:\proj\lumen")),
                        pane(Some(r"D:\work 空格\中文目录")),
                    ],
                    focused: 1,
                    ..Default::default()
                },
                TabEntry {
                    custom_title: None,
                    panes: vec![pane(None)],
                    focused: 0,
                    ..Default::default()
                },
            ],
            1,
        );
        let p = temp_path("roundtrip");
        f.save_to(&p).expect("写盘失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, f);
    }

    #[test]
    fn 带布局权重往返() {
        // F7③：布局比例权重随 tab 条目写盘/读回，逐位不失真
        // （serde_json 的 f32 序列化可精确往返）。
        let f = SessionsFile::new(
            vec![TabEntry {
                custom_title: None,
                panes: vec![pane(Some(r"C:\a")), pane(None), pane(None), pane(None)],
                focused: 2,
                row_weights: vec![0.3, 0.7],
                col_weights: vec![vec![0.25, 0.75], vec![0.6, 0.4]],
                maximized: None,
            }],
            0,
        );
        let p = temp_path("weights_roundtrip");
        f.save_to(&p).expect("写盘失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded, f);
        assert_eq!(loaded.tabs[0].row_weights, vec![0.3, 0.7]);
        assert_eq!(loaded.tabs[0].col_weights[1], vec![0.6, 0.4]);
    }

    #[test]
    fn 最大化下标往返与夹紧() {
        // P14：maximized 随 tab 条目写盘/读回；越界与单窗格降级 None。
        let f = SessionsFile::new(
            vec![TabEntry {
                panes: vec![pane(None), pane(None), pane(None)],
                focused: 1,
                maximized: Some(2),
                ..Default::default()
            }],
            0,
        );
        let p = temp_path("maximized_roundtrip");
        f.save_to(&p).expect("写盘失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs[0].maximized, Some(2));

        // 越界（手改文件）→ None。
        let p = temp_path("maximized_clamp");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "panes": [ {}, {} ], "maximized": 9 } ], "active_tab": 0 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs[0].maximized, None, "越界应降级 None");

        // 单窗格无最大化语义 → None；旧文件无字段 → None。
        let p = temp_path("maximized_single");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "panes": [ {} ], "maximized": 0 }, { "panes": [ {}, {} ] } ], "active_tab": 0 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs[0].maximized, None, "单窗格应降级 None");
        assert_eq!(loaded.tabs[1].maximized, None, "缺字段默认 None");
    }

    #[test]
    fn 旧v2无权重字段自动均分() {
        // M3.7 批2 写出的 v2（无 row_weights/col_weights）：serde
        // default 补空向量，恢复侧（main）据此回退均分布局——版本号
        // 不 bump，旧文件原样可读。
        let p = temp_path("v2_no_weights");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "panes": [ { "cwd": "C:\\a" }, {} ], "focused": 1 } ], "active_tab": 0 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert!(loaded.tabs[0].row_weights.is_empty(), "无字段应为空向量");
        assert!(loaded.tabs[0].col_weights.is_empty());
        // 空权重经布局校验回退均分（恢复链路的实际判定）。
        assert!(crate::shell::layout::PaneLayout::from_weights(
            2,
            &loaded.tabs[0].row_weights,
            &loaded.tabs[0].col_weights
        )
        .is_none());
    }

    #[test]
    fn 旧平铺格式自动迁移() {
        // M3.6b 写出的格式：entries 平铺 + active。每条目应迁移为
        // 单窗格 tab，自定义名保留、cwd 进唯一窗格、焦点为 0。
        let p = temp_path("legacy");
        std::fs::write(
            &p,
            r#"{ "entries": [ { "custom_title": "构建机", "cwd": "C:\\a" }, { "cwd": "C:\\b" } ], "active": 1 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.tabs[0].custom_title.as_deref(), Some("构建机"));
        assert_eq!(loaded.tabs[0].panes, vec![pane(Some(r"C:\a"))]);
        assert_eq!(loaded.tabs[0].focused, 0);
        assert_eq!(loaded.tabs[1].panes, vec![pane(Some(r"C:\b"))]);
        assert_eq!(loaded.active_tab, 1);
        assert_eq!(loaded.version, 2, "迁移后版本应归一为 2");
        // 迁移后再写盘 = v2 新格式（version/active_tab，不再含
        // entries 字段）。
        let p2 = temp_path("legacy_rewrite");
        loaded.save_to(&p2).expect("写盘失败");
        let text = std::fs::read_to_string(&p2).expect("读回失败");
        let _ = std::fs::remove_file(&p2);
        assert!(text.contains("\"tabs\""), "应写新格式: {text}");
        assert!(text.contains("\"version\": 2"), "应带版本号: {text}");
        assert!(text.contains("\"active_tab\""), "应写规格字段名: {text}");
        assert!(!text.contains("\"entries\""), "不应再写旧字段: {text}");
    }

    #[test]
    fn 批1过渡格式_active别名与缺version() {
        // M3.7 批1 写盘的过渡格式：嵌套 tabs 但根字段叫 active、无
        // version 字段。alias 兼容读取，加载后版本归一为 2。
        let p = temp_path("transitional");
        std::fs::write(
            &p,
            r#"{ "tabs": [ { "panes": [ { "cwd": "C:\\a" } ] }, { "panes": [ {}, {} ], "focused": 1 } ], "active": 1 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.active_tab, 1, "active 别名应读入 active_tab");
        assert_eq!(loaded.tabs[1].focused, 1);
        assert_eq!(loaded.version, 2, "加载后版本应归一为 2");
    }

    #[test]
    fn 损坏文件降级() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{ 这不是 json !!!").expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(loaded.is_none(), "损坏文件应降级 None（单默认会话）");
    }

    #[test]
    fn 缺失文件降级() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert!(SessionsFile::load_from(&p).is_none());
    }

    #[test]
    fn 空tab列表视同缺失() {
        let p = temp_path("empty");
        std::fs::write(&p, r#"{ "version": 2, "tabs": [], "active_tab": 0 }"#)
            .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p);
        let _ = std::fs::remove_file(&p);
        assert!(loaded.is_none(), "空列表应回退单默认会话");
    }

    #[test]
    fn 空窗格tab被丢弃() {
        // 手改文件可能出现 panes 为空的 tab：丢弃该 tab，整体仍可加载。
        let p = temp_path("empty_panes");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "panes": [] }, { "panes": [ { "cwd": "C:\\a" } ] } ], "active_tab": 1 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.active_tab, 0, "丢 tab 后激活下标应夹紧");
    }

    #[test]
    fn 下标越界与窗格超限夹紧() {
        let p = temp_path("clamp");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "panes": [ {}, {}, {}, {}, {}, {}, {}, {} ], "focused": 99 } ], "active_tab": 9 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs[0].panes.len(), MAX_PANES, "窗格数应截断到上限");
        assert_eq!(loaded.tabs[0].focused, MAX_PANES - 1);
        assert_eq!(loaded.active_tab, 0);
    }

    #[test]
    fn 空白自定义名视同未命名() {
        let p = temp_path("blank_title");
        std::fs::write(
            &p,
            r#"{ "version": 2, "tabs": [ { "custom_title": "   ", "panes": [ { "cwd": "C:\\a" } ] } ], "active_tab": 0 }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert!(loaded.tabs[0].custom_title.is_none());
    }

    #[test]
    fn 缺字段平滑加载() {
        // 手改/未来版本缺字段：serde(default) 补默认值。
        let p = temp_path("partial");
        std::fs::write(&p, r#"{ "tabs": [ { "panes": [ {} ] } ] }"#).expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs.len(), 1);
        assert!(loaded.tabs[0].custom_title.is_none());
        assert!(loaded.tabs[0].panes[0].cwd.is_none());
        assert_eq!(loaded.active_tab, 0);
    }

    #[test]
    fn 未来版本尽力加载() {
        // version 大于当前支持版本（降级运行）：未知字段忽略、已知
        // 结构尽力加载，版本归一为当前版本。
        let p = temp_path("future");
        std::fs::write(
            &p,
            r#"{ "version": 9, "tabs": [ { "panes": [ { "cwd": "C:\\a", "future_field": 1 } ] } ], "active_tab": 0, "extra": true }"#,
        )
        .expect("写测试文件失败");
        let loaded = SessionsFile::load_from(&p).expect("应能加载");
        let _ = std::fs::remove_file(&p);
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].panes, vec![pane(Some(r"C:\a"))]);
        assert_eq!(loaded.version, 2);
    }

    #[test]
    fn cwd失效回退() {
        // 存在的目录 → 可用；不存在的目录/指向文件的路径 → None。
        let dir = std::env::temp_dir();
        let ok = pane(dir.to_str());
        assert_eq!(ok.usable_cwd(), Some(dir.as_path()));

        let gone = pane(Some(r"C:\lumen_不存在的目录_单测专用"));
        assert!(gone.usable_cwd().is_none(), "失效目录应回退 None");

        let file_path = dir.join(format!("lumen_sessions_cwd_{}.txt", std::process::id()));
        std::fs::write(&file_path, b"x").expect("写测试文件失败");
        let not_dir = PaneEntry {
            cwd: Some(file_path.clone()),
            custom_title: None,
        };
        let usable = not_dir.usable_cwd().is_none();
        let _ = std::fs::remove_file(&file_path);
        assert!(usable, "指向文件的 cwd 不可用");

        assert!(pane(None).usable_cwd().is_none());
    }
}
