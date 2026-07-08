//! Windows 系统文件剪贴板（`CF_HDROP`）读写：让文件树的复制 / 粘贴与资源管理器及任意应用
//! 互通（海风哥反馈：原内部剪贴板与系统隔离——Lumen 复制的文件在资源管理器粘贴不到，反之亦然）。
//!
//! - [`copy_files`]：把若干本地路径写入剪贴板 `CF_HDROP`（`DROPFILES` 头 + 双 `\0` 结尾的宽字符
//!   路径列表）。写成功后**系统接管**那块全局内存，不可再 `GlobalFree`。
//! - [`paste_files`]：读剪贴板 `CF_HDROP`，用 `DragQueryFileW` 枚举出路径列表。
//! - [`has_files`]：剪贴板当前是否有文件（`IsClipboardFormatAvailable(CF_HDROP)`），驱动「粘贴」
//!   菜单是否出现。
//!
//! Linux 经 [`linux`] 模块用 `text/uri-list` 与文件管理器互通（X11 走 x11-clipboard，
//! 进程内常驻 selection owner 持续响应粘贴请求；Wayland 走 wl-clipboard-rs）。URI 编解码
//! 纯逻辑收在 [`uri`] 模块（全平台编译 + 单测）。其余非 Windows（macOS/BSD）暂为返回空 /
//! `false` 的桩（编译通过、功能待补）。

#[cfg(windows)]
pub use imp::{copy_files, has_files, paste_files};

#[cfg(target_os = "linux")]
pub use linux::{copy_files, has_files, paste_files};

#[cfg(target_os = "macos")]
pub use macos::{copy_files, has_files, paste_files};

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub use stub_other::{copy_files, has_files, paste_files};

// ── 其它非 Windows/Linux/macOS（BSD 等）：编译占位（文件剪贴板未实现）────────
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod stub_other {
    use std::path::PathBuf;

    pub fn copy_files(_paths: &[PathBuf]) -> bool {
        false
    }
    pub fn paste_files() -> Vec<PathBuf> {
        Vec::new()
    }
    pub fn has_files() -> bool {
        false
    }
}

// ── text/uri-list 编解码（纯逻辑，字节导向，无 X11/Wayland 依赖）───────────────
// cfg(any(linux, test))：Linux 实际使用 + 三平台单测覆盖；其它平台非测试构建不编译
// （避免 dead_code / 无谓拉 percent-encoding）。字节导向（&[u8]）而非 &str，以便在
// Windows 上也能单测（不触碰 unix 专属 OsStrExt），且忠实保留 Linux 非 UTF-8 路径。
#[cfg(any(target_os = "linux", test))]
mod uri {
    use percent_encoding::{percent_decode, percent_encode, AsciiSet, CONTROLS};

    /// file:// URI path 中需 percent 转义的 ASCII 字符集。保留 `/`（路径分隔）、unreserved
    /// （字母数字 `-._~`）及 URI path 合法的 sub-delims/`:@`；其余按 RFC 3986 转义。非 ASCII
    /// 字节由 `percent_encode` 自动转义（UTF-8 逐字节）。
    const URI_PATH: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'|')
        .add(b'\\')
        .add(b'^')
        .add(b'[')
        .add(b']');

    /// 把若干**绝对**路径（字节）拼成 `text/uri-list`（RFC 2483：每行一个 URI，CRLF 分隔）。
    /// 非绝对路径（不以 `/` 开头）跳过——`file://` 需绝对路径，相对项无意义。
    pub fn build_uri_list(paths: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for &p in paths {
            if p.first() != Some(&b'/') {
                continue;
            }
            out.extend_from_slice(b"file://");
            for chunk in percent_encode(p, URI_PATH) {
                out.extend_from_slice(chunk.as_bytes());
            }
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// 解析 `text/uri-list`，取出其中 `file://` 项的本地绝对路径（字节）。忽略注释行
    /// （`#` 开头，RFC 2483）、空行、非 file 协议项；`file://host/path` 的 host 段跳过。
    pub fn parse_uri_list(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for line in data.split(|&b| b == b'\n') {
            let line = line.trim_ascii(); // 去 \r 与两端空白
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let Some(rest) = strip_file_scheme(line) else {
                continue;
            };
            // rest 以 '/' 开头 = host 空（file:///path）；否则跳过 host 段到第一个 '/'。
            let path = if rest.first() == Some(&b'/') {
                rest
            } else {
                match rest.iter().position(|&b| b == b'/') {
                    Some(i) => &rest[i..],
                    None => continue,
                }
            };
            let decoded: Vec<u8> = percent_decode(path).collect();
            if !decoded.is_empty() {
                out.push(decoded);
            }
        }
        out
    }

    /// 若 `line` 以 `file://`（大小写不敏感）开头，返回其后剩余字节；否则 `None`。
    fn strip_file_scheme(line: &[u8]) -> Option<&[u8]> {
        const PREFIX: &[u8] = b"file://";
        let head = line.get(..PREFIX.len())?;
        head.eq_ignore_ascii_case(PREFIX)
            .then(|| &line[PREFIX.len()..])
    }
}

// ── Linux 文件剪贴板：X11（x11-clipboard）+ Wayland（wl-clipboard-rs）───────────
#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use super::uri;

    /// 与文件管理器互通的 MIME 类型（Qt 系如 Deepin dde-file-manager / Dolphin、多数
    /// GTK 文件管理器均认）。
    const URI_LIST_MIME: &str = "text/uri-list";
    /// `has_files` 结果缓存 TTL：该函数在文件树 UI 每帧调用，避免每帧一次 X11/Wayland 往返。
    const HAS_CACHE_TTL: Duration = Duration::from_millis(300);
    /// 读剪贴板 / 查 TARGETS 的超时：无属主时 X server 立即回 None，此值仅防僵死属主挂死 UI。
    const LOAD_TIMEOUT: Duration = Duration::from_millis(300);

    /// 当前是否 Wayland 会话（否则按 X11 处理，含 XWayland 下的 X11 应用）。
    fn is_wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
    }

    // ---------- 对外 3 个 API（与 Windows imp 同签名）----------

    /// 复制本地路径到系统剪贴板（`text/uri-list`），让文件管理器可粘贴。空列表 / 全非绝对
    /// 路径 / 后端不可用返回 `false`。
    pub fn copy_files(paths: &[PathBuf]) -> bool {
        if paths.is_empty() {
            return false;
        }
        let byte_paths: Vec<&[u8]> = paths.iter().map(|p| p.as_os_str().as_bytes()).collect();
        let data = uri::build_uri_list(&byte_paths);
        if data.is_empty() {
            return false; // 全部非绝对路径
        }
        let ok = if is_wayland() {
            wayland::copy(data)
        } else {
            x11::copy(data)
        };
        if ok {
            set_has_cache(true); // 刚成为属主，has_files 立即为真（省一次往返）
        }
        ok
    }

    /// 读系统剪贴板中的文件（`text/uri-list` → 本地路径）。无文件 / 读取失败返回空列表。
    pub fn paste_files() -> Vec<PathBuf> {
        let data = if is_wayland() {
            wayland::paste()
        } else {
            x11::paste()
        };
        uri::parse_uri_list(&data)
            .into_iter()
            .map(|b| PathBuf::from(std::ffi::OsString::from_vec(b)))
            .collect()
    }

    /// 系统剪贴板当前是否有文件（是否 offer `text/uri-list`）。驱动「粘贴」菜单是否出现；
    /// 每帧调用，故 300ms 缓存避免频繁往返。
    pub fn has_files() -> bool {
        if let Some(v) = get_has_cache() {
            return v;
        }
        let v = if is_wayland() {
            wayland::has_files()
        } else {
            x11::has_files()
        };
        set_has_cache(v);
        v
    }

    // ---------- has_files 缓存 ----------

    fn has_cache() -> &'static Mutex<Option<(Instant, bool)>> {
        static CACHE: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(None))
    }

    fn get_has_cache() -> Option<bool> {
        let guard = has_cache().lock().ok()?;
        match *guard {
            Some((t, v)) if t.elapsed() < HAS_CACHE_TTL => Some(v),
            _ => None,
        }
    }

    fn set_has_cache(v: bool) {
        if let Ok(mut guard) = has_cache().lock() {
            *guard = Some((Instant::now(), v));
        }
    }

    // ---------- X11 后端 ----------
    mod x11 {
        use super::{LOAD_TIMEOUT, URI_LIST_MIME};
        use std::sync::OnceLock;
        use x11_clipboard::Clipboard;

        /// 进程内常驻的剪贴板属主。x11-clipboard 的 worker 线程随 `Clipboard` 存活而持续响应
        /// SelectionRequest（复制的 uri-list 一直可被粘贴），故存为静态、全生命周期存活。
        struct State {
            clip: Clipboard,
            /// CLIPBOARD selection atom。
            selection: u32,
            /// `text/uri-list` target atom（intern 得，X server 全局）。
            uri_list: u32,
            /// TARGETS atom（查剪贴板 offer 了哪些格式）。
            targets: u32,
            /// 读取结果落地属性 atom。
            property: u32,
        }

        fn state() -> Option<&'static State> {
            static STATE: OnceLock<Option<State>> = OnceLock::new();
            STATE
                .get_or_init(|| {
                    let clip = Clipboard::new().ok()?;
                    let uri_list = clip.setter.get_atom(URI_LIST_MIME).ok()?;
                    let selection = clip.setter.atoms.clipboard;
                    let targets = clip.setter.atoms.targets;
                    let property = clip.getter.atoms.property;
                    Some(State {
                        clip,
                        selection,
                        uri_list,
                        targets,
                        property,
                    })
                })
                .as_ref()
        }

        pub fn copy(data: Vec<u8>) -> bool {
            match state() {
                Some(s) => s.clip.store(s.selection, s.uri_list, data).is_ok(),
                None => false,
            }
        }

        pub fn paste() -> Vec<u8> {
            match state() {
                Some(s) => s
                    .clip
                    .load(s.selection, s.uri_list, s.property, LOAD_TIMEOUT)
                    .unwrap_or_default(),
                None => Vec::new(),
            }
        }

        pub fn has_files() -> bool {
            let Some(s) = state() else {
                return false;
            };
            // 查 TARGETS：返回 ATOM 数组（每 4 字节一个 u32），含 uri-list atom 即有文件。
            match s.clip.load(s.selection, s.targets, s.property, LOAD_TIMEOUT) {
                Ok(data) => data
                    .chunks_exact(4)
                    .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == s.uri_list),
                Err(_) => false,
            }
        }
    }

    // ---------- Wayland 后端 ----------
    mod wayland {
        use super::URI_LIST_MIME;
        use std::io::Read;

        pub fn copy(data: Vec<u8>) -> bool {
            use wl_clipboard_rs::copy::{MimeType, Options, Source};
            // 默认后台线程 serve，非阻塞，持有 selection 直到被别处复制替换。
            Options::new()
                .copy(
                    Source::Bytes(data.into_boxed_slice()),
                    MimeType::Specific(URI_LIST_MIME.to_owned()),
                )
                .is_ok()
        }

        pub fn paste() -> Vec<u8> {
            use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};
            match get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                MimeType::Specific(URI_LIST_MIME),
            ) {
                Ok((mut pipe, _mime)) => {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    buf
                }
                // NoSeats / ClipboardEmpty / NoMimeType 等价空剪贴板。
                Err(_) => Vec::new(),
            }
        }

        pub fn has_files() -> bool {
            use wl_clipboard_rs::paste::{get_mime_types, ClipboardType, Seat};
            match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
                Ok(types) => types.contains(URI_LIST_MIME),
                Err(_) => false,
            }
        }
    }
}

// ── macOS 文件剪贴板：NSPasteboard 的文件 URL（public.file-url），与 Finder 互通 ──
// 参照 arboard 3.6.1 的 objc2 0.6 惯用法。NSPasteboard 线程安全，可从 UI 线程调用。
#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{msg_send, ClassType};
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    /// 通用剪贴板。launchd 守护态等边缘情况可能取不到 → `None`（用 msg_send 承接可空返回）。
    fn pasteboard() -> Option<Retained<NSPasteboard>> {
        unsafe { msg_send![NSPasteboard::class(), generalPasteboard] }
    }

    /// 把本地路径写成剪贴板的文件 URL（Finder「粘贴」用的同一格式）。空 / 后端不可用返回 `false`。
    pub fn copy_files(paths: &[PathBuf]) -> bool {
        let Some(pb) = pasteboard() else {
            return false;
        };
        let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
            .iter()
            .filter_map(|p| p.to_str())
            .map(|s| {
                let ns = NSString::from_str(s);
                let url = unsafe { NSURL::fileURLWithPath(&ns) };
                ProtocolObject::from_retained(url)
            })
            .collect();
        if urls.is_empty() {
            return false;
        }
        let array = NSArray::from_retained_slice(&urls);
        unsafe { pb.clearContents() };
        unsafe { pb.writeObjects(&array) }
    }

    /// 读剪贴板里的文件（NSURL → 本地路径），仅取文件 URL（排除 http 等）。无 / 失败返回空列表。
    /// 用 autoreleasepool 包住：本函数（经 has_files）可能被 UI 高频调用，及时释放
    /// readObjectsForClasses 产生的 NSArray/NSURL 等 autoreleased 对象，避免累积涨内存。
    pub fn paste_files() -> Vec<PathBuf> {
        objc2::rc::autoreleasepool(|_| {
            let Some(pb) = pasteboard() else {
                return Vec::new();
            };
            let class_array = NSArray::from_slice(&[NSURL::class()]);
            // 不传 fileURLsOnly 选项（省去 NSDictionary 类型匹配），改用 isFileURL 过滤。
            let objects = unsafe { pb.readObjectsForClasses_options(&class_array, None) };
            objects
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|obj| {
                            let url = obj.downcast::<NSURL>().ok()?;
                            if unsafe { url.isFileURL() } {
                                unsafe { url.path() }.map(|p| PathBuf::from(p.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// 剪贴板是否有文件。文件树 UI 每帧调用（TUI 忙时更是 ~30fps 连续渲染）→ 300ms
    /// 缓存，避免每帧读 NSPasteboard（频繁读累积 objc 临时对象 + 占 CPU）。
    pub fn has_files() -> bool {
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant};
        fn cache() -> &'static Mutex<Option<(Instant, bool)>> {
            static C: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
            C.get_or_init(|| Mutex::new(None))
        }
        if let Ok(g) = cache().lock() {
            if let Some((t, v)) = *g {
                if t.elapsed() < Duration::from_millis(300) {
                    return v;
                }
            }
        }
        let v = !paste_files().is_empty();
        if let Ok(mut g) = cache().lock() {
            *g = Some((Instant::now(), v));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::uri::{build_uri_list, parse_uri_list};

    #[test]
    fn 构建_基本两项() {
        let paths: [&[u8]; 2] = [b"/home/kk/a.txt", b"/tmp/b"];
        assert_eq!(
            build_uri_list(&paths),
            b"file:///home/kk/a.txt\r\nfile:///tmp/b\r\n"
        );
    }

    #[test]
    fn 构建_空格与中文转义() {
        let p: [&[u8]; 1] = ["/home/kk/我的 文件.txt".as_bytes()];
        let text = String::from_utf8(build_uri_list(&p)).unwrap();
        assert!(text.starts_with("file:///home/kk/"));
        assert!(text.contains("%20")); // 空格
        assert!(text.contains("%E6")); // “我” UTF-8 首字节 E6
        assert!(!text.contains(' ')); // 不留裸空格
    }

    #[test]
    fn 构建_跳过非绝对路径() {
        let p: [&[u8]; 2] = [b"relative/x", b"/abs/y"];
        assert_eq!(build_uri_list(&p), b"file:///abs/y\r\n");
    }

    #[test]
    fn 解析_往返保真() {
        let orig: [&[u8]; 2] = ["/home/kk/我的 文件.txt".as_bytes(), b"/tmp/b"];
        let back = parse_uri_list(&build_uri_list(&orig));
        assert_eq!(back, vec![orig[0].to_vec(), orig[1].to_vec()]);
    }

    #[test]
    fn 解析_忽略注释与非file协议() {
        let data = b"# comment\r\nfile:///a\r\nhttp://x/y\r\n\r\nfile:///b\r\n";
        assert_eq!(parse_uri_list(data), vec![b"/a".to_vec(), b"/b".to_vec()]);
    }

    #[test]
    fn 解析_host段跳过() {
        assert_eq!(
            parse_uri_list(b"file://localhost/etc/hosts\r\n"),
            vec![b"/etc/hosts".to_vec()]
        );
    }

    #[test]
    fn 解析_lf与crlf都接受() {
        assert_eq!(
            parse_uri_list(b"file:///a\nfile:///b\n"),
            vec![b"/a".to_vec(), b"/b".to_vec()]
        );
    }

    #[test]
    fn 解析_大小写不敏感scheme() {
        assert_eq!(parse_uri_list(b"FILE:///a\r\n"), vec![b"/a".to_vec()]);
    }
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
