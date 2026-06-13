//! Lumen 的 PTY 抽象层。
//!
//! 基于 portable-pty 封装：Windows 走 ConPTY，unix 走 openpty，
//! 本 crate 自身不含平台分支。输出读取在独立线程进行，
//! 通过 crossbeam channel 把字节流推给上层（主事件循环）。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

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
    /// 子进程杀手（Drop 时杀进程）。child 本体 move 进 wait 线程阻塞等
    /// 退出（见 [`Self::spawn`]），故这里只留可 clone 的 killer。
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// 进程存活标志：wait 线程在子进程退出时置 false（[`Self::is_alive`] 据此）。
    alive: Arc<AtomicBool>,
    /// shell 子进程 PID（spawn 时捕获，恒定）。用于查会话内前台运行的
    /// 程序（侧栏会话图标读取其 exe 图标，F7②）。spawn 后端取不到时 None。
    shell_pid: Option<u32>,
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
    /// `cwd` 为 shell 初始工作目录（会话恢复用，F4）；None 沿用
    /// 本进程当前目录。调用方需保证目录存在——不存在时子进程会
    /// 启动失败。
    pub fn spawn(
        shell: Option<&str>,
        args: &[String],
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
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
        if let Some(dir) = cwd {
            // 会话恢复：shell 在保存的工作目录中启动。
            cmd.cwd(dir);
        }
        // 终端能力声明：上层实现了 256 色与真彩 SGR。
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("启动 shell 失败: {shell}"))?;
        // slave 端句柄交给子进程后即可关闭，否则读端永远等不到 EOF。
        drop(pair.slave);
        // killer 留作 Drop 杀进程；child 本体稍后 move 进 wait 线程等退出。
        let killer = child.clone_killer();
        // shell PID 在 child move 走前捕获（侧栏会话图标用，F7②）。
        let shell_pid = child.process_id();
        // 进程存活标志：wait 线程在子进程退出时翻转。
        let alive = Arc::new(AtomicBool::new(true));

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
        // wait 线程先取一份发送端与存活标志（读线程随后 move 走 tx）。
        let wait_tx = tx.clone();
        let wait_alive = alive.clone();
        let mut wait_child = child;
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

        // wait 线程：直接阻塞等子进程退出再发 Exited，不依赖 ConPTY 的
        // read EOF——Windows ConPTY 下 shell 退出后 master read 常不返回
        // EOF（conhost 保持 pipe 打开），只靠读线程会漏报退出、窗格卡死
        // 无响应（海风哥 2026-06-13 实测 `exit` 无反应的真因）。读线程的
        // EOF 分支仍保留作兜底；两路都可能发 Exited，上层按窗格 id 去重。
        std::thread::Builder::new()
            .name("lumen-pty-wait".into())
            .spawn(move || {
                let _ = wait_child.wait();
                wait_alive.store(false, Ordering::Release);
                let _ = wait_tx.send(PtyEvent::Exited);
            })
            .context("启动 PTY 等待线程失败")?;

        Ok((
            Self {
                master: pair.master,
                killer,
                alive,
                shell_pid,
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

    /// shell 进程是否仍在运行（wait 线程在子进程退出时翻转此标志）。
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// shell 子进程 PID（spawn 时捕获，恒定）；后端取不到时 None。
    /// 侧栏会话图标据此查前台运行程序的 exe 图标（F7②）。
    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
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
