//! F10 终端可点击链接：从终端行文本识别 URL / 文件路径，并用系统默认
//! 程序打开。
//!
//! 分层：
//! - [`detect_link`]：**纯函数**，无 I/O——给定一行字符与光标所在字符
//!   下标，识别覆盖它的链接区段（URL 或文件路径候选）。可单测。
//! - [`resolve`]：把路径候选按当前工作目录解析为绝对路径并校验存在
//!   （文件系统 I/O，结果决定「裸路径是否算链接」）。
//! - [`open`]：用系统默认程序/浏览器打开（进程启动，后台回收句柄）。
//!
//! 范围（需求池 F10）：URL（http/https/file）、文件路径（含
//! `:行:列`，如 `src/main.rs:10:5`）、以及 lumen-term 采集的 OSC 8
//! 显式超链接（OSC 8 的区段与 URI 由终端侧直接给出，不经本模块的
//! [`detect_link`]，只复用 [`open`]）。

use std::path::{Path, PathBuf};

/// 行文本里识别出的原始链接（尚未做文件存在性校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawLink {
    /// 带 scheme 的网址（http/https/file）。
    Url(String),
    /// 文件路径候选 + 可选行列号（需 [`resolve`] 解析 cwd 并校验存在）。
    Path {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// 已解析、可打开的链接目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// 网址或 OSC 8 URI——交系统默认处理器（浏览器等）。
    Url(String),
    /// 本地文件（已校验存在）+ 可选行列号。
    File {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// 支持的 URL scheme（按长度降序，避免 `http` 抢先匹配 `https`）。
const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// 识别覆盖 `idx`（字符下标）的链接区段。
///
/// 返回 `(start, end, link)`：`start..end` 为**字符**下标区间（左闭右
/// 开），调用方据此映射回显示列做高亮。先试 URL，再试文件路径候选。
pub fn detect_link(chars: &[char], idx: usize) -> Option<(usize, usize, RawLink)> {
    if idx >= chars.len() {
        return None;
    }
    if let Some(hit) = detect_url(chars, idx) {
        return Some(hit);
    }
    detect_path(chars, idx)
}

/// 扫描所有 scheme 出现位置，返回覆盖 `idx` 的 URL 区段。
fn detect_url(chars: &[char], idx: usize) -> Option<(usize, usize, RawLink)> {
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if let Some(scheme_len) = scheme_at(chars, i) {
            let mut end = i + scheme_len;
            while end < n && is_url_char(chars[end]) {
                end += 1;
            }
            end = trim_url_end(chars, i, end);
            // scheme 之后至少要有一个主机字符，且区段须覆盖 idx。
            if end > i + scheme_len && idx >= i && idx < end {
                let url: String = chars[i..end].iter().collect();
                return Some((i, end, RawLink::Url(url)));
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// `chars[i..]` 是否以某个 scheme 开头，是则返回该 scheme 的字符数。
fn scheme_at(chars: &[char], i: usize) -> Option<usize> {
    for scheme in SCHEMES {
        let sc: Vec<char> = scheme.chars().collect();
        if i + sc.len() <= chars.len() && chars[i..i + sc.len()] == sc[..] {
            return Some(sc.len());
        }
    }
    None
}

/// URL 允许的字符：排除空白、控制符与一组在 URL 里非法/易致歧义的字符。
fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !"<>\"{}|\\^`'".contains(c)
}

/// 去掉 URL 尾部的句末标点与**未配对**的右括号（中文/英文行尾常见，
/// 如 `(见 https://a.com)` 的末尾 `)`，但 `…/wiki(x)` 内配对的保留）。
fn trim_url_end(chars: &[char], start: usize, mut end: usize) -> usize {
    let count = |open: char, close: char, hi: usize| -> (usize, usize) {
        let mut o = 0;
        let mut c = 0;
        for &ch in &chars[start..hi] {
            if ch == open {
                o += 1;
            } else if ch == close {
                c += 1;
            }
        }
        (o, c)
    };
    while end > start {
        let ch = chars[end - 1];
        let strip = match ch {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => {
                let (o, c) = count('(', ')', end);
                c > o
            }
            ']' => {
                let (o, c) = count('[', ']', end);
                c > o
            }
            '}' => {
                let (o, c) = count('{', '}', end);
                c > o
            }
            _ => false,
        };
        if strip {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

/// 以空白/引号/括号为边界取 `idx` 所在的「词」，识别为文件路径候选。
fn detect_path(chars: &[char], idx: usize) -> Option<(usize, usize, RawLink)> {
    let is_boundary = |c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '"' | '\'' | '`' | '(' | ')' | '<' | '>' | '|')
    };
    // 光标本身落在边界（空格/引号等）上：此处没有词，不算链接。
    if is_boundary(chars[idx]) {
        return None;
    }
    let mut start = idx;
    while start > 0 && !is_boundary(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && !is_boundary(chars[end]) {
        end += 1;
    }
    // 去掉尾部句末标点（不含 `:`——交给行列号解析）。
    while end > start && matches!(chars[end - 1], '.' | ',' | ';' | '!' | '?') {
        end -= 1;
    }
    if start >= end || idx >= end {
        return None;
    }
    let token: String = chars[start..end].iter().collect();
    let (path, line, col) = split_line_col(&token);
    if path.is_empty() || !looks_like_path(&path) {
        return None;
    }
    Some((start, end, RawLink::Path { path, line, col }))
}

/// 从词尾剥离 `:行` 或 `:行:列`（从末尾向前最多剥两段纯数字），其余
/// 作为路径。Windows 盘符 `C:` 不会被误剥（盘符后非纯数字）。
fn split_line_col(s: &str) -> (String, Option<u32>, Option<u32>) {
    // 从词尾剥一段 `:数字`（盘符 `C:` 后非数字，天然不被剥）。
    fn peel(t: &str) -> Option<(&str, u32)> {
        let idx = t.rfind(':')?;
        let num = &t[idx + 1..];
        if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
            num.parse::<u32>().ok().map(|n| (&t[..idx], n))
        } else {
            None
        }
    }
    if let Some((rest1, n1)) = peel(s) {
        if let Some((rest2, n2)) = peel(rest1) {
            // 两段数字：rest1 尾段是 line、s 尾段是 col。
            return (rest2.to_string(), Some(n2), Some(n1));
        }
        // 一段数字：作为 line。
        return (rest1.to_string(), Some(n1), None);
    }
    (s.to_string(), None, None)
}

/// 是否「像」一个文件路径：含路径分隔符、盘符前缀、或带文件扩展名。
/// 此过滤把「裸单词」挡在外面，避免 hover 每个词都触发文件系统校验。
fn looks_like_path(s: &str) -> bool {
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true; // 盘符 C:
    }
    has_extension(s)
}

/// 末段是否带 `.扩展名`（1~8 个字母数字，且点不在段首）。
fn has_extension(s: &str) -> bool {
    let last = s.rsplit(['/', '\\']).next().unwrap_or(s);
    if let Some(dot) = last.rfind('.') {
        let ext = &last[dot + 1..];
        dot > 0 && !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    } else {
        false
    }
}

/// 把原始链接解析为可打开目标：URL 直通；路径按 cwd 解析为绝对路径
/// 并校验存在（不存在则不算链接，返回 None）。
pub fn resolve(raw: &RawLink, cwd: Option<&Path>) -> Option<LinkTarget> {
    match raw {
        RawLink::Url(u) => Some(LinkTarget::Url(u.clone())),
        RawLink::Path { path, line, col } => {
            let p = Path::new(path);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd?.join(p)
            };
            if abs.exists() {
                Some(LinkTarget::File {
                    path: abs,
                    line: *line,
                    col: *col,
                })
            } else {
                None
            }
        }
    }
}

/// 用系统默认程序/浏览器打开链接（进程启动，后台回收子进程句柄）。
pub fn open(target: &LinkTarget) {
    match target {
        LinkTarget::Url(u) => open_url(u),
        LinkTarget::File { path, line, col } => open_file(path, *line, *col),
    }
}

/// 打开 URL（系统默认浏览器）。
///
/// Windows 走 `explorer.exe <url>`：内部转 ShellExecute，URL 含
/// `&`/`?`/`=` 等查询串字符也安全（不经 cmd 二次解析），与文件树
/// `open_with_default` 同款规避策略。
fn open_url(url: &str) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe").arg(url).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    match result {
        Ok(child) => reap_in_background(child),
        Err(e) => log::error!("打开 URL 失败 {url}: {e}"),
    }
}

/// 打开文件：带行号且检测到 VS Code 时用 `code -g 定位到行列`，否则用
/// 系统默认程序打开（不定位）。
fn open_file(path: &Path, line: Option<u32>, col: Option<u32>) {
    #[cfg(windows)]
    {
        if let Some(line) = line {
            if vscode_available() {
                let goto = match col {
                    Some(c) => format!("{}:{line}:{c}", path.display()),
                    None => format!("{}:{line}", path.display()),
                };
                use std::os::windows::process::CommandExt;
                // code 是 .cmd 批处理 shim，须经 cmd 调起（Command::new 直
                // 接找无扩展名/批处理会失败）；CREATE_NO_WINDOW 避免 cmd
                // 窗口闪现。源码路径几乎不含 cmd 元字符，可接受。
                let child = std::process::Command::new("cmd")
                    .args(["/c", "code", "-g", &goto])
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                match child {
                    Ok(c) => {
                        reap_in_background(c);
                        return;
                    }
                    Err(e) => log::warn!("VS Code 打开失败，回退默认程序: {e}"),
                }
            }
        }
    }
    open_path_default(path);
}

/// 用系统默认程序打开文件（不定位行列）。
fn open_path_default(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    match result {
        Ok(child) => reap_in_background(child),
        Err(e) => log::error!("打开文件失败 {}: {e}", path.display()),
    }
}

/// Windows 上 `CREATE_NO_WINDOW` 进程创建标志（隐藏子进程控制台）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// VS Code CLI（`code`）是否可用（首次调用探测一次后缓存）。
#[cfg(windows)]
fn vscode_available() -> bool {
    use std::sync::OnceLock;
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(|| {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .args(["/c", "where", "code"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// 后台线程回收已 spawn 的子进程句柄（防僵尸句柄堆积）。
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn detect(s: &str, idx: usize) -> Option<(usize, usize, RawLink)> {
        detect_link(&chars(s), idx)
    }

    #[test]
    fn url_基础识别() {
        let s = "see https://example.com/path now";
        let (start, end, link) = detect(s, 10).expect("应识别 URL");
        assert_eq!(&s[start..end], "https://example.com/path");
        assert_eq!(link, RawLink::Url("https://example.com/path".into()));
    }

    #[test]
    fn url_去尾部标点与未配对括号() {
        // 句末句号不属于 URL，区段（ASCII 下字符下标==字节下标）不含它。
        let full = "go to https://a.com/x.";
        let (s, e, link) = detect(full, 8).unwrap();
        assert_eq!(link, RawLink::Url("https://a.com/x".into()));
        assert_eq!(&full[s..e], "https://a.com/x");
        // 未配对右括号剥掉，配对的保留。
        let (_, _, link2) = detect("(see https://a.com/wiki(x))", 8).unwrap();
        assert_eq!(link2, RawLink::Url("https://a.com/wiki(x)".into()));
    }

    #[test]
    fn url_含查询串() {
        let (_, _, link) = detect("https://a.com/s?q=1&p=2", 3).unwrap();
        assert_eq!(link, RawLink::Url("https://a.com/s?q=1&p=2".into()));
    }

    #[test]
    fn 裸单词不算路径() {
        // 无分隔符/扩展名/盘符的词不触发路径识别。
        assert!(detect("hello world", 2).is_none());
    }

    #[test]
    fn 相对路径带扩展名() {
        let (_, _, link) = detect("edit src/main.rs please", 6).unwrap();
        assert_eq!(
            link,
            RawLink::Path {
                path: "src/main.rs".into(),
                line: None,
                col: None
            }
        );
    }

    #[test]
    fn 路径带行列号() {
        let (s, e, link) = detect("at src/main.rs:10:5 fail", 5).unwrap();
        assert_eq!(
            link,
            RawLink::Path {
                path: "src/main.rs".into(),
                line: Some(10),
                col: Some(5)
            }
        );
        // 区段覆盖整段含行列号。
        let txt = "at src/main.rs:10:5 fail";
        assert_eq!(&txt[s..e], "src/main.rs:10:5");
    }

    #[test]
    fn 路径仅行号() {
        let (_, _, link) = detect("src/lib.rs:42", 3).unwrap();
        assert_eq!(
            link,
            RawLink::Path {
                path: "src/lib.rs".into(),
                line: Some(42),
                col: None
            }
        );
    }

    #[test]
    fn windows_盘符路径不被行列号误剥() {
        let (_, _, link) = detect(r"C:\proj\a.rs:7", 3).unwrap();
        assert_eq!(
            link,
            RawLink::Path {
                path: r"C:\proj\a.rs".into(),
                line: Some(7),
                col: None
            }
        );
        // 裸盘符目录（无行列号）。
        let (_, _, link2) = detect(r"C:\Users\x", 4).unwrap();
        assert_eq!(
            link2,
            RawLink::Path {
                path: r"C:\Users\x".into(),
                line: None,
                col: None
            }
        );
    }

    #[test]
    fn 文件名带扩展名无目录() {
        let (_, _, link) = detect("open Cargo.toml", 7).unwrap();
        assert_eq!(
            link,
            RawLink::Path {
                path: "Cargo.toml".into(),
                line: None,
                col: None
            }
        );
    }

    #[test]
    fn split_line_col_各形态() {
        assert_eq!(split_line_col("a.rs"), ("a.rs".into(), None, None));
        assert_eq!(split_line_col("a.rs:1"), ("a.rs".into(), Some(1), None));
        assert_eq!(split_line_col("a.rs:1:2"), ("a.rs".into(), Some(1), Some(2)));
        assert_eq!(split_line_col("C:\\a"), ("C:\\a".into(), None, None));
    }

    #[test]
    fn 光标在区段外返回none() {
        // idx 落在 URL 之前的空格上。
        assert!(detect("x https://a.com", 1).is_none());
    }

    #[test]
    fn resolve_url直通() {
        let t = resolve(&RawLink::Url("https://a.com".into()), None).unwrap();
        assert_eq!(t, LinkTarget::Url("https://a.com".into()));
    }

    #[test]
    fn resolve_不存在的路径返回none() {
        let raw = RawLink::Path {
            path: "definitely_not_here_xyz.rs".into(),
            line: None,
            col: None,
        };
        assert!(resolve(&raw, Some(Path::new(r"C:\nonexistent_dir_zzz"))).is_none());
    }
}
