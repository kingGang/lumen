//! 命令补全 sidecar 进程管理（M4.4 批2）。
//!
//! 维护一个持久隐藏 pwsh 进程，通过 stdin/stdout 管道实现
//! 行级 JSON 协议的命令/参数补全请求-响应。进程崩溃时自愈重启；
//! 响应到达后通过 [`winit::event_loop::EventLoopProxy`] 唤醒主循环。
//!
//! ## 协议
//! - 请求（写 stdin）：`{"id":<u64>,"line":"<cmdline>","col":<usize>,"cwd":"<dir?>"}`
//! - 响应（读 stdout）：`{"id":<u64>,"ri":<usize>,"rl":<usize>,"items":[{"text":"..","type":".."}]}`
//!   - `ri`/`rl` = PowerShell `ReplacementIndex`/`ReplacementLength`（`line` 内的 **char** 索引，
//!     需转换为字节偏移后才能操作 Rust `&str`）。
//!   - `type` ∈ `Command` | `ParameterName` | `ProviderItem` | `ProviderContainer` | …

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use winit::event_loop::EventLoopProxy;

use crate::PtyWake;

// ─── 协议类型 ────────────────────────────────────────────────────────────────

/// sidecar JSON 请求（发往 pwsh 的 stdin）。
#[derive(Debug, Serialize)]
pub struct SidecarRequest {
    /// 请求序列号（调用方用于对齐响应、丢弃过期条目）。
    pub id: u64,
    /// 当前命令行文本。
    pub line: String,
    /// 光标在 `line` 中的字符（char）偏移。
    pub col: usize,
    /// 当前工作目录（可选；空串时 pwsh 保持上次目录）。
    pub cwd: String,
}

/// sidecar 响应中的单条补全候选（来自 pwsh stdout）。
#[derive(Debug, Deserialize, Clone)]
pub struct SidecarItem {
    /// 候选文本（用于替换）。
    pub text: String,
    /// 候选类型字符串（`Command` / `ParameterName` / `ProviderItem` / …）。
    #[serde(rename = "type")]
    pub kind: String,
}

/// sidecar JSON 响应（pwsh stdout 的一行）。
#[derive(Debug, Deserialize, Clone)]
pub struct SidecarResponse {
    /// 对应请求的序列号。
    pub id: u64,
    /// PowerShell ReplacementIndex（`line` 内 **char** 索引，非字节偏移）。
    pub ri: usize,
    /// PowerShell ReplacementLength（替换区间长度，以 **char** 计）。
    pub rl: usize,
    /// 补全候选列表（最多 100 条，pwsh 脚本已截断）。
    pub items: Vec<SidecarItem>,
}

// ─── sidecar 管理器 ──────────────────────────────────────────────────────────

/// 持久 pwsh sidecar 进程的管理器。
///
/// 封装进程生命周期、协议序列化/反序列化、响应通道和崩溃自愈。
/// 所有公共方法在主线程调用；响应由独立读线程经通道送回主线程。
///
/// # 使用方式
/// ```ignore
/// // 启动时初始化（在 AppState 构造中）：
/// let sidecar = CompletionSidecar::new(proxy.clone());
///
/// // Tab 键触发补全请求：
/// let req_id = sidecar.request("Get-Ch", 6, "C:\\Users\\foo");
/// state.completion_req_id = req_id;
///
/// // user_event(PtyWake) 中 drain 响应：
/// for resp in sidecar.poll() {
///     if resp.id == state.completion_req_id { /* 合并候选 */ }
/// }
/// ```
pub struct CompletionSidecar {
    /// 当前持久 pwsh 进程（None = 尚未启动或已自愈重启途中）。
    child: Option<Child>,
    /// pwsh 进程的 stdin 写端（与 child 同生命周期）。
    stdin: Option<ChildStdin>,
    /// 响应接收端（主线程 poll 使用）。
    rx: Receiver<SidecarResponse>,
    /// 响应发送端模板（每次 spawn 读线程时 clone）。
    tx: Sender<SidecarResponse>,
    /// 下一个请求序列号（单调递增）。
    next_req_id: u64,
    /// 事件循环唤醒句柄（响应到达时用 send_event 唤醒主循环）。
    proxy: EventLoopProxy<PtyWake>,
    /// 读线程「进程已退出」标志（读线程设 true 主线程据此触发重启）。
    reader_dead: Arc<AtomicBool>,
    /// pwsh 永久缺失标志：spawn 报 `NotFound`（系统未装 PowerShell，常见于
    /// Linux/macOS）后置 true，此后 `ensure_alive` 直接跳过 spawn，避免每次
    /// 补全请求都白 spawn + 刷日志。装了 pwsh 的 unix 不受影响（正常启用）。
    disabled: bool,
}

impl CompletionSidecar {
    /// 构造 sidecar 管理器（**不立即 spawn 进程**，首次 `request` 时懒启动）。
    pub fn new(proxy: EventLoopProxy<PtyWake>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<SidecarResponse>();
        Self {
            child: None,
            stdin: None,
            rx,
            tx,
            next_req_id: 1,
            proxy,
            reader_dead: Arc::new(AtomicBool::new(false)),
            disabled: false,
        }
    }

    /// 向 sidecar 发送一条补全请求，返回分配的请求 id（调用方需记录
    /// 以便对齐、丢弃过期响应）。
    ///
    /// 若进程尚未启动或已崩溃，先尝试重新 spawn；spawn 失败则静默降级
    /// （命令补全无结果，文件路径补全不受影响）。
    ///
    /// # Arguments
    /// * `line` - 当前命令行文本。
    /// * `col`  - 光标在 `line` 的 char 偏移（传给 pwsh `CompleteInput`）。
    /// * `cwd`  - 当前工作目录字符串（空串时 pwsh 保持上次目录）。
    ///
    /// # Returns
    /// 分配的请求 id（单调递增，0 为保留无效值）。
    pub fn request(&mut self, line: &str, col: usize, cwd: &str) -> u64 {
        // 确保进程存活（懒启动 / 崩溃自愈）。
        self.ensure_alive();

        let Some(stdin) = self.stdin.as_mut() else {
            // spawn 失败，静默降级，返回一个不会被匹配的 id。
            return 0;
        };

        let id = self.next_req_id;
        self.next_req_id += 1;

        let req = SidecarRequest {
            id,
            line: line.to_owned(),
            col,
            cwd: cwd.to_owned(),
        };

        // 序列化为 JSON 单行，追加 '\n' 触发 pwsh readline。
        match serde_json::to_string(&req) {
            Ok(mut json) => {
                json.push('\n');
                if let Err(e) = stdin.write_all(json.as_bytes()) {
                    warn!("sidecar stdin 写失败（{e}），标记进程死亡");
                    self.kill_child();
                    return 0;
                }
            }
            Err(e) => {
                error!("sidecar 请求序列化失败（不应发生）: {e}");
                return 0;
            }
        }

        id
    }

    /// 非阻塞 drain 通道中所有已到达的响应（主循环 user_event 调用）。
    ///
    /// 调用方只需保留 `resp.id == current_req_id` 的响应，其余视为过期丢弃。
    pub fn poll(&self) -> Vec<SidecarResponse> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(resp) => out.push(resp),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    // ── 内部：进程管理 ────────────────────────────────────────────────────────

    /// 确保 sidecar 进程存活：若未启动或读线程标记死亡，则重新 spawn。
    fn ensure_alive(&mut self) {
        if self.disabled {
            return; // pwsh 缺失已判定，永久跳过 spawn（避免每次请求白试）。
        }
        let dead = self.reader_dead.load(Ordering::Acquire);
        if !dead && self.child.is_some() {
            return; // 进程正常运行，无需操作。
        }
        if dead {
            // 读线程已结束（进程退出），清理旧状态后重启。
            info!("sidecar 进程已退出，触发自愈重启");
            self.kill_child();
        }
        self.spawn();
    }

    /// 清理 child/stdin 句柄（读线程将自然检测 EOF 然后退出）。
    fn kill_child(&mut self) {
        // 先 drop stdin，EOF 会让 pwsh readline 返回 null 并退出循环。
        self.stdin = None;
        if let Some(mut c) = self.child.take() {
            // 尝试杀死进程（失败忽略，进程可能已自然退出）。
            let _ = c.kill();
            // 不 wait：blocking wait 会卡主线程；进程已 kill，OS 会回收。
        }
        self.reader_dead.store(false, Ordering::Release);
    }

    /// Spawn 新的 pwsh sidecar 进程并启动 stdout 读线程。
    fn spawn(&mut self) {
        let encoded = sidecar_encoded_command();
        let mut cmd = Command::new("pwsh");
        cmd.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-EncodedCommand")
            .arg(&encoded)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()); // 不需要 pwsh 的错误输出。

        // Windows：CREATE_NO_WINDOW (0x08000000) 阻止 pwsh 弹出控制台窗口。
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000u32);
        }

        match cmd.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                let stdout = child.stdout.take();

                self.child = Some(child);
                self.stdin = stdin;
                self.reader_dead.store(false, Ordering::Release);

                // 启动 stdout 读线程。
                if let Some(stdout_pipe) = stdout {
                    let tx = self.tx.clone();
                    let proxy = self.proxy.clone();
                    let dead_flag = Arc::clone(&self.reader_dead);
                    if let Err(e) = std::thread::Builder::new()
                        .name("lumen-completion-sidecar-reader".into())
                        .spawn(move || {
                            let reader = BufReader::new(stdout_pipe);
                            for line_result in reader.lines() {
                                match line_result {
                                    Ok(line) if !line.is_empty() => {
                                        match serde_json::from_str::<SidecarResponse>(&line) {
                                            Ok(resp) => {
                                                // 发送响应；若主线程接收端已丢弃（应用退出），结束线程。
                                                if tx.send(resp).is_err() {
                                                    break;
                                                }
                                                // 唤醒主循环（不需要去重：sidecar 响应频率远低于 PTY 输出）。
                                                let _ = proxy.send_event(PtyWake);
                                            }
                                            Err(e) => {
                                                warn!("sidecar 响应解析失败: {e} | raw: {line}");
                                            }
                                        }
                                    }
                                    Ok(_) => {} // 空行，忽略。
                                    Err(e) => {
                                        warn!("sidecar stdout 读取错误: {e}");
                                        break;
                                    }
                                }
                            }
                            // 进程已退出（EOF），通知主线程需要重启。
                            dead_flag.store(true, Ordering::Release);
                            // 再发一次 wake 让主线程有机会看到 dead_flag（下次 request 时自愈）。
                            let _ = proxy.send_event(PtyWake);
                        })
                    {
                        error!("启动 sidecar 读线程失败: {e}");
                    }
                }
                info!("completion sidecar 进程已启动");
            }
            Err(e) => {
                self.child = None;
                self.stdin = None;
                if e.kind() == std::io::ErrorKind::NotFound {
                    // 系统未装 pwsh（常见于 Linux/macOS）：永久禁用命令补全，
                    // 不再每次请求重试。文件路径补全不受影响。只 info 一次。
                    info!("未找到 pwsh，命令补全禁用（文件路径补全仍可用）");
                    self.disabled = true;
                } else {
                    // 其它错误（权限/资源）可能是暂时性的：warn 但保留重试。
                    warn!("completion sidecar spawn 失败: {e}");
                }
            }
        }
    }
}

impl Drop for CompletionSidecar {
    /// 应用退出时关闭 sidecar 进程（不 wait，让 OS 处理孤儿）。
    fn drop(&mut self) {
        self.kill_child();
    }
}

// ─── 工具函数 ────────────────────────────────────────────────────────────────

/// 将 `completion_server.ps1` 脚本编码为 pwsh `-EncodedCommand` 参数
/// （UTF-16LE → base64，与 `session.rs` 的 `shell_integration_args` 完全同款）。
///
/// 每次调用时计算（脚本 `include_str!` 是编译期常量，运行时只做编码，开销可忽略）。
fn sidecar_encoded_command() -> String {
    let script = include_str!("../assets/completion_server.ps1");
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&utf16le)
}

/// 标准 base64 编码（RFC 4648，带 `=` padding，无换行）。
///
/// 与 `session::base64_encode` 完全相同的实现，此处复制以避免跨模块可见性改动；
/// 若将来 session 的版本升格为 `pub(crate)` 可统一删此处并直接引用。
///
/// # Returns
/// 标准 base64 字符串（仅含 `A-Za-z0-9+/=`）。
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 将 PowerShell 给出的 char 索引区间 `[ri, ri + rl)` 换算为
/// `line` 字符串中对应的**字节**偏移区间 `(byte_start, byte_end)`。
///
/// pwsh `CompleteInput` 返回的 `ri`/`rl` 是 Unicode char 计数，
/// 而 Rust `&str` 操作依赖字节偏移，含 ASCII 以外字符（中文命令行等）
/// 时必须显式换算，不能直接用 char 偏移当字节偏移。
///
/// # Arguments
/// * `line` - 当前命令行原文（UTF-8 字符串）。
/// * `ri`   - 替换起始（char 偏移，来自 pwsh）。
/// * `rl`   - 替换长度（char 数，来自 pwsh）。
///
/// # Returns
/// `(byte_start, byte_end)`，均夹紧在 `[0, line.len()]`。
pub fn char_range_to_bytes(line: &str, ri: usize, rl: usize) -> (usize, usize) {
    // 把 line 的 char 索引序列实体化一次；对于 ASCII 命令行这非常快。
    let mut char_indices = line.char_indices();

    // 找 ri-th char 的字节起始位置。
    let byte_start = if ri == 0 {
        0
    } else {
        char_indices
            .nth(ri - 1) // 先走到 ri-1...
            .map(|(b, c)| b + c.len_utf8()) // ...再 +1 char
            .unwrap_or(line.len())
    };

    // 找 (ri + rl)-th char 的字节起始位置（即替换区间结束）。
    // 用独立的 char_indices 迭代器重置到零，再走到 (ri+rl)-th 位置。
    let byte_end = if rl == 0 {
        byte_start
    } else {
        let end_char_idx = ri + rl;
        if end_char_idx == 0 {
            0
        } else {
            let mut ci2 = line.char_indices();
            ci2.nth(end_char_idx - 1)
                .map(|(b, c)| b + c.len_utf8())
                .unwrap_or(line.len())
        }
    };

    let byte_start = byte_start.min(line.len());
    let byte_end = byte_end.min(line.len());
    (byte_start, byte_end.max(byte_start))
}

// ─── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── base64_encode ──────────────────────────────────────────────────────────

    #[test]
    fn base64_rfc4648_标准向量() {
        // RFC 4648 §10 官方测试向量，与 session.rs 版本保持完全一致。
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_含高位字节与padding() {
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xBF]), "+/+/");
        assert_eq!(base64_encode(&[0xFF]), "/w==");
        assert_eq!(base64_encode(&[0xFF, 0xFF]), "//8=");
    }

    #[test]
    fn base64_utf16le_中文往返() {
        let s = "Get-ChildItem";
        let utf16le: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let enc = base64_encode(&utf16le);
        // 长度符合 base64(n 字节) = ceil(n/3)*4。
        assert_eq!(enc.len(), utf16le.len().div_ceil(3) * 4);
        // 字符集合法。
        assert!(enc
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')));
    }

    // ── JSON 序列化/反序列化 ───────────────────────────────────────────────────

    #[test]
    fn sidecar_request_序列化正确() {
        let req = SidecarRequest {
            id: 42,
            line: "Get-Ch".into(),
            col: 6,
            cwd: "C:\\Users\\foo".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        // 必须包含所有字段。
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"line\":\"Get-Ch\""));
        assert!(json.contains("\"col\":6"));
        assert!(json.contains("\"cwd\":\"C:\\\\Users\\\\foo\""));
    }

    #[test]
    fn sidecar_response_反序列化正确() {
        // 模拟 pwsh 输出的真实响应格式。
        let raw = r#"{"id":1,"ri":0,"rl":6,"items":[{"text":"Get-ChildItem","type":"Command"}]}"#;
        let resp: SidecarResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, 1);
        assert_eq!(resp.ri, 0);
        assert_eq!(resp.rl, 6);
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].text, "Get-ChildItem");
        assert_eq!(resp.items[0].kind, "Command");
    }

    #[test]
    fn sidecar_response_空候选列表() {
        let raw = r#"{"id":99,"ri":0,"rl":0,"items":[]}"#;
        let resp: SidecarResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, 99);
        assert!(resp.items.is_empty());
    }

    #[test]
    fn sidecar_response_参数补全() {
        // ls -<Tab> 的典型响应片段。
        let raw = r#"{"id":5,"ri":3,"rl":1,"items":[{"text":"-Path","type":"ParameterName"},{"text":"-Filter","type":"ParameterName"}]}"#;
        let resp: SidecarResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.ri, 3);
        assert_eq!(resp.rl, 1);
        assert_eq!(resp.items[0].kind, "ParameterName");
    }

    // ── char_range_to_bytes ────────────────────────────────────────────────────

    #[test]
    fn char_range_纯ascii_对应字节() {
        let line = "Get-Ch";
        // pwsh ri=0, rl=6 → 替换整个 token
        let (s, e) = char_range_to_bytes(line, 0, 6);
        assert_eq!(s, 0);
        assert_eq!(e, 6);
    }

    #[test]
    fn char_range_参数场景_ascii() {
        let line = "ls -";
        // ri=3, rl=1 → 替换 '-' (byte 3..4)
        let (s, e) = char_range_to_bytes(line, 3, 1);
        assert_eq!(s, 3);
        assert_eq!(e, 4);
    }

    #[test]
    fn char_range_含中文字符() {
        // "查询 Get" → bytes: "查"=3B,"询"=3B," "=1B,"G"=1B,"e"=1B,"t"=1B  合计 11B
        // char indices: 0="查", 1="询", 2=" ", 3="G", 4="e", 5="t"
        let line = "查询 Get";
        assert_eq!(line.len(), 3 + 3 + 1 + 1 + 1 + 1); // 10 bytes
                                                       // ri=3 (从第4个char 'G'), rl=3 (Get) → byte 7..10
        let (s, e) = char_range_to_bytes(line, 3, 3);
        assert_eq!(&line[s..e], "Get");
    }

    #[test]
    fn char_range_rl为零() {
        let line = "ls ";
        // ri=3, rl=0 → 空替换区间（光标位置）
        let (s, e) = char_range_to_bytes(line, 3, 0);
        assert_eq!(s, e); // 空区间，字节相等
    }

    #[test]
    fn char_range_超出边界夹紧() {
        let line = "ab";
        // ri=0, rl=100 → 夹紧到行末
        let (s, e) = char_range_to_bytes(line, 0, 100);
        assert_eq!(s, 0);
        assert_eq!(e, 2);
    }

    #[test]
    fn char_range_ri超出行末夹紧() {
        let line = "ab";
        let (s, e) = char_range_to_bytes(line, 99, 1);
        assert_eq!(s, line.len());
        assert_eq!(e, line.len());
    }

    // ── sidecar_encoded_command ────────────────────────────────────────────────

    #[test]
    fn encoded_command_非空且合法base64() {
        let enc = sidecar_encoded_command();
        assert!(!enc.is_empty(), "脚本 base64 不应为空");
        assert!(enc
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')));
    }
}
