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
        BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, RGBQUAD,
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
        let out = color_bitmap_to_rgba(ii.hbmColor, ii.hbmMask);
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

    fn color_bitmap_to_rgba(hbm: HBITMAP, hbm_mask: HBITMAP) -> Option<super::IconRgba> {
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
        // BGRA → RGBA。
        let any_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        // alpha 全 0 = 无 alpha 通道的旧式图标：透明度不在彩色位图里、而在配套的
        // 1-bit AND 掩码里。早年实现丢掉掩码、整张强制不透明——白背景的图标就被
        // 涂成不透明白方块（=用户看到的「白框」）。改用掩码逐像素恢复透明度；掩码
        // 读取失败才回退「整张不透明」（老行为，至少不崩、不透明总比丢图好）。
        if !any_alpha {
            if let Some(alpha) = mask_to_alpha(hbm_mask, w, h) {
                for (px, a) in buf.chunks_exact_mut(4).zip(alpha) {
                    px[3] = a;
                }
            } else {
                for px in buf.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
        }
        Some(super::IconRgba {
            width: w,
            height: h,
            rgba: buf,
        })
    }

    /// 读 1-bit AND 掩码位图 → 每像素 alpha（掩码 bit=0 不透明→255，bit=1
    /// 透明→0）。彩色位图无 alpha 通道（旧式图标）时用它恢复透明度。任何一步
    /// 失败返回 `None`（调用方回退「整张不透明」）。
    fn mask_to_alpha(hbm_mask: HBITMAP, w: u32, h: u32) -> Option<Vec<u8>> {
        if hbm_mask.is_null() {
            return None;
        }
        // 1bpp DIB 每行按 4 字节（DWORD）对齐。
        let stride = (w as usize).div_ceil(32) * 4;
        let mut buf = vec![0u8; stride * h as usize];
        // 1bpp BITMAPINFO 需 2 项调色板；windows_sys 的 `BITMAPINFO` 只含 1 项，
        // 手动拼一个 header + 2×RGBQUAD 的 POD 结构喂给 GetDIBits。
        #[repr(C)]
        struct BitmapInfo1 {
            header: BITMAPINFOHEADER,
            colors: [RGBQUAD; 2],
        }
        // SAFETY: 全零 POD 合法。
        let mut bi: BitmapInfo1 = unsafe { std::mem::zeroed() };
        bi.header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.header.biWidth = w as i32;
        bi.header.biHeight = -(h as i32); // top-down
        bi.header.biPlanes = 1;
        bi.header.biBitCount = 1;
        bi.header.biCompression = 0; // BI_RGB
        // SAFETY: CreateCompatibleDC(null)=内存 DC，失败返回 null。
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if dc.is_null() {
            return None;
        }
        // SAFETY: dc 有效；hbm_mask 未选入该 DC；buf 容量=stride*h 与 bi（1bpp、
        // top-down）描述一致；bi 指向 header + 2 调色板，满足 1bpp GetDIBits 要求。
        let lines = unsafe {
            GetDIBits(
                dc,
                hbm_mask,
                0,
                h,
                buf.as_mut_ptr().cast(),
                (&mut bi as *mut BitmapInfo1).cast(),
                DIB_RGB_COLORS,
            )
        };
        // SAFETY: dc 由 CreateCompatibleDC 创建，删除一次。
        unsafe { DeleteDC(dc) };
        if lines == 0 {
            return None;
        }
        // AND 掩码：bit=1 透明、bit=0 不透明；每字节高位对应最左像素。
        let mut alpha = vec![0u8; (w * h) as usize];
        for y in 0..h as usize {
            let row = &buf[y * stride..];
            for x in 0..w as usize {
                let bit = (row[x >> 3] >> (7 - (x & 7))) & 1;
                alpha[y * w as usize + x] = if bit == 0 { 255 } else { 0 };
            }
        }
        Some(alpha)
    }
}

/// 从 `/proc/<pid>/stat` 内容解析父进程 PID（PPid）。stat 第 4 字段是 ppid，但第 2
/// 字段 comm 用括号包裹且可能含空格/括号（如 `(Web Content)`）——故从**最后一个** `)`
/// 之后再按空白切分：其后依次是 state、ppid。解析失败返回 `None`。全平台编译（Linux 用 +
/// 三平台单测）。
#[cfg(any(target_os = "linux", test))]
fn parse_ppid_from_stat(stat: &str) -> Option<u32> {
    let after = stat.rsplit_once(')')?.1;
    let mut it = after.split_whitespace();
    let _state = it.next()?;
    it.next()?.parse().ok()
}

// ── Linux：/proc 找前台子进程 + freedesktop 图标主题 ───────────────────────────
#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::parse_ppid_from_stat;

    pub fn foreground_exe(shell_pid: u32) -> Option<PathBuf> {
        let pid = foreground_pid(shell_pid).unwrap_or(shell_pid);
        exe_path(pid)
    }

    /// 扫 `/proc` 找 `shell_pid` 的直接子进程（前台程序）；多个取 PID 最大者（近似最近
    /// 创建）。无子进程返回 `None`（停在提示符 → 回落 shell 自身）。
    fn foreground_pid(shell_pid: u32) -> Option<u32> {
        let mut best: Option<u32> = None;
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue; // 非数字目录（/proc/self、/proc/cpuinfo 等）跳过
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue; // 进程可能刚退出
            };
            if parse_ppid_from_stat(&stat) == Some(shell_pid) {
                best = Some(best.map_or(pid, |b| b.max(pid)));
            }
        }
        best
    }

    /// `readlink /proc/<pid>/exe` 得可执行绝对路径。权限不足 / 进程已退出返回 `None`。
    fn exe_path(pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    pub fn load_icon_rgba(exe: &Path) -> Option<super::IconRgba> {
        // 可执行名（去扩展名）近似图标名：GUI 程序（firefox/code…）多能命中 freedesktop
        // 图标主题；shell（bash/zsh/pwsh）通常无同名图标 → None → 上层回退自绘字形。
        let name = exe.file_stem()?.to_str()?;
        let icon = freedesktop_icons::lookup(name)
            .with_size(48)
            .with_cache()
            .find()?;
        // 仅解 PNG（image 已是依赖）；SVG 等矢量图暂不支持（需额外渲染器）→ None。
        if !icon
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"))
        {
            return None;
        }
        let img = image::open(&icon).ok()?.to_rgba8();
        Some(super::IconRgba {
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
        })
    }
}

// ── macOS：libproc 找前台子进程 + NSWorkspace 图标 → NSBitmapImageRep → RGBA ──
#[cfg(target_os = "macos")]
mod imp {
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSWorkspace};
    use objc2_foundation::NSString;

    // libproc（macOS libSystem 内，默认链接）：查子进程 / 取进程可执行路径。
    extern "C" {
        fn proc_listchildpids(ppid: i32, buffer: *mut i32, buffersize: i32) -> i32;
        fn proc_pidpath(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
    }

    pub fn foreground_exe(shell_pid: u32) -> Option<PathBuf> {
        let pid = foreground_pid(shell_pid).unwrap_or(shell_pid);
        exe_path(pid)
    }

    /// shell 的直接子进程（前台程序），取 PID 最大者（近似最近创建）。无子进程返回 None。
    fn foreground_pid(shell_pid: u32) -> Option<u32> {
        let mut buf = vec![0i32; 256];
        // SAFETY: buf 可写、buffersize 与其字节容量一致。返回填入的字节数（proc_list* 家族
        // 约定），<=0 视为无子进程/失败。
        let ret = unsafe {
            proc_listchildpids(shell_pid as i32, buf.as_mut_ptr(), (buf.len() * 4) as i32)
        };
        if ret <= 0 {
            return None;
        }
        let count = ((ret as usize) / 4).min(buf.len());
        buf[..count]
            .iter()
            .copied()
            .filter(|&p| p > 0)
            .map(|p| p as u32)
            .max()
    }

    /// proc_pidpath 取进程可执行绝对路径。失败返回 None。
    fn exe_path(pid: u32) -> Option<PathBuf> {
        let mut buf = vec![0u8; 4096];
        // SAFETY: buf 可写、buffersize 与其容量一致。返回写入的路径字节长度，<=0 为失败。
        let ret = unsafe { proc_pidpath(pid as i32, buf.as_mut_ptr(), buf.len() as u32) };
        if ret <= 0 {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(
            &buf[..ret as usize],
        )))
    }

    pub fn load_icon_rgba(exe: &Path) -> Option<super::IconRgba> {
        // autoreleasepool 包住：及时释放 TIFF NSData（较大）等 autoreleased 对象。
        objc2::rc::autoreleasepool(|_| {
            let path = NSString::from_str(exe.to_str()?);
            // NSWorkspace 对任意存在文件都返回图标（无专属图标则给通用可执行图标）。
            // SAFETY: 均为标准 AppKit 只读调用；NSWorkspace/NSImage 线程安全，本函数在 UI 线程调用。
            let tiff = unsafe {
                let ws = NSWorkspace::sharedWorkspace();
                let image = ws.iconForFile(&path);
                image.TIFFRepresentation()?
            };
            // TIFF → NSBitmapImageRep（alloc + initWithData；对 TIFF 数据构造位图 rep）。
            let rep = unsafe { NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &tiff) }?;
            bitmap_to_rgba(&rep)
        })
    }

    /// NSBitmapImageRep → top-down RGBA8。仅处理常见 32bpp / 4 通道 / 非平面；其余返回
    /// None（上层回退自绘字形）。按 bitmapFormat 处理 alpha 位置（首/尾）与预乘还原。
    fn bitmap_to_rgba(rep: &NSBitmapImageRep) -> Option<super::IconRgba> {
        // SAFETY: 以下均为 rep 的只读属性访问。
        let (planar, spp, bpp, w, h, stride, fmt) = unsafe {
            (
                rep.isPlanar(),
                rep.samplesPerPixel(),
                rep.bitsPerPixel(),
                rep.pixelsWide(),
                rep.pixelsHigh(),
                rep.bytesPerRow(),
                rep.bitmapFormat(),
            )
        };
        if planar || spp != 4 || bpp != 32 || w <= 0 || h <= 0 || w > 512 || h > 512 {
            return None;
        }
        let (w, h, stride) = (w as usize, h as usize, stride as usize);
        if stride < w * 4 {
            return None;
        }
        // SAFETY: bitmapData 返回 rep 位图缓冲首指针（rep 存活期内有效）；判空后按 stride/像素
        // 宽在 h*stride 界内逐像素取 4 字节（bpp==32 保证每像素 4 字节）。
        let data = unsafe { rep.bitmapData() };
        if data.is_null() {
            return None;
        }
        let alpha_first = fmt.contains(NSBitmapFormat::AlphaFirst);
        let premultiplied = !fmt.contains(NSBitmapFormat::AlphaNonpremultiplied);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                // SAFETY: 见上，y*stride + x*4 + 3 < h*stride，界内。
                let (r, g, b, a) = unsafe {
                    let px = data.add(y * stride + x * 4);
                    if alpha_first {
                        (*px.add(1), *px.add(2), *px.add(3), *px)
                    } else {
                        (*px, *px.add(1), *px.add(2), *px.add(3))
                    }
                };
                let (r, g, b) = if premultiplied && a != 0 {
                    let un = |c: u8| (((c as u32) * 255 + (a as u32) / 2) / a as u32).min(255) as u8;
                    (un(r), un(g), un(b))
                } else {
                    (r, g, b)
                };
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
        Some(super::IconRgba {
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}

// ── 其余非 Windows/Linux/macOS（BSD 等）：暂空实现（编译兜底）───────────────────
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn foreground_exe(_shell_pid: u32) -> Option<PathBuf> {
        None
    }

    pub fn load_icon_rgba(_exe: &Path) -> Option<super::IconRgba> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ppid_from_stat;

    #[test]
    fn ppid解析_常规() {
        let stat = "1234 (bash) S 1000 1234 1234 34816 1234 4194304 123 0 0 0 1 2";
        assert_eq!(parse_ppid_from_stat(stat), Some(1000));
    }

    #[test]
    fn ppid解析_comm含空格和右括号() {
        // 进程名含空格与右括号（真实存在，如 Firefox 的 "(Web Content)"）：须按最后一个
        // ')' 切分才不被 comm 里的括号误导。
        let stat = "42 (weird )( name) R 7 42 42 0 -1";
        assert_eq!(parse_ppid_from_stat(stat), Some(7));
    }

    #[test]
    fn ppid解析_残缺返回none() {
        assert_eq!(parse_ppid_from_stat("无括号的垃圾串"), None);
        assert_eq!(parse_ppid_from_stat("123 (x) S"), None); // 缺 ppid 字段
    }
}
