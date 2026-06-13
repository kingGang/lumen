//! F7②：侧栏会话图标 = 会话内**前台运行程序的 exe 图标**。
//!
//! 规则（海风哥 2026-06-13）：图标不可自定义、读取会话当前运行程序的
//! 图标；停在提示符（无运行程序）时前台就是 shell 本身，显示的即「命令
//! 行图标」（pwsh/cmd 的图标）。
//!
//! 分层：
//! - [`foreground_exe`]：从 shell 子进程 PID 出发，查其**直接子进程**
//!   （前台运行的程序）的 exe 完整路径；无子进程则回落 shell 自身的
//!   exe（=命令行图标）。纯进程快照查询，无窗口/GPU 依赖。
//! - [`load_icon_rgba`]：用系统外壳 API 抽取 exe 关联图标，转成
//!   top-down RGBA8 像素（供上层上传为 egui 纹理；按 exe 路径缓存，
//!   不必每帧抽取）。
//!
//! 全部失败路径返回 `None`，上层据此回退到自绘终端字形——绝不 panic、
//! 不阻塞。非 Windows 平台为空实现（恒 `None`）。

use std::path::{Path, PathBuf};

/// 抽取出的图标像素（top-down，每像素 RGBA8，`rgba.len() == w*h*4`）。
pub struct IconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 会话前台运行程序的 exe 完整路径：shell（`shell_pid`）的直接子进程
/// （跑命令时如 `cargo run` 的 cargo.exe）；无子进程时回落 shell 自身
/// （停在提示符=命令行图标）。查不到返回 `None`。
pub fn foreground_exe(shell_pid: u32) -> Option<PathBuf> {
    imp::foreground_exe(shell_pid)
}

/// 抽取 `exe` 关联图标为 RGBA8。失败返回 `None`（上层回退自绘字形）。
pub fn load_icon_rgba(exe: &Path) -> Option<IconRgba> {
    imp::load_icon_rgba(exe)
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    pub fn foreground_exe(shell_pid: u32) -> Option<PathBuf> {
        let pid = foreground_pid(shell_pid).unwrap_or(shell_pid);
        exe_path(pid)
    }

    /// 进程快照里找 `shell_pid` 的直接子进程（前台程序）。多个时取 PID
    /// 最大者（近似最近创建）。无子进程返回 `None`（停在提示符）。
    fn foreground_pid(shell_pid: u32) -> Option<u32> {
        // SAFETY: CreateToolhelp32Snapshot 返回有效句柄或 INVALID_HANDLE_VALUE，
        // 失败即不遍历。
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return None;
        }
        let mut best: Option<u32> = None;
        // SAFETY: PROCESSENTRY32W 为纯 POD，全零初始化合法；dwSize 必须先置。
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: snap 有效，entry 可写且 dwSize 已置。
        let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
        while ok != 0 {
            if entry.th32ParentProcessID == shell_pid {
                let pid = entry.th32ProcessID;
                best = Some(best.map_or(pid, |b| b.max(pid)));
            }
            // SAFETY: snap 有效，entry 可写。
            ok = unsafe { Process32NextW(snap, &mut entry) };
        }
        // SAFETY: snap 是有效句柄，关闭一次。
        unsafe { CloseHandle(snap) };
        best
    }

    /// 取进程 exe 完整路径（PROCESS_QUERY_LIMITED_INFORMATION 权限对多数
    /// 进程足够、无需提权）。失败返回 `None`。
    fn exe_path(pid: u32) -> Option<PathBuf> {
        // SAFETY: OpenProcess 失败返回 null，下面判空。
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: handle 有效；buf/len 指向有效缓冲；成功时写入路径并回填长度。
        let ok =
            unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        // SAFETY: handle 有效，关闭一次。
        unsafe { CloseHandle(handle) };
        if ok == 0 || len == 0 {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_wide(
            &buf[..len as usize],
        )))
    }

    pub fn load_icon_rgba(exe: &Path) -> Option<super::IconRgba> {
        let hicon = file_icon(exe)?;
        let out = icon_to_rgba(hicon);
        // SAFETY: hicon 来自 SHGetFileInfoW（SHGFI_ICON），用完销毁一次。
        unsafe { DestroyIcon(hicon) };
        out
    }

    /// 取 exe 的关联大图标（32×32）。
    fn file_icon(exe: &Path) -> Option<HICON> {
        let wide: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: SHFILEINFOW 为 POD，全零初始化合法。
        let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
        // SAFETY: wide 以 NUL 结尾；info 可写、大小正确。
        let r = unsafe {
            SHGetFileInfoW(
                wide.as_ptr(),
                0,
                &mut info,
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if r == 0 || info.hIcon.is_null() {
            return None;
        }
        Some(info.hIcon)
    }

    /// HICON → RGBA8。取彩色位图的像素（BGRA→RGBA），alpha 全 0 的旧图标
    /// 视为不透明。
    fn icon_to_rgba(hicon: HICON) -> Option<super::IconRgba> {
        // SAFETY: ICONINFO 为 POD，全零初始化合法。
        let mut ii: ICONINFO = unsafe { std::mem::zeroed() };
        // SAFETY: hicon 有效；ii 可写。成功时 hbmColor/hbmMask 为需调用方
        // DeleteObject 的 HBITMAP。
        if unsafe { GetIconInfo(hicon, &mut ii) } == 0 {
            return None;
        }
        let out = color_bitmap_to_rgba(ii.hbmColor);
        // SAFETY: hbmColor/hbmMask 是 GetIconInfo 产出的 HBITMAP，各删一次。
        unsafe {
            if !ii.hbmColor.is_null() {
                DeleteObject(ii.hbmColor);
            }
            if !ii.hbmMask.is_null() {
                DeleteObject(ii.hbmMask);
            }
        }
        out
    }

    fn color_bitmap_to_rgba(hbm: HBITMAP) -> Option<super::IconRgba> {
        if hbm.is_null() {
            return None;
        }
        // SAFETY: BITMAP 为 POD，全零初始化合法。
        let mut bm: BITMAP = unsafe { std::mem::zeroed() };
        // SAFETY: hbm 有效；GetObjectW 按大小写 BITMAP 结构。
        let n = unsafe {
            GetObjectW(
                hbm,
                std::mem::size_of::<BITMAP>() as i32,
                (&mut bm as *mut BITMAP).cast(),
            )
        };
        if n == 0 {
            return None;
        }
        let w = bm.bmWidth.max(0) as u32;
        let h = bm.bmHeight.max(0) as u32;
        // 尺寸防御：异常/超大图标直接放弃（回退字形）。
        if w == 0 || h == 0 || w > 512 || h > 512 {
            return None;
        }
        // 32bpp、top-down（biHeight 负）、不压缩的 DIB 描述。
        // SAFETY: BITMAPINFO 为 POD，全零初始化合法。
        let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(h as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = 0; // BI_RGB（不压缩）
        let mut buf = vec![0u8; (w * h * 4) as usize];
        // SAFETY: CreateCompatibleDC(null)=内存 DC，失败返回 null。
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if dc.is_null() {
            return None;
        }
        // SAFETY: dc 为有效内存 DC；hbm 未选入该 DC；buf 容量 w*h*4 与 bi
        // 描述一致；GetDIBits 据此写入像素。
        let lines = unsafe {
            GetDIBits(
                dc,
                hbm,
                0,
                h,
                buf.as_mut_ptr().cast(),
                &mut bi,
                DIB_RGB_COLORS,
            )
        };
        // SAFETY: dc 由 CreateCompatibleDC 创建，删除一次。
        unsafe { DeleteDC(dc) };
        if lines == 0 {
            return None;
        }
        // BGRA → RGBA；alpha 全 0（无 alpha 通道的旧图标）则视为不透明。
        let any_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            if !any_alpha {
                px[3] = 255;
            }
        }
        Some(super::IconRgba {
            width: w,
            height: h,
            rgba: buf,
        })
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn foreground_exe(_shell_pid: u32) -> Option<PathBuf> {
        None
    }

    pub fn load_icon_rgba(_exe: &Path) -> Option<super::IconRgba> {
        None
    }
}
