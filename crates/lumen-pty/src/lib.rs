//! Lumen 的 PTY 抽象层。
//!
//! 基于 portable-pty 封装：Windows 走 ConPTY，unix 走 openpty，
//! 本 crate 自身不含平台分支。输出读取在独立线程进行，
//! 通过 crossbeam channel 把字节流推给上层（主事件循环）。

use std::io::{Read, Write};

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// PTY 输出事件，由读线程推送。
#[derive(Debug)]
pub enum PtyEvent {
    /// shell 输出的原始字节（含 VT 转义序列）。
    Data(Vec<u8>),
    /// shell 进程已退出。
    Exited,
}

/// 一个运行中的 shell 会话。
///
/// Drop 时杀掉子进程，避免孤儿 shell。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// 写入走独立线程：ConPTY 输入管道在 conhost 繁忙时会反压，
    /// 主线程（UI 事件循环）绝不能阻塞在管道写上。
    write_tx: Sender<Vec<u8>>,
}

impl PtySession {
    /// 启动 shell 并返回会话与输出事件接收端。
    ///
    /// `shell` 为 None 时按平台选默认：Windows 优先 `pwsh.exe`，
    /// 找不到则回退 `powershell.exe`；unix 用 `$SHELL` 或 `/bin/bash`。
    /// `args` 为附加启动参数（如 shell integration 注入）。
    pub fn spawn(
        shell: Option<&str>,
        args: &[String],
        rows: u16,
        cols: u16,
    ) -> Result<(Self, Receiver<PtyEvent>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("打开 PTY 失败")?;

        let shell = shell.map(str::to_owned).unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell);
        cmd.args(args);
        // 终端能力声明：上层实现了 256 色与真彩 SGR。
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("启动 shell 失败: {shell}"))?;
        // slave 端句柄交给子进程后即可关闭，否则读端永远等不到 EOF。
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("克隆 PTY 读端失败")?;
        let mut writer = pair.master.take_writer().context("获取 PTY 写端失败")?;

        // 写线程：键盘输入量小，无界通道安全；发送端 drop 后线程退出。
        let (write_tx, write_rx) = unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name("lumen-pty-writer".into())
            .spawn(move || {
                for data in write_rx {
                    if writer
                        .write_all(&data)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("启动 PTY 写线程失败")?;

        // 有界通道形成背压：渲染端消费不过来时读线程会阻塞，
        // 避免 `yes` 这类高速输出把内存撑爆。
        let (tx, rx) = bounded::<PtyEvent>(128);
        std::thread::Builder::new()
            .name("lumen-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(PtyEvent::Exited);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .context("启动 PTY 读线程失败")?;

        Ok((
            Self {
                master: pair.master,
                child,
                write_tx,
            },
            rx,
        ))
    }

    /// 向 shell 写入用户输入（已编码为 VT 序列的字节）。
    /// 实际写入由独立线程完成，本方法不阻塞。
    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.write_tx
            .send(data.to_vec())
            .map_err(|_| anyhow::anyhow!("PTY 写线程已退出"))
    }

    /// 通知 PTY 窗口尺寸变化（行/列）。
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("调整 PTY 尺寸失败")
    }

    /// shell 进程是否仍在运行。
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// 按平台返回默认 shell。
fn default_shell() -> String {
    #[cfg(windows)]
    {
        // pwsh（PowerShell 7+）体验更好，装了就优先用。
        if which_in_path("pwsh.exe") {
            "pwsh.exe".into()
        } else {
            "powershell.exe".into()
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

/// 在 PATH 中查找可执行文件是否存在。
#[cfg(windows)]
fn which_in_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(exe).is_file()))
        .unwrap_or(false)
}
