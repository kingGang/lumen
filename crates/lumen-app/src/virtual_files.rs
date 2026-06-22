//! 远程虚拟文件剪贴板（M5.3 part3c-2 片6）：把「远程复制」的被控端文件以 COM 延迟渲染方式放进
//! 系统剪贴板，使资源管理器 / 桌面 Ctrl+V 粘贴时**按需**从被控端下载（类似 WinSCP 拖拽下载）。
//!
//! 远程文件不在本地、无本地路径，进不了 `CF_HDROP`（[`crate::clipboard_files`]）；故改用自定义
//! [`IDataObject`] 暴露两个 Shell 虚拟文件格式：`FileGroupDescriptorW`（文件名/属性）+ `FileContents`
//! （`TYMED_ISTREAM`，延迟渲染）。资源管理器粘贴时回调 `GetData(FileContents)`，本对象**立即**返回
//! 一个自实现的流式 `IStream`（`RemoteFileStream`），其 `Read` 阻塞消费由控制端边下边喂的分块，
//! 实现按需流式下载——进度条从一开始就显示、随真实下载平滑推进（不必先下完整文件再交付）。
//!
//! ## 线程模型
//! 专用 STA 线程承载 OLE 剪贴板（`OleInitialize` + 消息泵 + `OleSetClipboard`），**不污染** winit
//! 主循环的 COM 单元。UI 主线程经 [`ClipboardService`] 句柄发命令（`PostThreadMessageW` 唤醒消息
//! 泵）。`GetData` / `IStream::Read` 在 OLE 线程被资源管理器跨进程编组回调；`Read` 经 `ClipFetchCmd`
//! 请 **UI 主线程**起流式 `FetchReq`、阻塞等分块（`StreamMsg`）——UI 主线程从不反向阻塞 OLE 线程，
//! 无死锁。
//!
//! 非 Windows 为空桩（本项目仅 Windows，跨平台仅保证编译通过）。

#[cfg(windows)]
pub use imp::ClipboardService;

#[cfg(not(windows))]
pub use stub::ClipboardService;

#[cfg(not(windows))]
mod stub {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    use winit::event_loop::EventLoopProxy;

    use crate::remote_ws::ClipFetchCmd;
    use crate::PtyWake;

    pub struct ClipboardService;

    impl ClipboardService {
        pub fn start(
            _proxy: EventLoopProxy<PtyWake>,
            _wake_pending: Arc<AtomicBool>,
            _fetch_tx: Sender<ClipFetchCmd>,
        ) -> Self {
            Self
        }
        pub fn set_remote_file(&self, _path: String, _name: String, _size: u64) {}
        pub fn set_remote_dir(&self, _entries: Vec<lumen_protocol::remote::RecursiveDirEntry>) {}
        pub fn clear(&self) {}
    }
}

#[cfg(windows)]
mod imp {
    use std::iter::once;
    use std::mem::{size_of, ManuallyDrop};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};

    use winit::event_loop::EventLoopProxy;

    use windows::core::{implement, Ref, Result, BOOL, HRESULT, PCWSTR};
    use windows::Win32::Foundation::{
        DV_E_FORMATETC, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, HGLOBAL, LPARAM, OLE_E_ADVISENOTSUPPORTED,
        S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, WPARAM,
    };
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
        ISequentialStream_Impl, IStream, IStream_Impl, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
        LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK,
        STREAM_SEEK_CUR, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::{
        OleInitialize, OleIsCurrentClipboard, OleSetClipboard, OleUninitialize, DROPEFFECT_COPY,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Shell::{
        SHCreateStdEnumFmtEtc, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW,
        FILEGROUPDESCRIPTORW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, TranslateMessage, MSG,
        WM_APP,
    };

    use crate::remote_ws::{ClipFetchCmd, StreamMsg};
    use crate::PtyWake;
    use lumen_protocol::remote::RecursiveDirEntry;

    /// `FILE_ATTRIBUTE_NORMAL`（避免为单个常量再开 `Win32_Storage_FileSystem` feature）。
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    /// `FILE_ATTRIBUTE_DIRECTORY`（片8 目录项；同上不开 feature）。
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    /// 三个注册型剪贴板格式 id（`RegisterClipboardFormatW`，进程级稳定）。
    #[derive(Clone, Copy)]
    struct ClipFormats {
        descriptor: u16,
        contents: u16,
        preferred: u16,
    }

    /// UI 主线程 → OLE 线程命令。
    enum OleCmd {
        /// 把一个远程文件放进系统剪贴板（构造 `IDataObject` + `OleSetClipboard`）。
        SetRemoteFile { path: String, name: String, size: u64 },
        /// 片8：把一棵远程目录子树（平铺项）放进系统剪贴板（多项 descriptor + 多 lindex 流）。
        SetRemoteDir { entries: Vec<RecursiveDirEntry> },
        /// 若剪贴板当前对象仍是我们放的则清空。
        Clear,
        /// 退出 OLE 线程。
        Stop,
    }

    /// UI 主线程持有的剪贴板服务句柄：经命令通道 + `PostThreadMessageW` 驱动 OLE 线程。
    pub struct ClipboardService {
        cmd_tx: Sender<OleCmd>,
        thread_id: u32,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl ClipboardService {
        /// 启动专用 STA OLE 线程。`fetch_tx`：OLE 线程经它请 UI 主线程把远程文件下到临时文件。
        pub fn start(
            proxy: EventLoopProxy<PtyWake>,
            wake_pending: Arc<AtomicBool>,
            fetch_tx: Sender<ClipFetchCmd>,
        ) -> Self {
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<OleCmd>();
            let (id_tx, id_rx) = std::sync::mpsc::channel::<u32>();
            let handle = std::thread::Builder::new()
                .name("ole-clipboard".to_owned())
                .spawn(move || ole_thread(&cmd_rx, &id_tx, &proxy, &wake_pending, &fetch_tx))
                .ok();
            // 等线程报回 id（OleInitialize 失败回 0 → 后续命令静默无效）。
            let thread_id = id_rx.recv().unwrap_or(0);
            Self {
                cmd_tx,
                thread_id,
                handle,
            }
        }

        /// 远程复制单文件：放进系统剪贴板（替换上一个）。
        pub fn set_remote_file(&self, path: String, name: String, size: u64) {
            if self
                .cmd_tx
                .send(OleCmd::SetRemoteFile { path, name, size })
                .is_ok()
            {
                self.wake();
            }
        }

        /// 片8 远程复制目录：把递归枚举好的子树平铺项放进系统剪贴板（替换上一个）。
        pub fn set_remote_dir(&self, entries: Vec<RecursiveDirEntry>) {
            if self.cmd_tx.send(OleCmd::SetRemoteDir { entries }).is_ok() {
                self.wake();
            }
        }

        /// 清空我们放的系统剪贴板（本地复制 / 会话结束时调，防残留指向已失效被控端的虚拟文件）。
        pub fn clear(&self) {
            if self.cmd_tx.send(OleCmd::Clear).is_ok() {
                self.wake();
            }
        }

        fn wake(&self) {
            if self.thread_id == 0 {
                return;
            }
            // SAFETY: 向已知 OLE 线程投递无参 WM_APP，唤醒其 GetMessage 去 drain 命令通道。
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_APP, WPARAM(0), LPARAM(0));
            }
        }
    }

    impl Drop for ClipboardService {
        fn drop(&mut self) {
            let _ = self.cmd_tx.send(OleCmd::Stop);
            self.wake();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// 注册剪贴板格式，返回 id（失败 0）。
    fn register(name: &str) -> u16 {
        let wide: Vec<u16> = name.encode_utf16().chain(once(0)).collect();
        // SAFETY: PCWSTR 指向本地 null 结尾宽字符串，调用期间有效；失败返回 0。
        unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) as u16 }
    }

    /// 专用 STA 线程主体：OleInitialize → 报回线程 id → 消息泵（WM_APP 驱动命令 / 其余分发以
    /// 服务跨进程编组回调）→ OleUninitialize。
    fn ole_thread(
        cmd_rx: &Receiver<OleCmd>,
        id_tx: &Sender<u32>,
        proxy: &EventLoopProxy<PtyWake>,
        wake_pending: &Arc<AtomicBool>,
        fetch_tx: &Sender<ClipFetchCmd>,
    ) {
        // SAFETY: 本线程独占该 STA；OleInitialize 与函数末 OleUninitialize 配对。
        unsafe {
            if OleInitialize(None).is_err() {
                let _ = id_tx.send(0);
                return;
            }
        }
        let cf = ClipFormats {
            descriptor: register("FileGroupDescriptorW"),
            contents: register("FileContents"),
            preferred: register("Preferred DropEffect"),
        };
        // SAFETY: 纯查询当前线程 id。
        let _ = id_tx.send(unsafe { GetCurrentThreadId() });

        // 当前放在剪贴板上的对象（用于判属清理；系统另持 AddRef 引用）。
        let mut current: Option<IDataObject> = None;
        let mut msg = MSG::default();
        loop {
            // SAFETY: 标准消息泵，msg 为本地可变量。
            let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if r.0 <= 0 {
                break; // 0=WM_QUIT，-1=错误。
            }
            if msg.message == WM_APP {
                let mut stop = false;
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        OleCmd::SetRemoteFile { path, name, size } => {
                            let obj: IDataObject = RemoteDataObject::new(
                                path,
                                &name,
                                size,
                                cf,
                                fetch_tx.clone(),
                                proxy.clone(),
                                wake_pending.clone(),
                            )
                            .into();
                            // SAFETY: STA 线程内放剪贴板；系统 AddRef，current 留引用供后续判属清理。
                            unsafe {
                                let _ = OleSetClipboard(Some(&obj));
                            }
                            current = Some(obj);
                        }
                        OleCmd::SetRemoteDir { entries } => {
                            // 全部项超 MAX_PATH 剔除 / 空树 → None：不放空 descriptor（cItems=0 未定义）。
                            if let Some(rdo) = RemoteDataObject::new_dir(
                                entries,
                                cf,
                                fetch_tx.clone(),
                                proxy.clone(),
                                wake_pending.clone(),
                            ) {
                                let obj: IDataObject = rdo.into();
                                // SAFETY: STA 线程内放剪贴板；系统 AddRef，current 留引用供判属清理。
                                unsafe {
                                    let _ = OleSetClipboard(Some(&obj));
                                }
                                current = Some(obj);
                            }
                        }
                        OleCmd::Clear => {
                            clear_if_ours(current.as_ref());
                            current = None;
                        }
                        OleCmd::Stop => {
                            clear_if_ours(current.as_ref());
                            current = None;
                            stop = true;
                        }
                    }
                }
                if stop {
                    // SAFETY: 投递 WM_QUIT 让下一轮 GetMessage 返回 0、退出循环。
                    unsafe { PostQuitMessage(0) };
                }
            } else {
                // SAFETY: 标准消息翻译 / 分发（服务 COM 编组）。
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        // SAFETY: 与开头 OleInitialize 配对。
        unsafe { OleUninitialize() };
    }

    /// 仅当剪贴板当前对象仍是我们放的才清空（避免踩掉其它应用之后复制的内容）。
    fn clear_if_ours(current: Option<&IDataObject>) {
        if let Some(obj) = current {
            // SAFETY: OleIsCurrentClipboard 只读判属；S_OK 才 OleSetClipboard(None) 释放。
            unsafe {
                if OleIsCurrentClipboard(obj).is_ok() {
                    let _ = OleSetClipboard(None);
                }
            }
        }
    }

    /// 一个 descriptor 平铺项（文件或目录）。`items` 的顺序即 `fgd` 数组顺序即资源管理器
    /// `lindex` 索引——**严禁重排**（重排会令 lindex 错位、粘出错位文件内容）。
    struct DescItem {
        /// 相对路径宽字符（`/`→`\` 已转换、null 结尾，写入 `FILEDESCRIPTORW.cFileName`）。
        rel_name_w: Vec<u16>,
        /// 被控端不透明绝对路径（`FetchReq` key；目录项不会被取内容、此值不用）。
        abs_path: String,
        /// 文件字节数（目录为 0）。
        size: u64,
        /// 是否目录（打 `FILE_ATTRIBUTE_DIRECTORY`，不被请求 `FileContents`）。
        is_dir: bool,
    }

    impl DescItem {
        /// 由相对路径构造（`/`→`\` 转换）。相对路径宽字符 > 259（MAX_PATH 限制）则返回 `None`
        /// （整项剔除而非中间截断成乱路径——对抗审查 L2）。
        fn new(rel: &str, abs_path: String, size: u64, is_dir: bool) -> Option<Self> {
            let mut rel_name_w: Vec<u16> = rel.replace('/', "\\").encode_utf16().collect();
            if rel_name_w.len() > 259 {
                return None;
            }
            rel_name_w.push(0);
            Some(Self {
                rel_name_w,
                abs_path,
                size: if is_dir { 0 } else { size },
                is_dir,
            })
        }
    }

    /// 自定义虚拟文件数据对象（单文件或目录子树）。延迟渲染：`GetData(FileContents, lindex)` 时
    /// 才按 `lindex` 起对应文件的流式下载。
    #[implement(IDataObject)]
    struct RemoteDataObject {
        /// 平铺子树项（文件 + 目录）。顺序即 descriptor `fgd` 顺序即 `lindex` 索引（严禁重排）。
        items: Vec<DescItem>,
        cf: ClipFormats,
        fetch_tx: Sender<ClipFetchCmd>,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    }

    impl RemoteDataObject {
        /// 单文件（远程复制单个文件）：一项 descriptor。
        fn new(
            path: String,
            name: &str,
            size: u64,
            cf: ClipFormats,
            fetch_tx: Sender<ClipFetchCmd>,
            proxy: EventLoopProxy<PtyWake>,
            wake_pending: Arc<AtomicBool>,
        ) -> Self {
            // 单文件名无子目录分隔；超 259 也保底截断成一项（单文件名一般远不及）。
            let item = DescItem::new(name, path.clone(), size, false).unwrap_or_else(|| {
                let mut rel_name_w: Vec<u16> = name.encode_utf16().take(259).collect();
                rel_name_w.push(0);
                DescItem {
                    rel_name_w,
                    abs_path: path,
                    size,
                    is_dir: false,
                }
            });
            Self {
                items: vec![item],
                cf,
                fetch_tx,
                proxy,
                wake_pending,
            }
        }

        /// 片8 目录子树：把递归枚举的平铺项转成 descriptor 项（超 MAX_PATH 的项剔除）。全部被剔除
        /// / 空树时返回 `None`（不放空 descriptor，cItems=0 行为未定义——对抗审查 L1）。
        fn new_dir(
            entries: Vec<RecursiveDirEntry>,
            cf: ClipFormats,
            fetch_tx: Sender<ClipFetchCmd>,
            proxy: EventLoopProxy<PtyWake>,
            wake_pending: Arc<AtomicBool>,
        ) -> Option<Self> {
            // 标准做法（WinSCP / 7-Zip 同款）：列目录项 + 文件项，全部相对路径。目录项带 FD_ATTRIBUTES
            // + FILE_ATTRIBUTE_DIRECTORY，资源管理器见之 CreateDirectory（不 GetData）→ 连纯空子目录也
            // 建出；文件项 cFileName 含相对路径。entries 是 DFS pre-order（父目录项在子项之前），其顺序
            // 即 fgd 数组顺序即 lindex 索引——GetData 用 items[lindex] 命中正确（目录项占下标但不被请求内容）。
            let items: Vec<DescItem> = entries
                .into_iter()
                .filter_map(|e| DescItem::new(&e.rel_path, e.path, e.size, e.is_dir))
                .collect();
            if items.is_empty() {
                return None;
            }
            Some(Self {
                items,
                cf,
                fetch_tx,
                proxy,
                wake_pending,
            })
        }

        /// 资源管理器取内容：**立即**返回一个惰性流式 [`RemoteFileStream`]（不阻塞、此刻不发起下载），
        /// 由该流首个 `Read` 时才请 UI 主线程起流式下载——避免大目录粘贴时一次性起几百条 fetch（M2）。
        fn make_stream(&self, item: &DescItem) -> IStream {
            RemoteFileStream::new(
                StreamLaunch {
                    fetch_tx: self.fetch_tx.clone(),
                    path: item.abs_path.clone(),
                    proxy: self.proxy.clone(),
                    wake_pending: self.wake_pending.clone(),
                },
                item.size,
            )
            .into()
        }

        /// 是否支持某 `FORMATETC`（格式 id + tymed 双匹配）。
        fn supports(&self, fe: &FORMATETC) -> bool {
            let cf = fe.cfFormat;
            (cf == self.cf.descriptor && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0)
                || (cf == self.cf.contents && fe.tymed & TYMED_ISTREAM.0 as u32 != 0)
                || (cf == self.cf.preferred && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0)
        }

        /// 构造 `FileGroupDescriptorW`（N 项变长）的 HGLOBAL 媒介。文件项设 `FD_FILESIZE`（否则当
        /// 0 字节空文件、不取内容——0KB bug）；目录项打 `FILE_ATTRIBUTE_DIRECTORY`（建出空目录）。
        fn descriptor_medium(&self) -> Result<STGMEDIUM> {
            let n = self.items.len();
            // FILEGROUPDESCRIPTORW 含 fgd[1] 柔性首项；额外 (n-1) 项。空 items 不应到此（new_dir 已挡）。
            let size =
                size_of::<FILEGROUPDESCRIPTORW>() + n.saturating_sub(1) * size_of::<FILEDESCRIPTORW>();
            // SAFETY: 分配可移动零初始化全局内存，加锁逐项写入 FILEDESCRIPTORW 后解锁；返回的 HGLOBAL
            // 所有权随 STGMEDIUM 交给资源管理器。
            unsafe {
                let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size)?;
                let p = GlobalLock(h).cast::<FILEGROUPDESCRIPTORW>();
                if p.is_null() {
                    // GlobalLock 失败（紧接 alloc 成功后几乎不可能）：放弃本次（不引入额外 free 依赖，
                    // 该句柄进程级极小泄漏可接受）。
                    return Err(E_OUTOFMEMORY.into());
                }
                (*p).cItems = n as u32;
                // fgd 是柔性数组（声明 [FILEDESCRIPTORW;1]）：取首项指针后按 add(i) 索引第 i 项。
                // FILEDESCRIPTORW 是 #[repr(packed)]：禁止对其字段取引用（E0793），一律
                // addr_of_mut + write_unaligned 写入。
                let fgd0 = std::ptr::addr_of_mut!((*p).fgd).cast::<FILEDESCRIPTORW>();
                for (i, item) in self.items.iter().enumerate() {
                    let fd = fgd0.add(i);
                    let flags = if item.is_dir {
                        // FD_ATTRIBUTES：声明 dwFileAttributes 有效，否则资源管理器忽略 FILE_ATTRIBUTE_DIRECTORY、
                        // 把目录项当文件去 GetData 拉空（之前多文件目录失败的真因）。
                        FD_ATTRIBUTES.0 as u32
                    } else {
                        (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32
                    };
                    let attr = if item.is_dir {
                        FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        FILE_ATTRIBUTE_NORMAL
                    };
                    std::ptr::addr_of_mut!((*fd).dwFlags).write_unaligned(flags);
                    std::ptr::addr_of_mut!((*fd).dwFileAttributes).write_unaligned(attr);
                    std::ptr::addr_of_mut!((*fd).nFileSizeHigh)
                        .write_unaligned((item.size >> 32) as u32);
                    std::ptr::addr_of_mut!((*fd).nFileSizeLow).write_unaligned(item.size as u32);
                    // cFileName: [u16; 260]（含结尾 null；rel_name_w 已截到 ≤260 且 null 结尾）。
                    let name_dst = std::ptr::addr_of_mut!((*fd).cFileName).cast::<u16>();
                    for (j, &c) in item.rel_name_w.iter().take(260).enumerate() {
                        name_dst.add(j).write_unaligned(c);
                    }
                }
                let _ = GlobalUnlock(h);
                Ok(hglobal_medium(h))
            }
        }
    }

    /// 包一个 HGLOBAL 为 STGMEDIUM（所有权转给接收方）。
    fn hglobal_medium(h: HGLOBAL) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    /// 包一个 IStream 为 STGMEDIUM。
    fn istream_medium(s: IStream) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(s)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    /// 构造一个 4 字节 DWORD 的 HGLOBAL 媒介（Preferred DropEffect）。
    fn dword_medium(value: u32) -> Result<STGMEDIUM> {
        // SAFETY: 分配 4 字节全局内存写入 DWORD；失败 GlobalFree 回收。
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, 4)?;
            let p = GlobalLock(h).cast::<u32>();
            if p.is_null() {
                // 见 descriptor_medium：lock 失败几乎不可能，放弃即可。
                return Err(E_OUTOFMEMORY.into());
            }
            *p = value;
            let _ = GlobalUnlock(h);
            Ok(hglobal_medium(h))
        }
    }

    /// 构造一个用于 `EnumFormatEtc` 的 `FORMATETC`。
    fn make_format(cf: u16, tymed: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed as u32,
        }
    }

    impl IDataObject_Impl for RemoteDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
            // SAFETY: 资源管理器按 COM 契约传入合法非空 FORMATETC，仅读。
            let fe = unsafe { &*pformatetcin };
            let cf = fe.cfFormat;
            if cf == self.cf.descriptor && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
                return self.descriptor_medium();
            }
            if cf == self.cf.contents && fe.tymed & TYMED_ISTREAM.0 as u32 != 0 {
                // lindex = fgd 数组下标（单文件资源管理器可能传 -1，按 0 处理）；目录项 / 越界拒绝。
                let idx = fe.lindex.max(0) as usize;
                match self.items.get(idx) {
                    Some(item) if !item.is_dir => {
                        return Ok(istream_medium(self.make_stream(item)));
                    }
                    _ => return Err(DV_E_FORMATETC.into()),
                }
            }
            if cf == self.cf.preferred && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
                return dword_medium(DROPEFFECT_COPY.0);
            }
            Err(DV_E_FORMATETC.into())
        }

        fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            // SAFETY: 只读访问入参（COM 契约保证非空）。
            let fe = unsafe { &*pformatetc };
            if self.supports(fe) {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            _pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            // 不做规范化：返回 E_NOTIMPL，调用方按原 FORMATETC 处理。
            E_NOTIMPL
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> Result<()> {
            // 接受资源管理器写回（"Paste Succeeded" / "Performed DropEffect"），无需处理。
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let fmts = [
                make_format(self.cf.descriptor, TYMED_HGLOBAL.0),
                make_format(self.cf.contents, TYMED_ISTREAM.0),
                make_format(self.cf.preferred, TYMED_HGLOBAL.0),
            ];
            // SAFETY: 用本地 FORMATETC 列表建标准枚举器，系统复制其内容。
            unsafe { SHCreateStdEnumFmtEtc(&fmts) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<IAdviseSink>,
        ) -> Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    /// 流式只读虚拟文件流：资源管理器 `IStream::Read` 阻塞消费由 UI 主线程边下边喂的
    /// [`StreamMsg`]。读到 `Done` = EOF；`Failed` / 通道断 = 返回错误（资源管理器粘贴失败、
    /// 不落不完整文件）。`Read` 在资源管理器调用线程经 COM 编组回到 OLE 线程执行，阻塞等数据时
    /// 由 UI 主线程喂入，二者不互相阻塞、无死锁。
    /// 惰性流启动句柄（首个 `Read` 前持有；启动即 `take` 消费）。把「起一次流式下载」推迟到真正被
    /// 读取时，避免资源管理器对目录里 N 个文件 `GetData` 即起 N 条 fetch（M2）。
    struct StreamLaunch {
        fetch_tx: Sender<ClipFetchCmd>,
        path: String,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    }

    impl StreamLaunch {
        /// 首个 `Read` 时调：请 UI 主线程起一次流式下载，返回数据下行接收端。送命令失败（UI 已退）
        /// 则返回 `None`（流即视为失败）。
        fn launch(self) -> Option<Receiver<StreamMsg>> {
            let (tx, rx) = std::sync::mpsc::channel();
            if self
                .fetch_tx
                .send(ClipFetchCmd {
                    path: self.path,
                    data_tx: tx,
                })
                .is_err()
            {
                return None;
            }
            // 唤醒 UI 主线程（PTY 同款去重：仅在标志由 false→true 时发 PtyWake）。
            if !self.wake_pending.swap(true, Ordering::SeqCst) {
                let _ = self.proxy.send_event(PtyWake);
            }
            Some(rx)
        }
    }

    #[implement(IStream)]
    struct RemoteFileStream {
        inner: Mutex<StreamInner>,
        /// 文件总字节数（`Stat` 报 `cbSize`，供资源管理器显示大小 / 进度）。
        size: u64,
    }

    struct StreamInner {
        /// 惰性启动句柄（首个 `Read` 时 `take` → 起下载得到 `rx`）。
        launch: Option<StreamLaunch>,
        /// 数据下行端（启动后 `Some`；UI 主线程 `fetch_chunk` 喂）。
        rx: Option<Receiver<StreamMsg>>,
        /// 当前已收、尚未被 `Read` 取走的块（剩余切片 `pending[off..]`）。
        pending: Vec<u8>,
        off: usize,
        /// 已读总字节（仅供 `Seek(CUR, 0)` 报告当前位置）。
        pos: u64,
        /// 已收 `Done`（后续 `Read` 即 EOF）。
        done: bool,
        /// 已收 `Failed` / 通道断（后续 `Read` 返回错误）。
        failed: bool,
    }

    impl RemoteFileStream {
        fn new(launch: StreamLaunch, size: u64) -> Self {
            Self {
                size,
                inner: Mutex::new(StreamInner {
                    launch: Some(launch),
                    rx: None,
                    pending: Vec::new(),
                    off: 0,
                    pos: 0,
                    done: false,
                    failed: false,
                }),
            }
        }
    }

    impl ISequentialStream_Impl for RemoteFileStream_Impl {
        fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            let Ok(mut st) = self.inner.lock() else {
                return E_FAIL;
            };
            // 惰性启动：首个 Read 才起下载（避免大目录粘贴一次性起几百条 fetch）。
            if let Some(launch) = st.launch.take() {
                match launch.launch() {
                    Some(rx) => st.rx = Some(rx),
                    None => st.failed = true, // UI 已退，无法起下载。
                }
            }
            let dst = pv.cast::<u8>();
            let want = cb as usize;
            let mut written = 0usize;
            let mut hard_err = false;
            while written < want {
                // 先消费当前块剩余。
                if st.off < st.pending.len() {
                    let n = (want - written).min(st.pending.len() - st.off);
                    // SAFETY: dst 为资源管理器提供的 ≥cb 字节缓冲；写入 [written, written+n)。
                    unsafe {
                        core::ptr::copy_nonoverlapping(st.pending.as_ptr().add(st.off), dst.add(written), n);
                    }
                    st.off += n;
                    written += n;
                    st.pos += n as u64;
                    continue;
                }
                if st.done {
                    break; // EOF
                }
                if st.failed {
                    hard_err = written == 0;
                    break;
                }
                // 阻塞取下一块（UI 主线程喂；下载完发 Done、中止发 Failed）。rx 为 None（启动失败已置
                // failed、不会到此）兜底视为失败。
                let msg = match st.rx.as_ref() {
                    Some(rx) => rx.recv(),
                    None => Err(std::sync::mpsc::RecvError),
                };
                match msg {
                    Ok(StreamMsg::Chunk(data)) => {
                        st.pending = data;
                        st.off = 0;
                    }
                    Ok(StreamMsg::Done) => st.done = true,
                    Ok(StreamMsg::Failed) | Err(_) => st.failed = true,
                }
            }
            if !pcbread.is_null() {
                // SAFETY: 调用方提供的可空输出指针，已判非空。
                unsafe { *pcbread = written as u32 };
            }
            if hard_err {
                E_FAIL
            } else {
                S_OK
            }
        }

        fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            STG_E_ACCESSDENIED // 只读流
        }
    }

    impl IStream_Impl for RemoteFileStream_Impl {
        fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
            // 流式不可回退：仅支持「查当前位置」(CUR, 0)，其余拒绝。
            if dworigin == STREAM_SEEK_CUR && dlibmove == 0 {
                let pos = self.inner.lock().map(|s| s.pos).unwrap_or(0);
                if !plibnewposition.is_null() {
                    // SAFETY: 调用方提供的可空输出指针，已判非空。
                    unsafe { *plibnewposition = pos };
                }
                Ok(())
            } else {
                Err(STG_E_INVALIDFUNCTION.into())
            }
        }

        fn SetSize(&self, _libnewsize: u64) -> Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn CopyTo(
            &self,
            _pstm: Ref<IStream>,
            _cb: u64,
            _pcbread: *mut u64,
            _pcbwritten: *mut u64,
        ) -> Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
            Ok(())
        }

        fn Revert(&self) -> Result<()> {
            Ok(())
        }

        fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> Result<()> {
            if pstatstg.is_null() {
                return Err(STG_E_INVALIDFUNCTION.into());
            }
            // 报告文件总大小（cbSize），与 descriptor 的 FD_FILESIZE 一致；标明是流（pwcsName 留空）。
            // SAFETY: 调用方提供合法 STATSTG 输出指针；zeroed STATSTG（null name）合法。
            unsafe {
                let mut s: STATSTG = core::mem::zeroed();
                s.r#type = STGTY_STREAM.0 as u32;
                s.cbSize = self.size;
                *pstatstg = s;
            }
            Ok(())
        }

        fn Clone(&self) -> Result<IStream> {
            Err(STG_E_INVALIDFUNCTION.into())
        }
    }
}
