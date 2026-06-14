//! 应用数据目录解析（单一真源）。
//!
//! 所有持久化文件——`settings.json` / `profile.json` / `sessions.json` /
//! `history.jsonl`——都落在同一个应用数据目录下。该目录**按构建类型
//! 隔离**：
//!
//! - release 构建（正式安装版）：`%LOCALAPPDATA%/Lumen/`
//! - debug 构建（开发 / 测试）：`%LOCALAPPDATA%/Lumen-dev/`
//!
//! 这样开发期 `cargo run`（debug）读写独立目录，绝不污染正式安装版
//! 的真实配置 / 登录态 / 会话 / 命令历史（海风哥拍板：debug 的所有
//! 数据都跟正式版隔离）。与 [`crate::single_instance`] 的「debug 默认
//! 放开多开」是同一套「debug 自动隔离」约定。
//!
//! 逃生口：设环境变量 `LUMEN_DATA_DIR=<绝对路径>` 可把数据目录覆盖到
//! 任意位置（便于 debug 下临时调试正式数据，或自定义便携部署）。覆盖
//! 值原样使用，**不再**追加 `Lumen` 子目录。

use std::ffi::OsStr;
use std::path::PathBuf;

/// release 构建的应用数据子目录名（正式安装版）。
const DIR_RELEASE: &str = "Lumen";
/// debug 构建的应用数据子目录名（开发 / 测试，与正式版隔离）。
const DIR_DEBUG: &str = "Lumen-dev";

/// 解析逻辑（纯函数，便于单测注入环境）：优先 `LUMEN_DATA_DIR` 覆盖
/// 路径（非空时原样使用）；否则取 `LOCALAPPDATA` 下按构建类型选择的
/// 子目录。两者都缺时返回 `None`。
fn resolve(custom: Option<&OsStr>, localappdata: Option<&OsStr>, debug: bool) -> Option<PathBuf> {
    if let Some(c) = custom {
        if !c.is_empty() {
            return Some(PathBuf::from(c));
        }
    }
    let sub = if debug { DIR_DEBUG } else { DIR_RELEASE };
    localappdata.map(|d| PathBuf::from(d).join(sub))
}

/// 应用数据目录：`%LOCALAPPDATA%/<Lumen|Lumen-dev>/`（按构建类型隔离），
/// 或 `LUMEN_DATA_DIR` 指定的覆盖路径。
///
/// 返回 `None` 表示既无 `LUMEN_DATA_DIR` 覆盖、`LOCALAPPDATA` 也未设置
/// （极端定制环境）——调用方据此降级为「本次运行不持久化」。
pub fn data_dir() -> Option<PathBuf> {
    resolve(
        std::env::var_os("LUMEN_DATA_DIR").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
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
