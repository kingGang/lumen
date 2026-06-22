//! Windows 系统文件剪贴板（`CF_HDROP`）读写：让文件树的复制 / 粘贴与资源管理器及任意应用
//! 互通（海风哥反馈：原内部剪贴板与系统隔离——Lumen 复制的文件在资源管理器粘贴不到，反之亦然）。
//!
//! - [`copy_files`]：把若干本地路径写入剪贴板 `CF_HDROP`（`DROPFILES` 头 + 双 `\0` 结尾的宽字符
//!   路径列表）。写成功后**系统接管**那块全局内存，不可再 `GlobalFree`。
//! - [`paste_files`]：读剪贴板 `CF_HDROP`，用 `DragQueryFileW` 枚举出路径列表。
//! - [`has_files`]：剪贴板当前是否有文件（`IsClipboardFormatAvailable(CF_HDROP)`），驱动「粘贴」
//!   菜单是否出现。
//!
//! 非 Windows 平台为返回空 / `false` 的桩（本项目仅 Windows，跨平台仅保证编译通过）。

#[cfg(windows)]
pub use imp::{copy_files, has_files, paste_files};

#[cfg(not(windows))]
use std::path::PathBuf;

#[cfg(not(windows))]
pub fn copy_files(_paths: &[PathBuf]) -> bool {
    false
}
#[cfg(not(windows))]
pub fn paste_files() -> Vec<PathBuf> {
    Vec::new()
}
#[cfg(not(windows))]
pub fn has_files() -> bool {
    false
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, POINT};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows_sys::Win32::System::Ole::CF_HDROP;
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};

    /// 把本地路径列表写入系统剪贴板的 `CF_HDROP` 格式（资源管理器「复制」用的同一格式）。
    /// 返回是否成功。空列表直接返回 `false`。
    pub fn copy_files(paths: &[PathBuf]) -> bool {
        if paths.is_empty() {
            return false;
        }
        // 宽字符路径列表：每个路径以 `\0` 结尾，整个列表末尾再追加一个 `\0`（双 null 结尾）。
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.as_os_str().encode_wide());
            wide.push(0);
        }
        wide.push(0);

        let header = core::mem::size_of::<DROPFILES>();
        let bytes = header + wide.len() * core::mem::size_of::<u16>();

        // SAFETY: 全套 Win32 全局内存 + 剪贴板调用。下方对每个返回值判空 / 判错并在失败路径
        // GlobalFree 回收；成功 SetClipboardData 后系统接管内存，故不再 free（注释标注）。
        unsafe {
            let hmem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes);
            if hmem.is_null() {
                return false;
            }
            let base = GlobalLock(hmem);
            if base.is_null() {
                GlobalFree(hmem);
                return false;
            }
            // DROPFILES 头：pFiles = 路径列表相对块首的字节偏移（= 头大小）；fWide=1 表示宽字符。
            let df = base.cast::<DROPFILES>();
            (*df).pFiles = header as u32;
            (*df).pt = POINT { x: 0, y: 0 };
            (*df).fNC = 0;
            (*df).fWide = 1;
            // 路径列表紧跟在 DROPFILES 头之后。
            let dst = base.cast::<u8>().add(header).cast::<u16>();
            core::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(hmem);

            if OpenClipboard(core::ptr::null_mut()) == 0 {
                GlobalFree(hmem);
                return false;
            }
            EmptyClipboard();
            let set = SetClipboardData(u32::from(CF_HDROP), hmem as HANDLE);
            CloseClipboard();
            if set.is_null() {
                // SetClipboardData 失败：内存仍归我们，回收。
                GlobalFree(hmem);
                return false;
            }
            // 成功：系统已接管 hmem，绝不能再 GlobalFree（否则剪贴板访问悬垂内存）。
            true
        }
    }

    /// 剪贴板当前是否有 `CF_HDROP`（文件）。驱动「粘贴到此目录」菜单是否出现。
    #[must_use]
    pub fn has_files() -> bool {
        // SAFETY: 纯查询，不打开剪贴板、不分配，无资源需释放。
        unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP)) != 0 }
    }

    /// 读系统剪贴板 `CF_HDROP`，枚举其中的文件路径（资源管理器 / 任意应用「复制」的文件）。
    /// 无文件或读取失败返回空列表。
    #[must_use]
    pub fn paste_files() -> Vec<PathBuf> {
        let mut out = Vec::new();
        // SAFETY: GetClipboardData 返回的句柄归系统所有（不 free、不 DragFinish）；仅用
        // DragQueryFileW 只读枚举。OpenClipboard 成功后保证 CloseClipboard 配对。
        unsafe {
            if IsClipboardFormatAvailable(u32::from(CF_HDROP)) == 0 {
                return out;
            }
            if OpenClipboard(core::ptr::null_mut()) == 0 {
                return out;
            }
            let h = GetClipboardData(u32::from(CF_HDROP));
            if !h.is_null() {
                let hdrop = h as HDROP;
                // ifile = 0xFFFF_FFFF 时返回文件总数。
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, core::ptr::null_mut(), 0);
                for i in 0..count {
                    // 先查长度（不含结尾 `\0`），再取内容。
                    let len = DragQueryFileW(hdrop, i, core::ptr::null_mut(), 0);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32);
                    if got > 0 {
                        out.push(PathBuf::from(OsString::from_wide(&buf[..got as usize])));
                    }
                }
            }
            CloseClipboard();
        }
        out
    }
}
