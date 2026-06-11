//! 账号数据层（M3.5）：本地 mock 登录的档案持久化。
//!
//! 真账号后端 M5 才做（docs/M3应用外壳设计.md §7），本期为纯本地
//! mock：登录只做格式校验（[`mock_login`]：邮箱含 `@` 且密码非空），
//! 成功后把身份信息写 `%LOCALAPPDATA%/Lumen/profile.json`。
//! **文件中绝不存密码**——[`Profile`] 没有密码字段，密码仅在校验
//! 瞬间过手、不进任何持久化路径。
//!
//! 持久化模式与 settings.rs 同款：启动加载（缺失 = 未登录；损坏 =
//! 未登录 + 日志警告，绝不 panic，原文件保留现场）；写盘走「同目录
//! 临时文件 + rename」原子替换；登出 = 删除文件。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 登录档案（mock）。`None` 即未登录；顶栏头像、头像菜单、设置页
/// Account 三处 UI 同源 main 持有的 `Option<Profile>`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// 登录邮箱。
    pub email: String,
    /// 展示名（登录时取邮箱 `@` 前段）。
    pub display_name: String,
    /// 登录时刻（Unix 秒，展示/排查用）。
    pub logged_in_at: u64,
}

impl Profile {
    /// 头像首字母：展示名首字符大写（CJK 字符原样；空名回退 "?"）。
    pub fn avatar_letter(&self) -> String {
        self.display_name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_owned())
    }

    /// 档案文件路径：`%LOCALAPPDATA%/Lumen/profile.json`。
    /// 环境变量缺失（极端定制环境）返回 None，登录态仅在内存生效。
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join("Lumen").join("profile.json"))
    }

    /// 启动加载：缺失 = 未登录；损坏 = 未登录 + 日志警告，不 panic。
    pub fn load() -> Option<Self> {
        let p = Self::path()?;
        Self::load_from(&p)
    }

    /// 从指定路径加载（拆出来供单测注入临时路径）。
    pub fn load_from(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                log::info!("profile 文件不存在，未登录: {}", path.display());
                return None;
            }
            Err(e) => {
                log::warn!("读 profile 文件失败，按未登录处理 {}: {e}", path.display());
                return None;
            }
        };
        // PowerShell 5.1 写文件默认带 UTF-8 BOM，serde 不认 BOM 会把
        // 好文件误判损坏（误降级未登录），解析前剥掉（M3 审查追加项）。
        match serde_json::from_str::<Self>(text.trim_start_matches('\u{feff}')) {
            Ok(mut p) => {
                p.sanitize();
                if p.email.is_empty() {
                    log::warn!(
                        "profile 缺少邮箱，按未登录处理（原文件保留，重新登录时覆盖）: {}",
                        path.display()
                    );
                    return None;
                }
                Some(p)
            }
            Err(e) => {
                log::warn!(
                    "profile 解析失败，按未登录处理（原文件保留，重新登录时覆盖）{}: {e}",
                    path.display()
                );
                None
            }
        }
    }

    /// 写盘（登录成功时调用）。失败仅记日志——写不进盘不影响本次
    /// 运行的登录态，只是重启后回未登录。
    pub fn save(&self) {
        let Some(p) = Self::path() else {
            return;
        };
        if let Err(e) = self.save_to(&p) {
            log::error!("写 profile 文件失败 {}: {e:#}", p.display());
        }
    }

    /// 原子写盘：先写同目录临时文件再改名覆盖，防半写损坏。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("profile 路径无父目录")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建 profile 目录失败: {}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).context("序列化 profile 失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("写 profile 临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换 profile 文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 登出：删除档案文件（文件本就不存在视为已删，失败仅记日志）。
    pub fn delete() {
        let Some(p) = Self::path() else {
            return;
        };
        Self::delete_at(&p);
    }

    /// 删除指定路径的档案（拆出来供单测注入临时路径）。
    pub fn delete_at(path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => log::error!("删除 profile 文件失败 {}: {e}", path.display()),
        }
    }

    /// 加载后规整：字段去首尾空白；展示名缺失（用户手改文件删掉了）
    /// 时由邮箱 `@` 前段重新推导。
    fn sanitize(&mut self) {
        self.email = self.email.trim().to_owned();
        self.display_name = self.display_name.trim().to_owned();
        if self.display_name.is_empty() {
            self.display_name = display_name_of(&self.email);
        }
    }
}

/// mock 登录校验：邮箱含 `@`（且 `@` 前后段非空）、密码非空即成功，
/// 展示名取 `@` 前段。失败返回登录界面展示的红字文案。
/// 密码仅在此过手用于「非空」校验，**绝不落盘**。
pub fn mock_login(email: &str, password: &str) -> std::result::Result<Profile, String> {
    let email = email.trim();
    let valid = email
        .split_once('@')
        .is_some_and(|(user, host)| !user.is_empty() && !host.is_empty());
    if !valid {
        return Err("邮箱格式不正确（需形如 name@example.com）".to_owned());
    }
    if password.is_empty() {
        return Err("请输入密码".to_owned());
    }
    let logged_in_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default(); // 系统时钟早于 1970 的极端情形：记 0，不炸
    Ok(Profile {
        email: email.to_owned(),
        display_name: display_name_of(email),
        logged_in_at,
    })
}

/// 邮箱 `@` 前段作为展示名（无 `@` 时取整串，仅作降级兜底）。
fn display_name_of(email: &str) -> String {
    email.split('@').next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立文件名，避免并行测试互踩。
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumen_profile_test_{}_{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn 序列化往返() {
        let p = mock_login("jimhy@example.com", "secret").expect("mock 登录应成功");
        assert_eq!(p.display_name, "jimhy");
        let path = temp_path("roundtrip");
        p.save_to(&path).expect("写盘失败");
        // 落盘内容绝不含密码（mock 铁律）。
        let raw = std::fs::read_to_string(&path).expect("读回失败");
        assert!(!raw.contains("secret"), "profile.json 不得包含密码");
        let loaded = Profile::load_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, Some(p));
    }

    #[test]
    fn 损坏文件降级未登录() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ 这不是 json !!!").expect("写测试文件失败");
        let loaded = Profile::load_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, None);
    }

    #[test]
    fn 缺失文件即未登录() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Profile::load_from(&path), None);
    }

    #[test]
    fn 空邮箱视同损坏() {
        let path = temp_path("empty_email");
        std::fs::write(&path, r#"{ "display_name": "x", "logged_in_at": 1 }"#)
            .expect("写测试文件失败");
        let loaded = Profile::load_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, None);
    }

    #[test]
    fn bom前缀_正常解析() {
        // PowerShell 5.1 写文件默认带 UTF-8 BOM，不剥会误降级未登录。
        let path = temp_path("bom");
        std::fs::write(&path, "\u{feff}{ \"email\": \"jimhy@example.com\" }")
            .expect("写测试文件失败");
        let loaded = Profile::load_from(&path);
        let _ = std::fs::remove_file(&path);
        let p = loaded.expect("带 BOM 的合法 profile 应加载成功");
        assert_eq!(p.email, "jimhy@example.com");
        assert_eq!(p.display_name, "jimhy");
    }

    #[test]
    fn 缺展示名由邮箱推导() {
        let path = temp_path("derive_name");
        std::fs::write(&path, r#"{ "email": "haifeng@lumen.dev" }"#).expect("写测试文件失败");
        let loaded = Profile::load_from(&path);
        let _ = std::fs::remove_file(&path);
        let p = loaded.expect("应加载成功");
        assert_eq!(p.display_name, "haifeng");
        assert_eq!(p.avatar_letter(), "H");
    }

    #[test]
    fn 登出删除后回未登录() {
        let path = temp_path("logout");
        let p = mock_login("a@b.c", "pw").expect("mock 登录应成功");
        p.save_to(&path).expect("写盘失败");
        assert!(Profile::load_from(&path).is_some());
        Profile::delete_at(&path);
        assert_eq!(Profile::load_from(&path), None);
        // 重复删除（文件已不存在）不 panic。
        Profile::delete_at(&path);
    }

    #[test]
    fn mock校验规则() {
        assert!(mock_login("jimhy@example.com", "x").is_ok());
        assert!(
            mock_login("  jimhy@example.com  ", "x").is_ok(),
            "邮箱应去首尾空白"
        );
        assert!(mock_login("no-at-sign", "x").is_err(), "无 @ 应拒绝");
        assert!(mock_login("@host", "x").is_err(), "@ 前段为空应拒绝");
        assert!(mock_login("user@", "x").is_err(), "@ 后段为空应拒绝");
        assert!(mock_login("a@b.c", "").is_err(), "空密码应拒绝");
    }

    #[test]
    fn 头像首字母() {
        let p = mock_login("jimhy@example.com", "x").expect("mock 登录应成功");
        assert_eq!(p.avatar_letter(), "J");
        let empty = Profile::default();
        assert_eq!(empty.avatar_letter(), "?");
    }
}
