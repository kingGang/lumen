//! 应用数据目录解析（单一真源）。
//!
//! 所有持久化文件——`settings.json` / `profile.json` / `sessions.json` /
//! `history.jsonl`——都落在同一个应用数据目录下。该目录**按平台**取系统
//! 约定的用户数据基目录，再**按构建类型隔离**子目录：
//!
//! - Windows：`%LOCALAPPDATA%/<Lumen|Lumen-dev>/`
//! - macOS：`~/Library/Application Support/<Lumen|Lumen-dev>/`
//! - Linux/其它 unix：`$XDG_DATA_HOME`（缺省 `~/.local/share`）`/<Lumen|Lumen-dev>/`
//!
//! 这样开发期 `cargo run`（debug）读写独立子目录（`Lumen-dev`），绝不污染
//! 正式安装版的真实配置 / 登录态 / 会话 / 命令历史。与
//! [`crate::single_instance`] 的「debug 默认放开多开」是同一套「debug 自动
//! 隔离」约定。
//!
//! 逃生口：设环境变量 `LUMEN_DATA_DIR=<绝对路径>` 可把数据目录覆盖到任意
//! 位置（便于 debug 下临时调试正式数据，或自定义便携部署）。覆盖值原样
//! 使用，**不再**追加 `Lumen` 子目录。

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// release 构建的应用数据子目录名（正式安装版）。
const DIR_RELEASE: &str = "Lumen";
/// debug 构建的应用数据子目录名（开发 / 测试，与正式版隔离）。
const DIR_DEBUG: &str = "Lumen-dev";

/// 解析逻辑（纯函数，便于单测注入环境）：优先 `LUMEN_DATA_DIR` 覆盖路径
/// （非空时原样使用）；否则取平台数据基目录 `base` 下按构建类型选择的子
/// 目录。两者都缺时返回 `None`。
fn resolve(custom: Option<&OsStr>, base: Option<&OsStr>, debug: bool) -> Option<PathBuf> {
    if let Some(c) = custom {
        if !c.is_empty() {
            return Some(PathBuf::from(c));
        }
    }
    let sub = if debug { DIR_DEBUG } else { DIR_RELEASE };
    base.map(|d| PathBuf::from(d).join(sub))
}

/// 平台数据基目录（应用子目录之前的部分）：Windows `%LOCALAPPDATA%`，
/// macOS `~/Library/Application Support`，其它 unix `$XDG_DATA_HOME` 或
/// `~/.local/share`（XDG Base Directory 规范）。取不到返回 `None`。
#[cfg(windows)]
fn platform_base_dir() -> Option<OsString> {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
}

#[cfg(target_os = "macos")]
fn platform_base_dir() -> Option<OsString> {
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    Some(p.into_os_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_base_dir() -> Option<OsString> {
    // XDG_DATA_HOME 优先（用户显式配置）；否则规范默认 ~/.local/share。
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(xdg);
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    Some(p.into_os_string())
}

/// 应用数据目录：平台数据基目录下 `<Lumen|Lumen-dev>/`（按构建类型隔离），
/// 或 `LUMEN_DATA_DIR` 指定的覆盖路径。
///
/// 返回 `None` 表示既无 `LUMEN_DATA_DIR` 覆盖、平台数据基目录也解析不到
/// （极端定制环境，如 `HOME`/`LOCALAPPDATA` 均未设）——调用方据此降级为
/// 「本次运行不持久化」。
pub fn data_dir() -> Option<PathBuf> {
    resolve(
        std::env::var_os("LUMEN_DATA_DIR").as_deref(),
        platform_base_dir().as_deref(),
        cfg!(debug_assertions),
    )
}

/// 在应用数据目录下拼一个文件名得到完整路径（数据目录不可用时 `None`）。
pub fn data_file(name: &str) -> Option<PathBuf> {
    data_dir().map(|d| d.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn release_用lumen子目录() {
        let local = OsString::from(r"C:\Users\h\AppData\Local");
        let got = resolve(None, Some(local.as_os_str()), false).unwrap();
        assert_eq!(got, PathBuf::from(r"C:\Users\h\AppData\Local").join("Lumen"));
    }

    #[test]
    fn debug_用lumen_dev子目录与正式版隔离() {
        let local = OsString::from(r"C:\Users\h\AppData\Local");
        let got = resolve(None, Some(local.as_os_str()), true).unwrap();
        assert_eq!(
            got,
            PathBuf::from(r"C:\Users\h\AppData\Local").join("Lumen-dev")
        );
    }

    #[test]
    fn 环境变量覆盖优先于构建类型() {
        let custom = OsString::from(r"D:\lumen-portable");
        let local = OsString::from(r"C:\Users\h\AppData\Local");
        // debug 与 release 都应原样返回覆盖路径，且不追加 Lumen 子目录。
        for debug in [true, false] {
            let got = resolve(Some(custom.as_os_str()), Some(local.as_os_str()), debug).unwrap();
            assert_eq!(got, PathBuf::from(r"D:\lumen-portable"));
        }
    }

    #[test]
    fn 空覆盖值视作未设置() {
        let empty = OsString::new();
        let local = OsString::from(r"C:\Users\h\AppData\Local");
        let got = resolve(Some(empty.as_os_str()), Some(local.as_os_str()), false).unwrap();
        assert_eq!(got, PathBuf::from(r"C:\Users\h\AppData\Local").join("Lumen"));
    }

    #[test]
    fn 无覆盖且无localappdata返回none() {
        assert!(resolve(None, None, false).is_none());
        assert!(resolve(None, None, true).is_none());
    }
}
