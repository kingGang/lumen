//! unix：从系统读 shell 进程的**实时工作目录**（cwd 兜底）。
//!
//! Windows 靠 shell 集成注入的 OSC 9;9 上报 cwd；bash/zsh 目前无等价注入，
//! 文件树会卡在「等待 shell 上报路径」。此模块直接问操作系统要 shell 进程的
//! 当前目录——无需 shell 集成、且永远反映真实 cwd（cd 后由 sync_root 变化检测
//! 触发文件树重载，见 filetree::sync_root）。仅 unix 编译。

use std::path::PathBuf;

/// 读 pid 进程的当前工作目录。失败（进程已退 / 权限 / 平台不支持）返回 `None`。
#[cfg(target_os = "linux")]
pub fn shell_cwd(pid: u32) -> Option<PathBuf> {
    // /proc/<pid>/cwd 是指向进程 cwd 的符号链接，readlink 即得绝对路径。
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// macOS：`proc_pidinfo(PROC_PIDVNODEPATHINFO)` 取进程当前目录。
///
/// 填入 `struct proc_vnodepathinfo`（pvi_cdir + pvi_rdir，各为 vnode_info_path =
/// `{ vnode_info vip_vi; char vip_path[MAXPATHLEN] }`）。只取 pvi_cdir.vip_path
/// （当前目录）。偏移按 XNU `sys/proc_info.h` 在 64-bit 上精算：vinfo_stat=136B →
/// vnode_info=152B → vip_path 偏移 152；proc_vnodepathinfo 总长 2352B。用字节缓冲 +
/// 已知偏移取路径（不手写内部结构体，避免布局出错），并以「绝对路径必以 / 开头」
/// 兜底校验——偏移若算错取到非路径字节即返回 None（不崩、不误载）。
#[cfg(target_os = "macos")]
pub fn shell_cwd(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    const PROC_PIDVNODEPATHINFO: i32 = 9;
    const VNODE_INFO_SIZE: usize = 152; // sizeof(struct vnode_info)，64-bit macOS
    const MAXPATHLEN: usize = 1024;
    const VIP_PATH_OFF: usize = VNODE_INFO_SIZE; // pvi_cdir.vip_path 偏移
    const BUF_SIZE: usize = 2 * (VNODE_INFO_SIZE + MAXPATHLEN); // proc_vnodepathinfo = 2352

    extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, buffersize: i32) -> i32;
    }

    let mut buf = vec![0u8; BUF_SIZE];
    // SAFETY: buf 可写且容量 == buffersize；proc_pidinfo 按 flavor 填入定长结构。
    // 成功返回写入字节数（== BUF_SIZE），失败 <=0 或不足。
    let n = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr(),
            BUF_SIZE as i32,
        )
    };
    if n as usize != BUF_SIZE {
        return None;
    }
    // pvi_cdir.vip_path：偏移 VIP_PATH_OFF、最长 MAXPATHLEN、null 结尾 C 串。
    let path_bytes = &buf[VIP_PATH_OFF..VIP_PATH_OFF + MAXPATHLEN];
    let end = path_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(MAXPATHLEN);
    let path = &path_bytes[..end];
    // 兜底：绝对路径必以 '/' 开头；否则视为偏移/解析异常，弃用（不误载目录）。
    if path.first() != Some(&b'/') {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

/// 其它 unix（BSD 等）：暂无实现。
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn shell_cwd(_pid: u32) -> Option<PathBuf> {
    None
}
