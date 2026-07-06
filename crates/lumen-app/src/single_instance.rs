//! F8 单实例限制（需求池 F8；B1「布局重启不还原」根因——多实例互
//! 踩配置文件、后写者赢——的正解）。
//!
//! release 构建默认单实例：进程启动早期（winit 事件循环创建前）创建
//! 命名互斥量，`GetLastError == ERROR_ALREADY_EXISTS` 说明已有实例在
//! 跑——通过命名事件（SetEvent）通知它前台化，本进程静默退出。第一
//! 实例起一个 detach 的后台线程 `WaitForSingleObject` 等该事件，收到
//! 信号置原子标志并借既有 `PtyWake` user event 唤醒主循环（唤醒协议
//! 与事件类型零变化），主循环在 `user_event` 入口取走标志做
//! `set_minimized(false) + focus_window + request_user_attention`。
//!
//! 多开口径（海风哥拍板：正式版不可多开、测试版可多开）：debug 构建
//! 默认放开；release 用 `--multi-instance` 参数或环境变量
//! `LUMEN_MULTI_INSTANCE=1` 跳过检测（测试逃生口）。
//!
//! 对象名挂 `Local\` 会话级命名空间并带用户名后缀：命名空间已按登录
//! 会话隔离，用户名是同机多用户场景的纵深防御。

use std::sync::atomic::{AtomicBool, Ordering};

use winit::event_loop::EventLoopProxy;

use crate::PtyWake;

/// 第二实例请求前台化的标志：监听线程置位，主循环 `user_event` 入口
/// 经 [`take_foreground_request`] 取走（swap false）。
static FOREGROUND_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 单实例检测结果。
pub enum InstanceCheck {
    /// 本进程是第一个实例：持有命名互斥量（句柄不手动关闭，存活期 =
    /// 进程存活期，由系统在进程退出时回收——关闭即放开单实例锁）。
    Primary(PrimaryGuard),
    /// 已有实例在运行（已通知其前台化），本进程应静默退出。
    AlreadyRunning,
    /// 多开放行（debug 构建 / 逃生口 / 极端的 Win32 调用失败兜底），
    /// 未持有任何内核对象。
    MultiAllowed,
}

/// 第一实例持有的前台化通道句柄（各平台不同）。
pub struct PrimaryGuard {
    /// Windows：前台化事件句柄（监听线程 wait 用）。以 usize 形式保存便于
    /// 跨线程传递（`HANDLE` 是裸指针、非 Send）；0 = 事件创建失败，单实例
    /// 锁仍生效但无前台化通道。
    #[cfg(windows)]
    event: usize,
    /// Linux：绑定在抽象命名空间的 Unix 套接字监听端。持有它即持有单实例
    /// 锁（进程退出时内核自动释放抽象地址，无残留套接字文件）；监听线程
    /// `try_clone` 一份 accept 第二实例的前台化连接。
    #[cfg(target_os = "linux")]
    listener: std::os::unix::net::UnixListener,
}

/// 主循环取走前台化请求：有挂起请求则清零并返回 true。
pub fn take_foreground_request() -> bool {
    FOREGROUND_REQUESTED.swap(false, Ordering::AcqRel)
}

/// 多开逃生口判定（纯函数便于单测）：参数含 `--multi-instance` 或
/// 环境变量 `LUMEN_MULTI_INSTANCE` 值为 `1`。
fn multi_flag<I>(args: I, env_val: Option<&str>) -> bool
where
    I: IntoIterator<Item = String>,
{
    env_val == Some("1") || args.into_iter().any(|a| a == "--multi-instance")
}

/// 单实例检测入口（main 早期、事件循环创建前调用）。
pub fn acquire() -> InstanceCheck {
    // 测试版（debug 构建）默认放开多开：开发期常并行起多个实例对比。
    if cfg!(debug_assertions) {
        log::debug!("debug 构建：单实例检测默认放开（可多开）");
        return InstanceCheck::MultiAllowed;
    }
    let env_val = std::env::var("LUMEN_MULTI_INSTANCE").ok();
    if multi_flag(std::env::args().skip(1), env_val.as_deref()) {
        log::info!("多开逃生口启用（--multi-instance / LUMEN_MULTI_INSTANCE=1），跳过单实例检测");
        return InstanceCheck::MultiAllowed;
    }
    acquire_impl()
}

/// 第一实例：起前台化监听线程（detach，随进程退出，无需 join）。
pub fn spawn_foreground_listener(guard: &PrimaryGuard, proxy: EventLoopProxy<PtyWake>) {
    spawn_listener_impl(guard, proxy);
}

/// 字符串转 0 结尾的 UTF-16 缓冲（Win32 宽字符串约定）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 用户名清洗：内核对象名不允许命名空间前缀之外的反斜杠（Windows
/// 账户名本身不含 `\`，纯防御——比如 USERNAME 被手动设成域形式）。
fn sanitize_user(user: &str) -> String {
    user.replace('\\', "_")
}

/// 互斥量与前台化事件的内核对象名（UTF-16，0 结尾）。
#[cfg(windows)]
fn object_names() -> (Vec<u16>, Vec<u16>) {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_owned());
    let user = sanitize_user(&user);
    (
        wide(&format!("Local\\Lumen-SingleInstance-{user}")),
        wide(&format!("Local\\Lumen-SingleInstance-Event-{user}")),
    )
}

#[cfg(windows)]
fn acquire_impl() -> InstanceCheck {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};

    let (mutex_name, event_name) = object_names();
    // SAFETY: 名称是 0 结尾的合法 UTF-16 缓冲，调用期间存活；安全属性
    // 传空指针 = 默认安全描述符。bInitialOwner=FALSE——只用「对象是否
    // 已存在」这一语义，不进入持有态，无需 ReleaseMutex。
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    // SAFETY: GetLastError 无前置条件，紧跟 CreateMutexW 读取其结果
    //（即使创建成功，已存在时也会置 ERROR_ALREADY_EXISTS）。
    let last_err = unsafe { GetLastError() };
    if mutex.is_null() {
        // 极端失败（权限/句柄耗尽）：放行启动——单实例是簿记性约束，
        // 不该挡掉终端主功能。
        log::warn!("创建单实例互斥量失败（GetLastError={last_err}），放行本次启动");
        return InstanceCheck::MultiAllowed;
    }
    if last_err == ERROR_ALREADY_EXISTS {
        // 已有实例在跑：SetEvent 通知其前台化，本进程随后静默退出。
        // SAFETY: 同 CreateMutexW——0 结尾 UTF-16 名称 + 默认安全属性；
        // auto-reset（bManualReset=FALSE）、初始未触发。同名事件已被
        // 第一实例创建时返回的就是既有事件的句柄（正是所需）。
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
        if event.is_null() {
            log::warn!("打开前台化事件失败，无法通知已有实例前台化（仅影响前台化，不影响单实例）");
        } else {
            // SAFETY: event 非空，是有效的事件对象句柄。
            unsafe { SetEvent(event) };
            // SAFETY: event 非空且本进程不再使用；显式关闭保持整洁。
            unsafe { CloseHandle(event) };
        }
        // SAFETY: mutex 非空；本进程未持有互斥量（bInitialOwner=FALSE
        // 且已存在），关闭句柄不影响第一实例的锁。
        unsafe { CloseHandle(mutex) };
        return InstanceCheck::AlreadyRunning;
    }
    // 本进程是第一实例：创建前台化事件供后续实例 SetEvent。互斥量
    // 句柄此后不再引用，但绝不 CloseHandle——它必须存活整个进程周期
    //（进程退出时由系统统一回收）。
    // SAFETY: 同上（auto-reset：SetEvent 唤醒一个 waiter 后自动复位，
    // 多次触发不累积粘连）。
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if event.is_null() {
        log::warn!("创建前台化事件失败：单实例仍生效，但后续实例无法触发前台化");
    }
    InstanceCheck::Primary(PrimaryGuard {
        event: event as usize,
    })
}

#[cfg(windows)]
fn spawn_listener_impl(guard: &PrimaryGuard, proxy: EventLoopProxy<PtyWake>) {
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

    let event = guard.event;
    if event == 0 {
        return; // 事件创建失败（acquire 已警告）：无前台化通道可监听
    }
    let spawned = std::thread::Builder::new()
        .name("single-instance-fg".to_owned())
        .spawn(move || {
            loop {
                // SAFETY: event 是 acquire_impl 创建的有效事件句柄，
                // 进程存活期内不关闭（usize 仅为跨线程传递，原值不变）。
                let r = unsafe { WaitForSingleObject(event as HANDLE, INFINITE) };
                if r != WAIT_OBJECT_0 {
                    // WAIT_FAILED 等异常：不自旋重试，监听就此结束
                    //（单实例锁不受影响，只是失去前台化能力）。
                    log::warn!("前台化事件等待异常（返回 {r:#x}），监听线程退出");
                    return;
                }
                log::info!("收到第二实例的前台化信号");
                FOREGROUND_REQUESTED.store(true, Ordering::Release);
                // 借既有 PtyWake 唤醒主循环。不参与 wake_pending 去重
                //（该协议管的是数据通道的高频唤醒合并；本信号极稀疏，
                // 多一次空 drain 无害）。Err = 事件循环已关闭，线程退出。
                if proxy.send_event(PtyWake).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("前台化监听线程启动失败（仅影响前台化，不影响单实例）: {e}");
    }
}

// —— Linux：抽象命名空间 Unix 套接字既作单实例锁又作前台化 IPC。 ——
//
// 抽象套接字（名字以 NUL 起始，不落文件系统）由内核在进程退出时自动回收，
// 无残留 socket 文件、无需清理逻辑，天然规避「上次崩溃留下陈旧锁」问题。
// `bind` 成功 = 本进程是第一实例（持有监听端即持有锁）；`bind` 报 `AddrInUse`
// = 已有实例在跑，连上去写一字节请求其前台化，本进程静默退出。名字带用户名
// 后缀做同机多用户隔离（抽象套接字对同网络命名空间所有用户可见，与 Windows
// `Local\` 命名空间同样是簿记性约束、非安全边界）。

/// 抽象套接字地址名（不含起始 NUL，`from_abstract_name` 内部加）。
#[cfg(target_os = "linux")]
fn abstract_socket_name() -> Vec<u8> {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_owned());
    let user = sanitize_user(&user);
    format!("lumen-single-instance-{user}").into_bytes()
}

#[cfg(target_os = "linux")]
fn acquire_impl() -> InstanceCheck {
    use std::io::Write;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

    let name = abstract_socket_name();
    let addr = match SocketAddr::from_abstract_name(&name) {
        Ok(a) => a,
        Err(e) => {
            // 构造地址失败（超长等，不应发生）：放行启动，单实例是簿记约束。
            log::warn!("构造单实例抽象地址失败（{e}），放行本次启动");
            return InstanceCheck::MultiAllowed;
        }
    };
    match UnixListener::bind_addr(&addr) {
        Ok(listener) => InstanceCheck::Primary(PrimaryGuard { listener }),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // 已有实例：连上写一字节请求前台化，然后静默退出。连接失败也无妨
            //（对端可能正忙），单实例语义已达成。
            match UnixStream::connect_addr(&addr) {
                Ok(mut s) => {
                    let _ = s.write_all(b"raise");
                }
                Err(e) => log::warn!("已有实例在跑但通知其前台化失败（仅影响前台化）: {e}"),
            }
            InstanceCheck::AlreadyRunning
        }
        Err(e) => {
            // 其它 bind 失败（权限/资源）：放行启动，不挡主功能。
            log::warn!("绑定单实例套接字失败（{e}），放行本次启动");
            InstanceCheck::MultiAllowed
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_listener_impl(guard: &PrimaryGuard, proxy: EventLoopProxy<PtyWake>) {
    use std::io::Read;

    // clone 一份监听端交给后台线程 accept（原件随 guard 存活于主线程/AppState）。
    let listener = match guard.listener.try_clone() {
        Ok(l) => l,
        Err(e) => {
            log::warn!("clone 单实例监听端失败（仅影响前台化）: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("single-instance-fg".to_owned())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(mut stream) => {
                        // 收到任意连接即视为前台化请求（内容不解析，读空即可）。
                        let mut buf = [0u8; 8];
                        let _ = stream.read(&mut buf);
                        log::info!("收到第二实例的前台化信号");
                        FOREGROUND_REQUESTED.store(true, Ordering::Release);
                        // 借既有 PtyWake 唤醒主循环。Err = 事件循环已关闭，线程退出。
                        if proxy.send_event(PtyWake).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        log::warn!("单实例监听 accept 异常（忽略继续）: {e}");
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("前台化监听线程启动失败（仅影响前台化，不影响单实例）: {e}");
    }
}

// —— 其它 unix（macOS/BSD）：暂无单实例实现，放行多开（编译兜底）。 ——
// 抽象套接字是 Linux 专属；macOS 需走文件系统套接字 + 陈旧清理或 flock，
// 待后续在真机上补。

#[cfg(not(any(windows, target_os = "linux")))]
fn acquire_impl() -> InstanceCheck {
    log::debug!("本平台暂无单实例实现，放行多开");
    InstanceCheck::MultiAllowed
}

#[cfg(not(any(windows, target_os = "linux")))]
fn spawn_listener_impl(_guard: &PrimaryGuard, _proxy: EventLoopProxy<PtyWake>) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逃生口：环境变量 LUMEN_MULTI_INSTANCE 仅 `1` 放行。
    #[test]
    fn multi_flag_env() {
        assert!(multi_flag(std::iter::empty::<String>(), Some("1")));
        assert!(!multi_flag(std::iter::empty::<String>(), Some("0")));
        assert!(!multi_flag(std::iter::empty::<String>(), Some("true")));
        assert!(!multi_flag(std::iter::empty::<String>(), None));
    }

    /// 逃生口：--multi-instance 参数放行（其余参数/前缀不放行）。
    #[test]
    fn multi_flag_arg() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(multi_flag(args(&["--multi-instance"]), None));
        assert!(multi_flag(args(&["--foo", "--multi-instance"]), None));
        assert!(!multi_flag(args(&["--multi"]), None));
        assert!(!multi_flag(args(&["--multi-instance-x"]), None));
        assert!(!multi_flag(args(&[]), None));
    }

    /// UTF-16 名称缓冲以 0 结尾（Win32 宽字符串约定），长度 = 码元数 + 1。
    #[test]
    fn wide_null_terminated() {
        let w = wide("Lumen-单实例");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(w.len(), "Lumen-单实例".encode_utf16().count() + 1);
    }

    /// 用户名里的反斜杠被替换（内核对象名不允许命名空间外的 `\`）。
    #[test]
    fn sanitize_user_strips_backslash() {
        assert_eq!(sanitize_user(r"DOMAIN\user"), "DOMAIN_user");
        assert_eq!(sanitize_user("alice"), "alice");
    }
}
