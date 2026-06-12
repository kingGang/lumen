//! 文件路径补全逻辑引擎（M4.4 批1）。
//!
//! 本模块只做纯逻辑（token 提取 + 路径枚举），不依赖 egui / PTY。
//! UI 渲染见 [`crate::shell::completion_ui`]，集成入口见 `main.rs`。

use std::path::{Path, PathBuf};

/// 一个补全候选。
pub struct Completion {
    /// 列表展示名（目录条目末尾带 `/`）。
    pub display: String,
    /// 接受后用于替换「当前 token」的完整文本。
    pub replacement: String,
    /// 是否为目录（UI 层可据此选颜色）。
    pub is_dir: bool,
}

/// 从行文本 + 光标字节偏移提取「当前 token」。
///
/// 光标前的最后一个分隔符之后到光标处为「当前 token」。
/// 分隔符定义：空白、`|`、`;`、`&`、`<`、`>`、`(`、`)`、`"`、`'`。
///
/// # Returns
/// `(token 起始字节偏移, token 切片)`。光标前无分隔符时起始为 0。
///
/// # Examples
/// ```
/// use lumen_app::completion::current_token;
/// let (start, tok) = current_token("ls src/ma", 9);
/// assert_eq!(start, 3);
/// assert_eq!(tok, "src/ma");
/// ```
pub fn current_token(line: &str, cursor_byte: usize) -> (usize, &str) {
    // 确保 cursor_byte 不超出行长（防御）。
    let cursor_byte = cursor_byte.min(line.len());
    let prefix = &line[..cursor_byte];

    // 从光标向左扫描，找最近一个分隔符的位置（取字节索引）。
    let separators = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '|' | ';' | '&' | '<' | '>' | '(' | ')' | '"' | '\''
        )
    };

    // rfind 返回最后一个分隔符的字节偏移（该字符本身不属于 token）。
    let token_start = match prefix.rfind(separators) {
        Some(sep_byte) => {
            // sep_byte 是 UTF-8 字符的起始；跳过该字符得到 token 起始。
            let ch = prefix[sep_byte..].chars().next().unwrap_or(' ');
            sep_byte + ch.len_utf8()
        }
        None => 0,
    };

    (token_start, &line[token_start..cursor_byte])
}

/// 文件路径补全：对 `token` 做本地文件系统路径补全。
///
/// 算法：
/// 1. 将 token 按最后一个路径分隔符（`/` 或 `\`）切分为「目录前缀」和「文件名前缀」；
///    若无分隔符，则目录前缀为空、文件名前缀为 token 全文。
/// 2. 目录前缀非空时在 cwd 下解析为候选目录；为空时直接在 cwd 下查找。
/// 3. 读取目录条目，过滤出以文件名前缀**开头**的条目（Windows 大小写不敏感）；
///    隐藏文件（`.` 开头）仅当文件名前缀以 `.` 开头才列出。
/// 4. 目录条目排在前，同类按名称排序。
/// 5. 读目录失败（无此目录、权限不足等）返回空 `Vec`。
///
/// # Arguments
/// * `token` - 当前 token（用于确定目录前缀 + 文件名前缀）。
/// * `cwd`   - 相对路径的基准目录。
///
/// # Returns
/// 补全候选列表；可能为空。
///
/// # Errors
/// 目录读取失败时静默返回空 `Vec`，不向调用方传播 `io::Error`。
pub fn complete_path(token: &str, cwd: &Path) -> Vec<Completion> {
    // ── 1. 切分 token 为「目录部分」+「文件名前缀」 ──────────────────
    // 同时支持 `/` 和 `\`（Windows 路径也常用 `\`）。
    let last_sep = token.rfind(['/', '\\']).map(|i| i + 1); // 包含分隔符本身（replacement 里也需要它）

    let (dir_prefix, name_prefix) = match last_sep {
        Some(sep_end) => (&token[..sep_end], &token[sep_end..]),
        None => ("", token),
    };

    // ── 2. 解析候选目录 ───────────────────────────────────────────────
    let search_dir: PathBuf = if dir_prefix.is_empty() {
        cwd.to_path_buf()
    } else {
        // 绝对路径直接用；相对路径以 cwd 为基准。
        let p = PathBuf::from(dir_prefix);
        if p.is_absolute() {
            p
        } else {
            cwd.join(&p)
        }
    };

    // ── 3. 读目录、过滤、排序 ─────────────────────────────────────────
    let entries = match std::fs::read_dir(&search_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let show_hidden = name_prefix.starts_with('.');

    // 收集符合条件的条目（目录 vs 文件分开，最后合并排序）。
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // 非 UTF-8 文件名跳过
        };

        // 隐藏文件过滤。
        if name.starts_with('.') && !show_hidden {
            continue;
        }

        // 大小写不敏感前缀匹配（Windows 语义）。
        if !name.to_lowercase().starts_with(&name_prefix.to_lowercase()) {
            continue;
        }

        // 判断是否为目录（symlink → 跟随）。
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir() || ft.is_symlink())
            .unwrap_or(false)
            && entry.path().is_dir();

        if is_dir {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }

    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|a| a.to_lowercase());

    // ── 4. 构造 Completion 列表（目录在前）──────────────────────────
    let mut result = Vec::with_capacity(dirs.len() + files.len());

    for name in dirs {
        let display = format!("{name}/");
        let replacement = format!("{dir_prefix}{name}/");
        result.push(Completion {
            display,
            replacement,
            is_dir: true,
        });
    }

    for name in files {
        let display = name.clone();
        let replacement = format!("{dir_prefix}{name}");
        result.push(Completion {
            display,
            replacement,
            is_dir: false,
        });
    }

    result
}

// ─── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── current_token ──────────────────────────────────────────────────────────

    #[test]
    fn token_行首无分隔符() {
        let (start, tok) = current_token("src/main.rs", 11);
        assert_eq!(start, 0);
        assert_eq!(tok, "src/main.rs");
    }

    #[test]
    fn token_空格分隔() {
        let (start, tok) = current_token("ls src/ma", 9);
        assert_eq!(start, 3);
        assert_eq!(tok, "src/ma");
    }

    #[test]
    fn token_管道分隔() {
        let (start, tok) = current_token("cat file|grep", 13);
        assert_eq!(start, 9);
        assert_eq!(tok, "grep");
    }

    #[test]
    fn token_光标在行中间() {
        // cursor_byte=6 → prefix="ls src" → 分隔符在 2，token_start=3
        let (start, tok) = current_token("ls src/main.rs", 6);
        assert_eq!(start, 3);
        assert_eq!(tok, "src");
    }

    #[test]
    fn token_空行() {
        let (start, tok) = current_token("", 0);
        assert_eq!(start, 0);
        assert_eq!(tok, "");
    }

    #[test]
    fn token_cursor_超出行长_夹紧() {
        let line = "abc";
        let (start, tok) = current_token(line, 999);
        assert_eq!(start, 0);
        assert_eq!(tok, "abc");
    }

    // ── complete_path ──────────────────────────────────────────────────────────

    /// 建立隔离的临时目录并返回路径，测试结束后由调用方删除。
    fn make_test_dir(prefix: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("lumen_completion_test_{prefix}"));
        // 幂等：不管上次有没有删干净。
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("无法创建测试目录");
        base
    }

    #[test]
    fn complete_列出同级文件() {
        let dir = make_test_dir("list");
        fs::write(dir.join("alpha.txt"), b"").unwrap();
        fs::write(dir.join("beta.txt"), b"").unwrap();
        fs::create_dir(dir.join("gamma_dir")).unwrap();

        let results = complete_path("", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();

        // 目录在前（gamma_dir/），然后是文件
        assert_eq!(names[0], "gamma_dir/");
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_前缀过滤() {
        let dir = make_test_dir("prefix");
        fs::write(dir.join("main.rs"), b"").unwrap();
        fs::write(dir.join("main_test.rs"), b"").unwrap();
        fs::write(dir.join("lib.rs"), b"").unwrap();

        let results = complete_path("ma", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"main_test.rs"));
        assert!(!names.contains(&"lib.rs"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_目录条目末尾加斜杠() {
        let dir = make_test_dir("slash");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub_file"), b"").unwrap();

        let results = complete_path("sub", &dir);
        let dir_entry = results.iter().find(|c| c.is_dir).expect("应有目录候选");
        assert_eq!(dir_entry.display, "sub/");
        assert!(dir_entry.replacement.ends_with('/'));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_隐藏文件默认不列() {
        let dir = make_test_dir("hidden");
        fs::write(dir.join(".hidden"), b"").unwrap();
        fs::write(dir.join("visible"), b"").unwrap();

        let results = complete_path("", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();
        assert!(!names.contains(&".hidden"));
        assert!(names.contains(&"visible"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_隐藏文件点前缀才列() {
        let dir = make_test_dir("hidden2");
        fs::write(dir.join(".bashrc"), b"").unwrap();
        fs::write(dir.join(".profile"), b"").unwrap();
        fs::write(dir.join("normal"), b"").unwrap();

        let results = complete_path(".", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&".bashrc"));
        assert!(names.contains(&".profile"));
        assert!(!names.contains(&"normal"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_子目录路径() {
        let dir = make_test_dir("subdir");
        let sub = dir.join("src");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("main.rs"), b"").unwrap();
        fs::write(sub.join("lib.rs"), b"").unwrap();

        let results = complete_path("src/ma", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&"lib.rs"));

        // replacement 包含 dir_prefix
        let entry = results.iter().find(|c| c.display == "main.rs").unwrap();
        assert_eq!(entry.replacement, "src/main.rs");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_路径不存在返回空() {
        let dir = std::env::temp_dir().join("lumen_completion_nonexistent_xyz");
        let results = complete_path("", &dir);
        assert!(results.is_empty());
    }

    #[test]
    fn complete_大小写不敏感() {
        let dir = make_test_dir("case");
        fs::write(dir.join("README.md"), b"").unwrap();
        fs::write(dir.join("other.txt"), b"").unwrap();

        // 小写前缀也能匹配大写文件名（Windows 语义）
        let results = complete_path("read", &dir);
        let names: Vec<&str> = results.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"README.md"), "应不区分大小写匹配 README.md");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_目录在前且按名排序() {
        let dir = make_test_dir("order");
        fs::write(dir.join("z_file"), b"").unwrap();
        fs::create_dir(dir.join("a_dir")).unwrap();
        fs::write(dir.join("m_file"), b"").unwrap();
        fs::create_dir(dir.join("b_dir")).unwrap();

        let results = complete_path("", &dir);
        // 前两项应为目录（按字母序），后两项为文件
        assert_eq!(results[0].display, "a_dir/");
        assert_eq!(results[1].display, "b_dir/");
        // 文件区：m_file < z_file
        let file_names: Vec<&str> = results[2..].iter().map(|c| c.display.as_str()).collect();
        assert_eq!(file_names, vec!["m_file", "z_file"]);

        let _ = fs::remove_dir_all(&dir);
    }
}
