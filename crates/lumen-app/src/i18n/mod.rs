//! 多语言（i18n）支持模块（F6）。
//!
//! # 设计原则
//! - **零依赖**：纯静态表，无运行时解析，无外部 crate。
//! - **编译期完备性**：三语实例均实现 [`strings::Strings`] 的全部字段——
//!   任何实例缺字段即编译报错，杜绝遗漏翻译。
//! - **即时生效**：egui 即时模式，语言切换后下一帧自动使用新表。
//! - **线程安全**：全局语言状态用 [`AtomicU8`] 存储；UI 单线程写，读无锁。
//!
//! # 用法
//! ```ignore
//! use crate::i18n;
//!
//! // 读当前语言文案
//! let s = i18n::strings();
//! ui.label(s.sidebar_sessions);
//!
//! // 插值（单参）
//! let msg = i18n::fmt1(s.pane_count_fmt, 3);
//!
//! // 插值（双参）
//! let msg = i18n::fmt2(s.toast_font_fallback_fmt, "Fira Code", "Consolas");
//!
//! // 切换语言（设置页 ComboBox 选中时）
//! i18n::set_language(Language::ZhTw);
//! ```
//!
//! # 新增文案纪律（强制）
//! 1. 在 [`strings::Strings`] 加字段（`pub xxx: &'static str`）并写 `///` rustdoc；
//! 2. 在 [`zh_cn`]、[`zh_tw`]、[`en`] 三个文件各填对应值；
//! 3. 插值文案：单参用 `{}` 占位 + [`fmt1`]；双参用 `{0}` `{1}` + [`fmt2`]；
//! 4. 后台线程不得直接保存翻译文本——改为传枚举 + 原始参数，UI 侧展示时查表组装。
//!
//! 违反第 1/2 条 → 编译报错；违反第 3/4 条 → clippy/代码审查拦截。

pub mod en;
pub mod strings;
pub mod zh_cn;
pub mod zh_tw;

pub use strings::Strings;

use std::sync::atomic::{AtomicU8, Ordering};

/// 支持的语言枚举。
///
/// `#[repr(u8)]` 保证可以无损存入 [`AtomicU8`]。
/// serde rename 使 settings.json 中的字符串符合 BCP 47 惯例。
/// `Default = ZhCn`（海风哥拍板：默认简体中文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum Language {
    /// 简体中文（默认）
    #[serde(rename = "zh-CN")]
    #[default]
    ZhCn = 0,
    /// 繁體中文
    #[serde(rename = "zh-TW")]
    ZhTw = 1,
    /// English
    #[serde(rename = "en")]
    En = 2,
}

impl Language {
    /// 返回语言在界面上固定显示的原生名称。
    ///
    /// 这三个 label 不随界面语言变化（业界惯例：让用户在不懂当前界面
    /// 语言的情况下也能识别并切换到熟悉的语言）。
    pub fn label(self) -> &'static str {
        match self {
            Self::ZhCn => "简体中文",
            Self::ZhTw => "繁體中文",
            Self::En => "English",
        }
    }

    /// 从 u8 还原；未知值回退 ZhCn（防 settings.json 损坏）。
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::ZhCn,
            1 => Self::ZhTw,
            2 => Self::En,
            _ => Self::ZhCn,
        }
    }

    /// 所有语言的有序列表（ComboBox 枚举用）。
    pub const ALL: [Self; 3] = [Self::ZhCn, Self::ZhTw, Self::En];
}

/// 全局当前语言（AtomicU8；Relaxed 即可，单 UI 线程写、读）。
static CURRENT_LANG: AtomicU8 = AtomicU8::new(Language::ZhCn as u8);

/// 设置全局语言（设置页 ComboBox 选中时、启动 load 后调用）。
pub fn set_language(lang: Language) {
    CURRENT_LANG.store(lang as u8, Ordering::Relaxed);
}

/// 读取当前全局语言。
pub fn current_language() -> Language {
    Language::from_u8(CURRENT_LANG.load(Ordering::Relaxed))
}

/// 取当前语言的文案表引用（`&'static Strings`）。
///
/// egui 即时模式：每帧调用此函数即可，语言切换后下一帧自动生效。
pub fn strings() -> &'static Strings {
    match current_language() {
        Language::ZhCn => &zh_cn::STRINGS,
        Language::ZhTw => &zh_tw::STRINGS,
        Language::En => &en::STRINGS,
    }
}

/// 单参模板替换：`{}` 占位符替换为 `a` 的 Display 表示。
///
/// 只替换第一个 `{}`（[`str::replacen`] 限制为 1 次），防止模板内容
/// 本身含有 `{}` 时被二次替换（已知限制：若参数文本本身含 `{}`，替换
/// 后的结果中该 `{}` **不会**再被处理——行为符合预期，测试已钉住）。
pub fn fmt1(tpl: &str, a: impl std::fmt::Display) -> String {
    tpl.replacen("{}", &a.to_string(), 1)
}

/// 双参模板替换：`{0}` 替换为 `a`，`{1}` 替换为 `b`。
///
/// 先替 `{0}` 后替 `{1}`，二者互不干扰（参数文本本身含 `{1}` 时：
/// 替换 `{0}` 后参数值进入结果字符串，后续替换 `{1}` 时恰好命中——
/// 属于已知边界行为，用户应避免参数文本含 `{0}` / `{1}` 字面量；
/// 测试已记录此行为）。
pub fn fmt2(tpl: &str, a: impl std::fmt::Display, b: impl std::fmt::Display) -> String {
    tpl.replace("{0}", &a.to_string())
        .replace("{1}", &b.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— Language serde 往返 ——

    #[test]
    fn language_serde_roundtrip_zh_cn() {
        let s = serde_json::to_string(&Language::ZhCn).expect("序列化失败");
        assert_eq!(s, "\"zh-CN\"");
        let back: Language = serde_json::from_str(&s).expect("反序列化失败");
        assert_eq!(back, Language::ZhCn);
    }

    #[test]
    fn language_serde_roundtrip_zh_tw() {
        let s = serde_json::to_string(&Language::ZhTw).expect("序列化失败");
        assert_eq!(s, "\"zh-TW\"");
        let back: Language = serde_json::from_str(&s).expect("反序列化失败");
        assert_eq!(back, Language::ZhTw);
    }

    #[test]
    fn language_serde_roundtrip_en() {
        let s = serde_json::to_string(&Language::En).expect("序列化失败");
        assert_eq!(s, "\"en\"");
        let back: Language = serde_json::from_str(&s).expect("反序列化失败");
        assert_eq!(back, Language::En);
    }

    #[test]
    fn language_serde_unknown_variant_fails() {
        // 未知 variant 应反序列化失败（字段级容错在 Settings::load_from 处理）。
        let result: Result<Language, _> = serde_json::from_str("\"fr\"");
        assert!(result.is_err(), "未知语言值应反序列化失败");
    }

    // —— 旧 settings.json 无 language 字段兼容 ——

    #[test]
    fn settings_without_language_field_defaults_to_zh_cn() {
        // 旧版 settings.json 无 language 字段：#[serde(default)] 补默认值 ZhCn。
        let json = r#"{ "appearance": { "font_size": 15.0 } }"#;
        let s: crate::settings::Settings = serde_json::from_str(json).expect("解析失败");
        assert_eq!(s.language, Language::ZhCn, "缺 language 字段应默认 ZhCn");
    }

    #[test]
    fn settings_with_language_field_roundtrip() {
        let s = crate::settings::Settings {
            language: Language::En,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).expect("序列化失败");
        let back: crate::settings::Settings = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(back.language, Language::En);
    }

    // —— fmt1 ——

    #[test]
    fn fmt1_basic() {
        assert_eq!(fmt1("还有 {} 项", 5usize), "还有 5 项");
    }

    #[test]
    fn fmt1_only_first_placeholder_replaced() {
        // 模板含两个 `{}`：只替换第一个。
        assert_eq!(fmt1("{} and {}", "A"), "A and {}");
    }

    #[test]
    fn fmt1_param_contains_braces() {
        // 参数文本本身含 `{}`：替换后结果内的 `{}` 不被二次展开
        // （replacen 已执行完毕，行为符合预期，本测试钉住该边界）。
        let result = fmt1("value: {}", "{}");
        assert_eq!(result, "value: {}", "参数内含 {{}} 时不应被二次替换");
    }

    #[test]
    fn fmt1_no_placeholder() {
        assert_eq!(fmt1("no placeholder", 42usize), "no placeholder");
    }

    // —— fmt2 ——

    #[test]
    fn fmt2_basic() {
        assert_eq!(
            fmt2("深色：{0} ｜ 浅色：{1}", "Dracula", "Solarized Light"),
            "深色：Dracula ｜ 浅色：Solarized Light"
        );
    }

    #[test]
    fn fmt2_order_independent() {
        // {1} 在 {0} 前出现时同样正确。
        assert_eq!(fmt2("{1} then {0}", "A", "B"), "B then A");
    }

    #[test]
    fn fmt2_param_contains_placeholder() {
        // 参数 a 含 "{1}"：替换 {0} 后，结果中存在 "{1}"，之后替换 {1}
        // 会命中它——属于已知边界行为，本测试记录该行为（见文档注释）。
        let result = fmt2("{0} and {1}", "{1}", "B");
        // {0} → "{1}"，此时字符串为 "{1} and {1}"；替换 {1} → "B"
        // 结果为 "B and B"。
        assert_eq!(result, "B and B", "已知边界：参数含 {{1}} 时两处都被替换");
    }

    #[test]
    fn fmt2_no_placeholders() {
        assert_eq!(fmt2("static text", "X", "Y"), "static text");
    }

    #[test]
    fn update_modal_version_fmt_占位符正确_三语() {
        // 发布只走 GitHub（不发 Gitee）后，弹窗版本行只剩版本号（单参 fmt1）。
        // 钉住三语模板都含一个 {} 且 fmt1 能把版本号插入、无残留占位符。
        for s in [&zh_cn::STRINGS, &zh_tw::STRINGS, &en::STRINGS] {
            let out = fmt1(s.update_modal_version_fmt, "0.2.0");
            assert!(out.contains("0.2.0"), "版本号应被替换: {out}");
            assert!(!out.contains("{}"), "占位符应被替换: {out}");
        }
    }

    // —— strings() 三语覆盖校验（抽查部分字段，编译期全量已保证）——

    #[test]
    fn strings_zh_cn_spot_check() {
        let s = &zh_cn::STRINGS;
        assert_eq!(s.sidebar_sessions, "会话");
        assert_eq!(s.menu_rename, "重命名");
        assert!(!s.login_btn.is_empty());
    }

    #[test]
    fn strings_zh_tw_spot_check() {
        let s = &zh_tw::STRINGS;
        assert_eq!(s.sidebar_sessions, "工作階段");
        assert_eq!(s.menu_rename, "重新命名");
        assert!(!s.login_btn.is_empty());
    }

    #[test]
    fn strings_en_spot_check() {
        let s = &en::STRINGS;
        assert_eq!(s.sidebar_sessions, "Sessions");
        assert_eq!(s.menu_rename, "Rename");
        assert!(!s.login_btn.is_empty());
    }
}
