//! PowerShell 轻量语法高亮 tokenizer 与续行检测（设计稿 §8 / §5）。
//!
//! 本模块是纯函数 + 跨行状态机，零 winit/wgpu/pty 依赖，100% 可单测。
//! 一套 lexer 同时服务两个用途（设计稿 §8「needs_continuation() 复用同一 tokenizer」）：
//!
//! 1. **语法高亮**：[`highlight_document`] 逐行产出 [`Token`] 区间（行内字节偏移 +
//!    语义类别 [`TokenKind`]），渲染层把类别映射到主题色板——本 crate 不关心具体颜色。
//! 2. **续行检测**：[`needs_continuation`] 判定文档末尾是否处于未闭合状态
//!    （块注释 / here-string / 跨行字符串 / 未闭合括号 / 行尾管道或续行反引号），
//!    供 app 层决定 Enter 是「自动换行续行」还是「提交命令」（设计稿 §4）。
//!
//! # 词法范围（务实，非 PowerShell 解析器级完备）
//!
//! 覆盖设计稿 §8 要求的类别：命令名 / 参数 `-Foo` / 字符串（含未闭合）/ 变量 `$x` /
//! 数字 / 管道与操作符 / 注释 / 关键字。`Highlighter` 语义日后可换 tree-sitter，
//! 此实现优先「保守正确、永不 panic」而非边角语法全覆盖。
//!
//! # 已知近似（文档化）
//!
//! - 续行反引号连接的下一行，其首词仍按「命令位置」着色（实际是参数），
//!   属轻微着色近似，不影响语义。
//! - `1..10` 区间、`/path` 起始的裸路径等边角写法着色可能不精确，但绝不 panic。

use serde::{Deserialize, Serialize};

/// token 语义类别。渲染层据此映射主题色板；本 crate 不涉及颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenKind {
    /// 命令名（语句起始位置的裸词，如 `Get-ChildItem`、`git`）。
    Command,
    /// 语言关键字（`if`/`foreach`/`function`/`return` 等）。
    Keyword,
    /// 参数开关（`-Recurse`、`-Path`）。
    Parameter,
    /// 变量（`$x`、`$env:PATH`、`${complex}`、`$_`）。
    Variable,
    /// 数字字面量（`123`、`0xFF`、`1.5`、`1gb`）。
    Number,
    /// 字符串字面量（双/单引号、here-string；含未闭合延续）。
    StringLit,
    /// 操作符与标点（`|`、`>`、`;`、`=`、`-eq` 等比较操作符、括号）。
    Operator,
    /// 注释（`# 行注释` 与 `<# 块注释 #>`）。
    Comment,
    /// 普通裸词参数 / 未归类文本。
    Text,
}

/// 一个 token：行内字节区间 `[start, end)` + 语义类别。
///
/// `start`/`end` 是**行内**字节偏移（与 [`crate::cursor::Position::byte`] 同单位），
/// 始终落在合法 UTF-8 字符边界上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    /// 起始字节偏移（行内，含）。
    pub start: usize,
    /// 结束字节偏移（行内，不含）。
    pub end: usize,
    /// 语义类别。
    pub kind: TokenKind,
}

/// 进入某一物理行时的词法延续状态（跨行结构由上一行带入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexState {
    /// 普通状态（行起始默认）。
    Normal,
    /// 处于块注释 `<# ... #>` 中。
    BlockComment,
    /// 处于未闭合的双引号字符串中（交互/多行续行场景）。
    DoubleString,
    /// 处于未闭合的单引号字符串中。
    SingleString,
    /// 处于双引号 here-string `@" ... "@` 中。
    HereDouble,
    /// 处于单引号 here-string `@' ... '@` 中。
    HereSingle,
}

/// 跨行 lexer 状态机。逐行喂入 [`Lexer::lex_line`]，扫完后读字段判定续行。
struct Lexer {
    /// 当前词法延续状态。
    state: LexState,
    /// 括号深度（`(` `{` `[` 累计减 `)` `}` `]`，钳制不为负）。
    paren_depth: i32,
    /// 最后一个「显著」token（非空白、非注释）是否为管道 `|`。
    last_is_pipe: bool,
    /// 最近处理的物理行是否以续行反引号结尾（` 后仅余空白）。
    ends_with_backtick_cont: bool,
}

impl Lexer {
    fn new() -> Self {
        Self {
            state: LexState::Normal,
            paren_depth: 0,
            last_is_pipe: false,
            ends_with_backtick_cont: false,
        }
    }

    /// 文档末尾是否需要续行。
    fn needs_continuation(&self) -> bool {
        self.state != LexState::Normal
            || self.paren_depth > 0
            || self.last_is_pipe
            || self.ends_with_backtick_cont
    }

    /// 词法分析一物理行，更新自身跨行状态，返回该行的 token 列表。
    fn lex_line(&mut self, line: &str) -> Vec<Token> {
        // 本行起始重置「续行反引号」标志（仅最后处理行的取值参与续行判定）。
        self.ends_with_backtick_cont = false;

        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let len = line.len();
        let mut tokens = Vec::new();

        // ── 先消费上一行带入的跨行结构 ──────────────────────────────────────
        let mut i = match self.state {
            LexState::Normal => 0,
            LexState::BlockComment => {
                match find_seq(&chars, 0, '#', '>') {
                    Some(idx) => {
                        let end = byte_at(&chars, idx + 2, len);
                        tokens.push(Token {
                            start: 0,
                            end,
                            kind: TokenKind::Comment,
                        });
                        self.state = LexState::Normal;
                        idx + 2
                    }
                    None => {
                        // 整行仍在块注释内。
                        if len > 0 {
                            tokens.push(Token {
                                start: 0,
                                end: len,
                                kind: TokenKind::Comment,
                            });
                        }
                        return tokens;
                    }
                }
            }
            LexState::DoubleString | LexState::SingleString => {
                let double = self.state == LexState::DoubleString;
                match scan_string_body(&chars, 0, double) {
                    Some(close) => {
                        let end = byte_at(&chars, close + 1, len);
                        tokens.push(Token {
                            start: 0,
                            end,
                            kind: TokenKind::StringLit,
                        });
                        self.state = LexState::Normal;
                        close + 1
                    }
                    None => {
                        if len > 0 {
                            tokens.push(Token {
                                start: 0,
                                end: len,
                                kind: TokenKind::StringLit,
                            });
                        }
                        return tokens;
                    }
                }
            }
            LexState::HereDouble | LexState::HereSingle => {
                let marker = if self.state == LexState::HereDouble {
                    "\"@"
                } else {
                    "'@"
                };
                if line.trim_start().starts_with(marker) {
                    // 终止符行：整行作为字符串收尾，回到 Normal。
                    if len > 0 {
                        tokens.push(Token {
                            start: 0,
                            end: len,
                            kind: TokenKind::StringLit,
                        });
                    }
                    self.state = LexState::Normal;
                } else if len > 0 {
                    tokens.push(Token {
                        start: 0,
                        end: len,
                        kind: TokenKind::StringLit,
                    });
                }
                return tokens;
            }
        };

        // ── Normal 状态主扫描 ────────────────────────────────────────────────
        let n = chars.len();
        // 命令位置标志：语句起始的裸词着色为命令名。每行起始视作新语句。
        let mut expect_command = true;

        while i < n {
            let (bs, c) = chars[i];

            if c.is_whitespace() {
                i += 1;
                continue;
            }

            // 行注释：`#` 作为 token 起始字符（`C#sharp` 中部的 `#` 不会进此分支）。
            if c == '#' {
                tokens.push(Token {
                    start: bs,
                    end: len,
                    kind: TokenKind::Comment,
                });
                break;
            }

            // 块注释起始 `<#`。
            if c == '<' && peek_char(&chars, i + 1) == Some('#') {
                match find_seq(&chars, i + 2, '#', '>') {
                    Some(idx) => {
                        let end = byte_at(&chars, idx + 2, len);
                        tokens.push(Token {
                            start: bs,
                            end,
                            kind: TokenKind::Comment,
                        });
                        i = idx + 2;
                    }
                    None => {
                        tokens.push(Token {
                            start: bs,
                            end: len,
                            kind: TokenKind::Comment,
                        });
                        self.state = LexState::BlockComment;
                        break;
                    }
                }
                continue;
            }

            // here-string 起始 `@"` / `@'`（其后须仅余空白到行尾）。
            if c == '@' {
                if let Some(q) = peek_char(&chars, i + 1) {
                    if (q == '"' || q == '\'') && rest_blank(&chars, i + 2) {
                        tokens.push(Token {
                            start: bs,
                            end: len,
                            kind: TokenKind::StringLit,
                        });
                        self.state = if q == '"' {
                            LexState::HereDouble
                        } else {
                            LexState::HereSingle
                        };
                        break;
                    }
                }
            }

            // 字符串。
            if c == '"' || c == '\'' {
                let double = c == '"';
                match scan_string_body(&chars, i + 1, double) {
                    Some(close) => {
                        let end = byte_at(&chars, close + 1, len);
                        tokens.push(Token {
                            start: bs,
                            end,
                            kind: TokenKind::StringLit,
                        });
                        i = close + 1;
                    }
                    None => {
                        tokens.push(Token {
                            start: bs,
                            end: len,
                            kind: TokenKind::StringLit,
                        });
                        self.state = if double {
                            LexState::DoubleString
                        } else {
                            LexState::SingleString
                        };
                        break;
                    }
                }
                self.last_is_pipe = false;
                expect_command = false;
                continue;
            }

            // 变量 `$...`。
            if c == '$' {
                let end_idx = scan_variable(&chars, i);
                let end = byte_at(&chars, end_idx, len);
                tokens.push(Token {
                    start: bs,
                    end,
                    kind: TokenKind::Variable,
                });
                self.last_is_pipe = false;
                expect_command = false;
                i = end_idx;
                continue;
            }

            // 数字。
            if c.is_ascii_digit() {
                let end_idx = scan_number(&chars, i);
                let end = byte_at(&chars, end_idx, len);
                tokens.push(Token {
                    start: bs,
                    end,
                    kind: TokenKind::Number,
                });
                self.last_is_pipe = false;
                expect_command = false;
                i = end_idx;
                continue;
            }

            // 续行反引号 / 转义反引号。
            if c == '`' {
                if rest_blank(&chars, i + 1) {
                    // 行尾续行符。
                    tokens.push(Token {
                        start: bs,
                        end: len,
                        kind: TokenKind::Operator,
                    });
                    self.ends_with_backtick_cont = true;
                    break;
                }
                // 行中反引号：作转义符，单独成 Operator（不参与命令位置）。
                let end = byte_at(&chars, i + 1, len);
                tokens.push(Token {
                    start: bs,
                    end,
                    kind: TokenKind::Operator,
                });
                i += 1;
                continue;
            }

            // `-` 起始：参数 / 比较操作符 / 减号。
            if c == '-' {
                if matches!(peek_char(&chars, i + 1), Some(ch) if ch.is_ascii_alphabetic()) {
                    let end_idx = scan_ascii_word(&chars, i + 1);
                    let word = slice_text(line, &chars, i + 1, end_idx, len);
                    let kind = if is_operator_word(&word) {
                        TokenKind::Operator
                    } else {
                        TokenKind::Parameter
                    };
                    let end = byte_at(&chars, end_idx, len);
                    tokens.push(Token {
                        start: bs,
                        end,
                        kind,
                    });
                    self.last_is_pipe = false;
                    expect_command = false;
                    i = end_idx;
                    continue;
                }
                // 裸 `-`：减号操作符。
                let end = byte_at(&chars, i + 1, len);
                tokens.push(Token {
                    start: bs,
                    end,
                    kind: TokenKind::Operator,
                });
                self.last_is_pipe = false;
                i += 1;
                continue;
            }

            // 操作符 / 标点。
            if is_op_char(c) {
                let op = classify_op(&chars, i);
                let end = byte_at(&chars, i + op.len, len);
                tokens.push(Token {
                    start: bs,
                    end,
                    kind: TokenKind::Operator,
                });
                self.paren_depth = (self.paren_depth + op.paren_delta).max(0);
                self.last_is_pipe = op.is_pipe;
                if op.opens_command {
                    expect_command = true;
                }
                i += op.len;
                continue;
            }

            // 裸词：命令名 / 关键字 / 普通文本。
            let end_idx = scan_bareword(&chars, i);
            let end = byte_at(&chars, end_idx, len);
            let word = slice_text(line, &chars, i, end_idx, len);
            let kind = if is_keyword(&word) {
                TokenKind::Keyword
            } else if expect_command {
                TokenKind::Command
            } else {
                TokenKind::Text
            };
            tokens.push(Token {
                start: bs,
                end,
                kind,
            });
            self.last_is_pipe = false;
            // 关键字后仍处命令位置（如 `if (...) { cmd }`）；命令/文本消耗命令位置。
            if kind != TokenKind::Keyword {
                expect_command = false;
            }
            i = end_idx.max(i + 1);
        }

        tokens
    }
}

// ─── 公开 API ──────────────────────────────────────────────────────────────────

/// 逐行对整个文档做语法高亮，返回每行的 token 列表（跨行结构正确延续）。
///
/// 输出 `result[row]` 对应 `lines[row]` 的 token；token 区间不重叠、按起始递增。
/// 行内的空白不产出 token（渲染层按默认色绘制空隙）。
pub fn highlight_document(lines: &[&str]) -> Vec<Vec<Token>> {
    let mut lexer = Lexer::new();
    lines.iter().map(|line| lexer.lex_line(line)).collect()
}

/// 文档末尾是否处于未闭合状态、需要续行（Enter 应自动换行而非提交）。
///
/// 触发续行的情形：块注释 / here-string / 跨行字符串未闭合、括号未闭合、
/// 最后一个显著 token 是管道 `|`、或末行以续行反引号结尾。
pub fn needs_continuation(lines: &[&str]) -> bool {
    let mut lexer = Lexer::new();
    for line in lines {
        lexer.lex_line(line);
    }
    lexer.needs_continuation()
}

// ─── 词法扫描辅助（纯函数）──────────────────────────────────────────────────────

/// 取 `chars[idx]` 的字节偏移；`idx` 到达/越过末尾时返回行长度 `len`。
fn byte_at(chars: &[(usize, char)], idx: usize, len: usize) -> usize {
    chars.get(idx).map(|&(b, _)| b).unwrap_or(len)
}

/// 窥视 `chars[idx]` 的字符（越界返回 `None`）。
fn peek_char(chars: &[(usize, char)], idx: usize) -> Option<char> {
    chars.get(idx).map(|&(_, c)| c)
}

/// `chars[from..]` 是否全为空白（或为空）。
fn rest_blank(chars: &[(usize, char)], from: usize) -> bool {
    chars[from.min(chars.len())..]
        .iter()
        .all(|&(_, c)| c.is_whitespace())
}

/// 从 `from` 起查找连续两字符 `a` `b`，返回 `a` 的字符索引；未找到返回 `None`。
fn find_seq(chars: &[(usize, char)], from: usize, a: char, b: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i].1 == a && chars[i + 1].1 == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 取 `chars[start..end]` 对应的行内子串（小写化前的原文）。
fn slice_text(line: &str, chars: &[(usize, char)], start: usize, end: usize, len: usize) -> String {
    let s = byte_at(chars, start, len);
    let e = byte_at(chars, end, len);
    line.get(s..e).unwrap_or("").to_string()
}

/// 扫描字符串体（不含起始引号），返回闭合引号的字符索引；行内未闭合返回 `None`。
///
/// - 双引号：反引号 `` ` `` 转义后一字符；`""` 表示一个字面引号（转义）。
/// - 单引号：`''` 表示一个字面单引号（转义）；无反引号转义。
fn scan_string_body(chars: &[(usize, char)], from: usize, double: bool) -> Option<usize> {
    let quote = if double { '"' } else { '\'' };
    let mut i = from;
    while i < chars.len() {
        let c = chars[i].1;
        if double && c == '`' {
            // 反引号转义下一字符。
            i += 2;
            continue;
        }
        if c == quote {
            if peek_char(chars, i + 1) == Some(quote) {
                // 双写引号 = 转义，跳过两个。
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 扫描变量 `$...`，返回变量结束的字符索引（不含）。
fn scan_variable(chars: &[(usize, char)], start: usize) -> usize {
    // start 指向 `$`。
    let j = start + 1;
    match peek_char(chars, j) {
        // `${ ... }` 复杂变量名。
        Some('{') => {
            let mut k = j + 1;
            while k < chars.len() && chars[k].1 != '}' {
                k += 1;
            }
            // 含闭合 `}`（若存在）。
            (k + 1).min(chars.len())
        }
        // 特殊单字符变量 `$_` `$?` `$^`。
        Some('?') | Some('^') => j + 1,
        // 普通变量名 `[A-Za-z0-9_:]+`（含 `$env:PATH` 的作用域冒号）。
        Some(ch) if ch.is_alphanumeric() || ch == '_' => {
            let mut k = j;
            while k < chars.len() {
                let cc = chars[k].1;
                if cc.is_alphanumeric() || cc == '_' || cc == ':' {
                    k += 1;
                } else {
                    break;
                }
            }
            k
        }
        // `$(` 子表达式或孤立 `$`：变量 token 仅含 `$`。
        _ => j,
    }
}

/// 扫描数字字面量，返回结束字符索引（不含）。务实地吞并进制/小数/单位后缀。
fn scan_number(chars: &[(usize, char)], start: usize) -> usize {
    let mut k = start;
    while k < chars.len() {
        let c = chars[k].1;
        if c.is_ascii_alphanumeric() || c == '.' {
            k += 1;
        } else {
            break;
        }
    }
    k
}

/// 扫描 ASCII 字母数字串（用于 `-参数` 名 / 操作符名），返回结束字符索引（不含）。
fn scan_ascii_word(chars: &[(usize, char)], start: usize) -> usize {
    let mut k = start;
    while k < chars.len() && chars[k].1.is_ascii_alphanumeric() {
        k += 1;
    }
    k
}

/// 是否为裸词字符（命令名 / 路径 / 文本）。`-` 允许在词中部（`Get-ChildItem`）。
fn is_bareword_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '#' | ':' | '\\' | '/')
}

/// 扫描裸词，返回结束字符索引（不含，至少前进 1）。
fn scan_bareword(chars: &[(usize, char)], start: usize) -> usize {
    let mut k = start;
    while k < chars.len() && is_bareword_char(chars[k].1) {
        k += 1;
    }
    k.max(start + 1)
}

/// 是否为操作符/标点起始字符。注意 `.` 不在此列（归裸词，覆盖 `./script`、成员访问）。
fn is_op_char(c: char) -> bool {
    matches!(
        c,
        '|' | ';'
            | '&'
            | '='
            | '+'
            | '*'
            | '/'
            | '%'
            | '<'
            | '>'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | ','
            | '!'
    )
}

/// 操作符分类结果。
struct OpInfo {
    /// 占用的字符数（1 或 2）。
    len: usize,
    /// 括号深度增量（`(`/`{`/`[` 为 +1，`)`/`}`/`]` 为 -1，余 0）。
    paren_delta: i32,
    /// 其后是否回到命令位置（`|`/`;`/`&`/`{`/`(`/`=` 等之后）。
    opens_command: bool,
    /// 是否为单管道 `|`（续行判定用）。
    is_pipe: bool,
}

/// 分类 `chars[i]` 起始的操作符，识别 `>>` `&&` `||` 等双字符形态。
fn classify_op(chars: &[(usize, char)], i: usize) -> OpInfo {
    let c = chars[i].1;
    let next = peek_char(chars, i + 1);
    match c {
        '|' => {
            if next == Some('|') {
                OpInfo {
                    len: 2,
                    paren_delta: 0,
                    opens_command: true,
                    is_pipe: false,
                }
            } else {
                OpInfo {
                    len: 1,
                    paren_delta: 0,
                    opens_command: true,
                    is_pipe: true,
                }
            }
        }
        '&' => {
            let len = if next == Some('&') { 2 } else { 1 };
            OpInfo {
                len,
                paren_delta: 0,
                opens_command: true,
                is_pipe: false,
            }
        }
        ';' => OpInfo {
            len: 1,
            paren_delta: 0,
            opens_command: true,
            is_pipe: false,
        },
        '=' => OpInfo {
            len: 1,
            paren_delta: 0,
            opens_command: true,
            is_pipe: false,
        },
        '(' | '{' => OpInfo {
            len: 1,
            paren_delta: 1,
            opens_command: true,
            is_pipe: false,
        },
        '[' => OpInfo {
            len: 1,
            paren_delta: 1,
            opens_command: false,
            is_pipe: false,
        },
        ')' | '}' | ']' => OpInfo {
            len: 1,
            paren_delta: -1,
            opens_command: false,
            is_pipe: false,
        },
        '>' => {
            let len = if next == Some('>') { 2 } else { 1 };
            OpInfo {
                len,
                paren_delta: 0,
                opens_command: false,
                is_pipe: false,
            }
        }
        '<' => {
            let len = if next == Some('<') { 2 } else { 1 };
            OpInfo {
                len,
                paren_delta: 0,
                opens_command: false,
                is_pipe: false,
            }
        }
        // `+` `*` `%` `!` `,` `/` 等：普通二元/一元操作符。
        _ => OpInfo {
            len: 1,
            paren_delta: 0,
            opens_command: false,
            is_pipe: false,
        },
    }
}

/// `-word` 形态中，`word`（已去掉前导 `-`）是否为 PowerShell 比较/逻辑操作符。
fn is_operator_word(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    // 先按原名匹配；未命中再剥离大小写敏感前缀 `c`/`i`（如 `-ceq`/`-ieq`）重试。
    // 剥离后须非空，避免把单字母 `-c`/`-i` 误判为操作符。
    if is_bare_operator_name(&w) {
        return true;
    }
    let base = w
        .strip_prefix('c')
        .or_else(|| w.strip_prefix('i'))
        .unwrap_or("");
    !base.is_empty() && is_bare_operator_name(base)
}

/// `word` 是否为 PowerShell 比较/逻辑/位操作符的裸名（不含 `-` 与大小写前缀）。
fn is_bare_operator_name(word: &str) -> bool {
    matches!(
        word,
        "eq" | "ne"
            | "gt"
            | "ge"
            | "lt"
            | "le"
            | "like"
            | "notlike"
            | "match"
            | "notmatch"
            | "contains"
            | "notcontains"
            | "in"
            | "notin"
            | "replace"
            | "split"
            | "join"
            | "is"
            | "isnot"
            | "as"
            | "and"
            | "or"
            | "xor"
            | "not"
            | "band"
            | "bor"
            | "bxor"
            | "bnot"
            | "shl"
            | "shr"
    )
}

/// 是否为 PowerShell 语言关键字（大小写不敏感）。
fn is_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "begin"
            | "break"
            | "catch"
            | "class"
            | "continue"
            | "data"
            | "define"
            | "do"
            | "dynamicparam"
            | "else"
            | "elseif"
            | "end"
            | "enum"
            | "exit"
            | "filter"
            | "finally"
            | "for"
            | "foreach"
            | "from"
            | "function"
            | "hidden"
            | "if"
            | "in"
            | "param"
            | "process"
            | "return"
            | "static"
            | "switch"
            | "throw"
            | "trap"
            | "try"
            | "until"
            | "using"
            | "while"
    )
}

// ─── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 提取单行的 token（便于断言）。
    fn line_tokens(line: &str) -> Vec<Token> {
        highlight_document(&[line]).into_iter().next().unwrap()
    }

    /// 断言某字节区间存在指定类别的 token。
    fn has_token(tokens: &[Token], text: &str, line: &str, kind: TokenKind) -> bool {
        tokens
            .iter()
            .any(|t| line.get(t.start..t.end) == Some(text) && t.kind == kind)
    }

    // ── 基础类别 ──────────────────────────────────────────────────────────────

    #[test]
    fn 命令名_行首裸词() {
        let line = "Get-ChildItem -Recurse";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "Get-ChildItem", line, TokenKind::Command));
        assert!(has_token(&toks, "-Recurse", line, TokenKind::Parameter));
    }

    #[test]
    fn 管道后回到命令位置() {
        let line = "ls | Select-Object Name";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "ls", line, TokenKind::Command));
        assert!(has_token(&toks, "|", line, TokenKind::Operator));
        assert!(has_token(&toks, "Select-Object", line, TokenKind::Command));
        // 管道后第二个裸词是参数（Text），非命令。
        assert!(has_token(&toks, "Name", line, TokenKind::Text));
    }

    #[test]
    fn 变量_含作用域与特殊() {
        let line = "$env:PATH $_ ${my var}";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "$env:PATH", line, TokenKind::Variable));
        assert!(has_token(&toks, "$_", line, TokenKind::Variable));
        assert!(has_token(&toks, "${my var}", line, TokenKind::Variable));
    }

    #[test]
    fn 数字与字符串() {
        let line = "echo 123 0xFF 1.5 \"hi\" 'world'";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "123", line, TokenKind::Number));
        assert!(has_token(&toks, "0xFF", line, TokenKind::Number));
        assert!(has_token(&toks, "1.5", line, TokenKind::Number));
        assert!(has_token(&toks, "\"hi\"", line, TokenKind::StringLit));
        assert!(has_token(&toks, "'world'", line, TokenKind::StringLit));
    }

    #[test]
    fn 比较操作符_区分参数() {
        let line = "if ($a -eq 1) { } -Recurse";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "if", line, TokenKind::Keyword));
        assert!(has_token(&toks, "-eq", line, TokenKind::Operator));
        assert!(has_token(&toks, "-Recurse", line, TokenKind::Parameter));
    }

    #[test]
    fn 行注释() {
        let line = "ls # 这是注释 -Force";
        let toks = line_tokens(line);
        assert!(has_token(
            &toks,
            "# 这是注释 -Force",
            line,
            TokenKind::Comment
        ));
        // 注释里的 -Force 不应单独成参数 token。
        assert!(!has_token(&toks, "-Force", line, TokenKind::Parameter));
    }

    #[test]
    fn csharp_井号不误判注释() {
        // `C#` 中部的 `#` 不是 token 起始，整体作裸词。
        let line = "dotnet C#proj";
        let toks = line_tokens(line);
        assert!(has_token(&toks, "C#proj", line, TokenKind::Text));
        assert!(!toks.iter().any(|t| t.kind == TokenKind::Comment));
    }

    // ── 跨行结构 ──────────────────────────────────────────────────────────────

    #[test]
    fn 块注释跨行() {
        let lines = vec!["before <# 注释开始", "still comment", "结束 #> after"];
        let grid = highlight_document(&lines);
        // 第二行整行为注释。
        assert_eq!(grid[1].len(), 1);
        assert_eq!(grid[1][0].kind, TokenKind::Comment);
        // 第三行 `结束 #>` 是注释收尾，`after` 是命令。
        assert!(has_token(&grid[2], "after", lines[2], TokenKind::Command));
    }

    #[test]
    fn here_string跨行() {
        let lines = vec!["$x = @\"", "line one", "line $y", "\"@", "echo done"];
        let grid = highlight_document(&lines);
        assert_eq!(grid[1][0].kind, TokenKind::StringLit);
        assert_eq!(grid[2][0].kind, TokenKind::StringLit);
        assert_eq!(grid[3][0].kind, TokenKind::StringLit);
        // here-string 结束后恢复正常着色。
        assert!(has_token(&grid[4], "echo", lines[4], TokenKind::Command));
    }

    // ── 续行检测 ──────────────────────────────────────────────────────────────

    #[test]
    fn 续行_未闭合括号() {
        assert!(needs_continuation(&["Get-Process | Where-Object ("]));
        assert!(!needs_continuation(&["Get-Process | Where-Object ( )"]));
    }

    #[test]
    fn 续行_行尾管道() {
        assert!(needs_continuation(&["ls |"]));
        assert!(!needs_continuation(&["ls | cat"]));
    }

    #[test]
    fn 续行_行尾反引号() {
        assert!(needs_continuation(&["Get-ChildItem `"]));
        // 反引号后还有内容 = 转义，非续行。
        assert!(!needs_continuation(&["echo a`b"]));
    }

    #[test]
    fn 续行_未闭合引号() {
        assert!(needs_continuation(&["echo \"未闭合"]));
        assert!(needs_continuation(&["echo '未闭合"]));
        assert!(!needs_continuation(&["echo \"已闭合\""]));
    }

    #[test]
    fn 续行_未闭合块注释与here() {
        assert!(needs_continuation(&["<# 块注释未闭合"]));
        assert!(needs_continuation(&["$x = @\"", "内容未收尾"]));
        assert!(!needs_continuation(&["<# 闭合 #>"]));
    }

    #[test]
    fn 续行_完整命令不续行() {
        assert!(!needs_continuation(&["Get-ChildItem -Recurse"]));
        assert!(!needs_continuation(&[""]));
        assert!(!needs_continuation(&["if ($a) { echo 1 }"]));
    }

    #[test]
    fn 续行_嵌套括号配平() {
        assert!(needs_continuation(&["foo (bar (baz"]));
        assert!(!needs_continuation(&["foo (bar (baz)))"]));
    }

    #[test]
    fn 续行_多行配平闭合() {
        // 第一行开括号，第二行闭合 → 不再续行。
        let lines = vec!["Where-Object {", "$_.Name", "}"];
        assert!(!needs_continuation(&lines));
        // 缺末行闭合 → 续行。
        let open = vec!["Where-Object {", "$_.Name"];
        assert!(needs_continuation(&open));
    }

    // ── 健壮性：永不 panic ────────────────────────────────────────────────────

    #[test]
    fn 健壮性_中文与emoji不panic() {
        for line in [
            "你好 世界 | grep 中文",
            "echo 👨‍👩‍👧‍👦 -p",
            "$变量 = 1",
            "'未闭合中文串",
        ] {
            let _ = line_tokens(line);
            let _ = needs_continuation(&[line]);
        }
    }

    #[test]
    fn 健壮性_空与纯符号() {
        for line in ["", "   ", "|||", "${", "$", "@\"", "<#", "\"", "`"] {
            let _ = line_tokens(line);
            let _ = needs_continuation(&[line]);
        }
    }

    #[test]
    fn token区间合法且有序() {
        let line = "Get-ChildItem -Path $env:HOME | Select -First 5 # done";
        let toks = line_tokens(line);
        let mut last_end = 0;
        for t in &toks {
            assert!(t.start >= last_end, "token 区间应不重叠递增");
            assert!(t.end <= line.len());
            assert!(line.get(t.start..t.end).is_some(), "区间须落在字符边界");
            last_end = t.end;
        }
    }
}
