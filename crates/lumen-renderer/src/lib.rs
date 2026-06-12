//! Lumen 的渲染层：wgpu surface 管理 + glyphon 文本渲染 + 矩形管线。
//!
//! 每帧流程：Grid → (背景/光标/下划线矩形, 每行 rich text) → GPU。
//!
//! M3 起终端内容渲染到**持久离屏纹理**（egui 以 `ui.image` 把它嵌进
//! 工作区），surface 的取帧/呈现所有权上移到 app 层（egui pass 是
//! surface 的唯一写入方）。M3.7 分屏（F5）后离屏纹理与行级排版缓存
//! 都按窗格（会话 id）隔离：激活 tab 的全部窗格同帧各渲各的纹理，
//! 共享 atlas/字体系统/矩形管线等重资源；窗格关闭时调用方负责
//! [`Renderer::drop_offscreen`] 释放纹理与缓存。

/// footer 输入区视图数据结构（M4.1 批C，feature = "input-editor"）——设计稿 §7.1。
#[cfg(feature = "input-editor")]
pub mod composer_view;
mod rect;
mod theme;
pub mod themes;

use std::collections::HashMap;

use anyhow::{Context, Result};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use lumen_term::{CellFlags, Selection, Terminal};

pub use theme::{Rgb, Theme};
pub use wgpu;

/// 字号（逻辑像素）与行高倍率。
const FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT_FACTOR: f32 = 1.35;
/// 内容区与窗口边缘的内边距（逻辑像素，随 DPI 缩放）。
const PADDING: f32 = 10.0;

/// 一行的排版缓存：内容哈希 + 各文本段（起始列, 排版 buffer）。
struct RowSegs {
    hash: Option<u64>,
    segs: Vec<(usize, TextBuffer)>,
}

/// 终端内容的持久离屏渲染目标。
///
/// 同一张纹理持有两个视图：终端管线写入纹理本身格式（通常 sRGB）的
/// 渲染视图；egui 采样用去掉 sRGB 后缀的同布局视图——egui 的着色器
/// 期望「非 sRGB-aware」纹理（它自己做 gamma→linear），若用 sRGB 视图
/// 采样会被硬件先转线性、再被 egui 二次转换，画面整体偏暗。
struct Offscreen {
    /// 终端管线的渲染目标视图（纹理本身格式）。两个视图各自持有
    /// 底层纹理的引用（wgpu 资源引用计数），不必单独保存 Texture。
    render_view: wgpu::TextureView,
    /// 供 egui 采样的视图（非 sRGB 格式重解释）。
    sample_view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl Offscreen {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let sample_format = format.remove_srgb_suffix();
        let view_formats: &[wgpu::TextureFormat] = if sample_format == format {
            &[]
        } else {
            std::slice::from_ref(&sample_format)
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lumen offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats,
        });
        let render_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sample_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(sample_format),
            ..Default::default()
        });
        Self {
            render_view,
            sample_view,
            width,
            height,
        }
    }
}

/// 终端渲染器。持有 GPU 资源与字体系统。
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// 各窗格的离屏渲染目标（键 = 会话 id，尺寸 = 窗格物理像素）。
    /// 窗格关闭时由调用方 [`Self::drop_offscreen`] 释放。
    offscreens: HashMap<u64, Offscreen>,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// 行级排版缓存（键 = 会话 id）：行内容（哈希）不变则整行跳过
    /// shaping。TUI 全屏界面（如 codex 的框线边框）段数巨大，没有
    /// 缓存时每帧全量整形会把打字回显拖卡。多窗格同帧渲染必须按
    /// 会话隔离，否则互相踢缓存等于没有缓存；窗格关闭随
    /// [`Self::drop_offscreen`] 一并清理，防内存泄漏。
    row_caches: HashMap<u64, Vec<RowSegs>>,
    rects: rect::RectRenderer,

    theme: Theme,
    font_family: String,
    /// 渲染与测量必须用同一字号：行高经过取整，从行高反推
    /// 字号会放大 advance，长行下光标与文字逐字漂移。
    font_size: f32,
    /// DPI 缩放因子（运行时重配置字体把逻辑字号换算成物理字号用）。
    scale_factor: f32,
    cell_w: f32,
    cell_h: f32,
    /// 内边距（物理像素）。
    padding: f32,
    /// 背景图模式（P13）：`true` 时离屏 Clear 用全透明，让 egui 层的
    /// 背景图纹理透出；`false` 时 Clear 为主题背景色（默认行为）。
    ///
    /// 技术细节：egui mesh blend 是 premultiplied（ONE, ONE_MINUS_SRC_ALPHA）。
    /// 若 Clear 为不透明色（RGB≠0, A=1）而下层已有背景图，终端像素的
    /// RGB 分量会被加色叠加到背景图上，导致颜色泄漏（发白/发亮）。
    /// 全透明（RGBA=0）则终端像素完整覆盖背景图对应区域，无泄漏。
    transparent_background: bool,
    /// footer 上边框颜色（问题5）：由外壳层在主题切换时通过
    /// [`Self::set_footer_border_color`] 注入，对齐全 app 的
    /// `panel_outline` 描边色；默认值为深色板 #4a4a4a。
    footer_border_color: Rgb,
}

impl Renderer {
    /// 创建渲染器。`target` 一般传 `Arc<winit::window::Window>`。
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        scale_factor: f32,
    ) -> Result<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(target)
            .context("创建 wgpu surface 失败")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("未找到可用的 GPU adapter")?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .context("创建 wgpu device 失败")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Mailbox：非阻塞呈现且无撕裂（过期帧直接被替换）。高频渲染
        // 时 Fifo/AutoVsync 的取帧会阻塞主线程等垂直同步，把键盘与
        // PTY 处理一起拖住。
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::AutoVsync
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let font_family = pick_mono_family(&font_system);
        log::info!("使用等宽字体: {font_family}");

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let rects = rect::RectRenderer::new(&device, format);

        let font_size = FONT_SIZE * scale_factor;
        let (cell_w, cell_h) = measure_cell(&mut font_system, &font_family, font_size);
        log::info!("单元格尺寸: {cell_w}x{cell_h} 物理像素");

        Ok(Self {
            device,
            queue,
            surface,
            config,
            offscreens: HashMap::new(),
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            row_caches: HashMap::new(),
            rects,
            theme: Theme::default(),
            font_family,
            font_size,
            scale_factor,
            cell_w,
            cell_h,
            padding: PADDING * scale_factor,
            transparent_background: false,
            // 问题5：默认深色板 panel_outline #4a4a4a（主题应用时由外壳覆写）。
            footer_border_color: Rgb(0x4a, 0x4a, 0x4a),
        })
    }

    /// 运行时重配置字体（设置页「即时生效」链路的渲染器侧入口）。
    ///
    /// `family` 为字体家族名（空串 = 自动挑选默认等宽字体）；系统中
    /// 不存在该字体时回退默认，不 panic。`font_size_logical` 为逻辑
    /// 像素字号（内部乘 DPI 缩放）。重新测量单元格尺寸并使全部行排版
    /// 缓存失效；调用方随后须按新 cell 尺寸重算行列数并对全部会话
    /// resize（终端 + PTY）。
    ///
    /// 返回实际生效的家族名——与请求不同即发生了回退，调用方据此在
    /// 设置页提示用户。
    pub fn reconfigure_font(&mut self, family: &str, font_size_logical: f32) -> String {
        let resolved = resolve_family(&self.font_system, family);
        self.font_size = (font_size_logical * self.scale_factor).max(1.0);
        self.font_family = resolved.clone();
        let (w, h) = measure_cell(&mut self.font_system, &self.font_family, self.font_size);
        self.cell_w = w;
        self.cell_h = h;
        self.invalidate_row_cache();
        log::info!(
            "字体重配置: 「{}」{font_size_logical}（物理 {}）单元格 {w}x{h}",
            self.font_family,
            self.font_size
        );
        resolved
    }

    /// DPI 缩放因子变化（窗口跨显示器迁移 / 运行中改系统缩放）。
    ///
    /// 更新缩放并按新值重算内边距（物理像素）。字号/单元格度量不在
    /// 此处重算——调用方随后必须调 [`Self::reconfigure_font`] 以新
    /// 缩放重测（顺带使行排版缓存失效），再走「矩形/网格对照检查」
    /// 链路完成行列数重算与全会话 resize。
    pub fn set_scale_factor(&mut self, scale: f32) {
        self.scale_factor = scale.max(0.1);
        self.padding = PADDING * self.scale_factor;
    }

    /// 切换终端主题，并使行排版缓存整体失效——行哈希只包含单元格
    /// 自身的颜色编码（Indexed/Rgb），不含主题解析出的实际 RGB，
    /// 换主题后必须强制全量重排一次，否则旧配色的行会因哈希命中
    /// 而保持旧颜色。
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.invalidate_row_cache();
    }

    /// 设置背景图透明通路（P13）。
    ///
    /// `enabled` 为 `true` 时离屏 Clear 改用全透明（`wgpu::Color::TRANSPARENT`），
    /// 允许 egui 层的背景图透过终端内容区显现；
    /// `false` 时恢复主题背景色 Clear（默认行为）。
    pub fn set_transparent_background(&mut self, enabled: bool) {
        self.transparent_background = enabled;
    }

    /// 设置 footer 上边框颜色（问题5）。
    ///
    /// 应在主题切换时由外壳层调用，传入当前生效色板的 `panel_outline`
    /// 分量（R/G/B，sRGB 值域 0–255），对齐全 app 面板描边语义。
    pub fn set_footer_border_color(&mut self, r: u8, g: u8, b: u8) {
        self.footer_border_color = Rgb(r, g, b);
    }

    /// 行级排版缓存整体失效（字体/字号/主题变更后调用，全窗格）。
    fn invalidate_row_cache(&mut self) {
        for cache in self.row_caches.values_mut() {
            for r in cache.iter_mut() {
                r.hash = None;
            }
        }
    }

    /// 单元格物理像素尺寸（app 用它换算终端区尺寸 ↔ 行列数）。
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// 内边距（物理像素）。footer 内边距计算需要此值。
    pub fn padding(&self) -> f32 {
        self.padding
    }

    /// 给定终端区物理像素尺寸能容纳的 (rows, cols)（扣除四周内边距）。
    ///
    /// 不含 footer 扣高；feature = "input-editor" 开启时请用
    /// [`Self::grid_size_for_with_footer`] 传入 footer 高度。
    pub fn grid_size_for(&self, width: u32, height: u32) -> (usize, usize) {
        self.grid_size_for_with_footer(width, height, 0.0)
    }

    /// 给定终端区物理像素尺寸能容纳的 (rows, cols)，扣除 footer 后再计算。
    ///
    /// `footer_px` 为 footer 区域物理像素高度（`0.0` = 无 footer，与
    /// [`Self::grid_size_for`] 等价）。调用方传入 `footer_height_px(...)` 的
    /// 返回值，renderer 接口侵入最小（侦察报告 §5 建议方案）。
    ///
    /// # 设计稿对应章节
    /// 设计稿 §7.1「grid 扣高」+ 侦察报告 §5。
    pub fn grid_size_for_with_footer(
        &self,
        width: u32,
        height: u32,
        footer_px: f32,
    ) -> (usize, usize) {
        let usable_h = (height as f32 - self.padding * 2.0 - footer_px).max(0.0);
        let usable_w = (width as f32 - self.padding * 2.0).max(0.0);
        let rows = (usable_h / self.cell_h).floor() as usize;
        let cols = (usable_w / self.cell_w).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// 窗格内像素坐标（相对窗格原点）→ 视图格子坐标（行, 列），
    /// 自动夹紧到该窗格的网格范围。`width`/`height` 为窗格物理像素
    /// 尺寸（分屏后各窗格尺寸不同，由调用方传入）。
    ///
    /// 不含 footer 排除；feature = "input-editor" 开启时请用
    /// [`Self::cell_at_with_footer`] 以正确排除 footer 区域点击。
    pub fn cell_at(&self, px: f64, py: f64, width: u32, height: u32) -> (usize, usize) {
        self.cell_at_with_footer(px, py, width, height, 0.0)
    }

    /// 窗格内像素坐标 → 视图格子坐标，排除底部 footer 区域。
    ///
    /// `footer_px` 为 footer 物理像素高度（`0.0` = 无 footer，与
    /// [`Self::cell_at`] 等价）。点击落在 footer 区域（y ≥ height - footer_px）
    /// 时夹紧到网格末行，不映射进 footer（footer 有自己的点击处理）。
    ///
    /// # 设计稿对应章节
    /// 设计稿 §7.1「cell_at 排除 footer 区域」。
    pub fn cell_at_with_footer(
        &self,
        px: f64,
        py: f64,
        width: u32,
        height: u32,
        footer_px: f32,
    ) -> (usize, usize) {
        let (rows, cols) = self.grid_size_for_with_footer(width, height, footer_px);
        let col = ((px as f32 - self.padding) / self.cell_w).floor() as isize;
        let row = ((py as f32 - self.padding) / self.cell_h).floor() as isize;
        (
            row.clamp(0, rows as isize - 1) as usize,
            col.clamp(0, cols as isize - 1) as usize,
        )
    }

    /// 格子在终端区内的像素原点（IME 候选框定位等）。
    pub fn cell_origin(&self, row: usize, col: usize) -> (f32, f32) {
        (
            self.padding + col as f32 * self.cell_w,
            self.padding + row as f32 * self.cell_h,
        )
    }

    /// wgpu 设备（egui 渲染器等外部管线共用同一设备）。
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// wgpu 队列。
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// surface 像素格式（外部渲染管线的目标格式须与之一致）。
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// surface 当前物理尺寸。
    pub fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// 当前主题。
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// 指定窗格供 egui 采样的离屏视图（非 sRGB 重解释，缘由见
    /// [`Offscreen`]）；该窗格尚未创建离屏纹理时返回 None。
    pub fn offscreen_view(&self, id: u64) -> Option<&wgpu::TextureView> {
        self.offscreens.get(&id).map(|o| &o.sample_view)
    }

    /// 确保指定窗格的离屏纹理存在且为指定尺寸；新建或尺寸变化重建
    /// 时返回 true（调用方需把新视图绑定到该窗格的 egui 纹理 id）。
    pub fn ensure_offscreen(&mut self, id: u64, width: u32, height: u32) -> bool {
        let (width, height) = (width.max(1), height.max(1));
        if self
            .offscreens
            .get(&id)
            .is_some_and(|o| (o.width, o.height) == (width, height))
        {
            return false;
        }
        self.offscreens.insert(
            id,
            Offscreen::new(&self.device, self.config.format, width, height),
        );
        true
    }

    /// 释放指定窗格的离屏纹理与行排版缓存（窗格关闭时调用；egui 侧
    /// 的纹理注册由调用方自行注销）。
    pub fn drop_offscreen(&mut self, id: u64) {
        self.offscreens.remove(&id);
        self.row_caches.remove(&id);
    }

    /// 窗口 surface 物理尺寸变化（离屏纹理由 [`Self::ensure_offscreen`] 单独管理）。
    pub fn resize_surface(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// 取下一帧 surface 纹理。Lost/Outdated 时重新配置并返回 None
    /// （调用方跳过本帧呈现，等下一次重绘）。
    pub fn acquire_frame(&mut self) -> Option<wgpu::SurfaceTexture> {
        use wgpu::CurrentSurfaceTexture;
        match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(f) | CurrentSurfaceTexture::Suboptimal(f) => Some(f),
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                None
            }
            // Timeout/Occluded/校验错误：跳过本帧。
            _ => None,
        }
    }

    /// 渲染一帧终端内容到指定窗格（`id` = 会话 id）的离屏纹理
    /// （不触碰 surface，呈现由 app 层的 egui pass 完成）。
    ///
    /// `selection` 为当前鼠标选区（绝对行号定位）；`cursor` 为绘制中
    /// 的光标态 (行, 列, 可见)——由上层做位置防抖后传入，不直接读
    /// grid 光标，避免把 TUI 重绘期间的临时停留位画上屏；不可见时
    /// 行号仍有效，充当未闭合命令块状态条的下边界（与光标同源防抖，
    /// 块条几何才能帧间连续，见下方注释）；`selected_block` 为选中
    /// 命令块的 id（块背景高亮）。
    ///
    /// 等价于 `render_impl(id, term, selection, cursor, selected_block, None)`。
    /// feature = "input-editor" 开启时请用 [`Self::render_with_composer`] 传入
    /// footer 视图；flag 剔除时本函数是唯一 render 入口，行为与现状逐字节一致。
    pub fn render(
        &mut self,
        id: u64,
        term: &Terminal,
        selection: Option<&Selection>,
        cursor: (usize, usize, bool),
        selected_block: Option<u64>,
    ) -> Result<()> {
        self.render_impl(id, term, selection, cursor, selected_block, None)
    }

    /// 带 footer 输入区视图的渲染帧（feature = "input-editor"）——设计稿 §7.1。
    ///
    /// `composer` 为 footer 只读视图：
    /// - `None` 或 Hidden 态：不绘制 footer，grid 使用全高。
    /// - Composer / StatusBar 态：底部绘制 footer 卡片，grid 扣除 footer 高度。
    #[cfg(feature = "input-editor")]
    pub fn render_with_composer(
        &mut self,
        id: u64,
        term: &Terminal,
        selection: Option<&Selection>,
        cursor: (usize, usize, bool),
        selected_block: Option<u64>,
        composer: Option<&composer_view::ComposerView>,
    ) -> Result<()> {
        self.render_impl(id, term, selection, cursor, selected_block, composer)
    }

    /// 渲染实现（统一入口，两个公开 render 方法均调用此处）。
    fn render_impl(
        &mut self,
        id: u64,
        term: &Terminal,
        selection: Option<&Selection>,
        cursor: (usize, usize, bool),
        selected_block: Option<u64>,
        #[cfg(feature = "input-editor")] composer: Option<&composer_view::ComposerView>,
        #[cfg(not(feature = "input-editor"))] _composer: Option<()>,
    ) -> Result<()> {
        // 离屏视图按值克隆（Arc 浅拷贝），避免长借用 self 卡住后续字段访问。
        let Some(off) = self.offscreens.get(&id) else {
            anyhow::bail!("窗格 {id} 的离屏纹理不存在（须先 ensure_offscreen）");
        };
        let view = off.render_view.clone();
        let (target_w, target_h) = (off.width, off.height);

        // ---- M4.1 批C：footer 扣高（feature = "input-editor"）——设计稿 §7.1 ----
        // footer_px 为底部保留的物理像素高度：
        //   - feature 剔除 / Hidden 态：0.0，grid 全高，行为与旧版逐字节一致。
        //   - Composer / StatusBar 态：1 行高 + 内边距（min 1 行常驻等高铁律）。
        // 内边距固定为 padding * 0.4，取 padding 的一个比例，不随字号比例失调。
        #[cfg(feature = "input-editor")]
        let footer_px = {
            let fp = self.padding * 0.4;
            let max_h = target_h as f32 / 3.0;
            composer_view::footer_height_px(composer, self.cell_h, fp, max_h)
        };
        #[cfg(not(feature = "input-editor"))]
        let footer_px: f32 = 0.0;

        let grid = term.grid();
        let rows = grid.rows();
        let cols = grid.cols();
        let (cw, ch) = (self.cell_w, self.cell_h);
        let pad = self.padding;

        // ---- 收集矩形：块标识、背景色块、选区高亮、下划线/删除线、光标 ----
        let view_top_abs = grid.view_top_abs_line();
        let mut instances: Vec<rect::RectInstance> = Vec::new();

        // 命令块视觉：左缘状态色条（运行中蓝/成功绿/失败红）、块首行
        // 顶部细分隔线。选中块的半透明高亮收集到单独列表，在所有
        // 单元格背景之后绘制（先画会被不透明 cell 背景盖成花斑）。
        // 不在备用屏幕（vim 等全屏程序）时才绘制。
        let mut block_tints: Vec<rect::RectInstance> = Vec::new();
        let (cur_row, cur_col, cursor_visible) = cursor;
        if !term.is_alt_screen() {
            let bar_x = pad * 0.2;
            let bar_w = (pad * 0.3).max(2.0);
            // 未闭合（运行中）块的下边界用「防抖光标行」而非 live 光标
            // 行：codex 等 TUI 在同步帧尾常把光标停在重绘残留位，live
            // 行帧间跨行大跳，蓝色状态条跟着伸缩就是左缘闪烁（需求池
            // P1）。防抖行与光标块同源（上层 cursor_displayed），随归
            // 位序列/超时才更新，块条几何因此与光标一样帧间连续。
            // 绝对行号换算与光标绘制同式：视图首行 + display_offset +
            // 防抖行 = dropped + scrollback + 行，不随用户回滚漂移。
            let bar_cap_abs = view_top_abs + grid.display_offset() as u64 + cur_row as u64;
            for vr in 0..rows {
                let abs_line = view_top_abs + vr as u64;
                let Some(block) = term.block_at_line_capped(abs_line, bar_cap_abs) else {
                    continue;
                };
                let y = pad + vr as f32 * ch;
                if selected_block == Some(block.id) {
                    block_tints.push(rect::RectInstance {
                        pos: [pad, y],
                        size: [cols as f32 * cw, ch],
                        color: self.theme.selection.to_linear_f32(0.35),
                    });
                }
                let bar_color = match block.exit_code {
                    None if !block.is_closed() => self.theme.ansi[4], // 运行中：蓝
                    Some(0) | None => self.theme.ansi[2],             // 成功：绿
                    Some(_) => self.theme.ansi[1],                    // 失败：红
                };
                instances.push(rect::RectInstance {
                    pos: [bar_x, y],
                    size: [bar_w, ch],
                    color: bar_color.to_linear_f32(0.85),
                });
                if abs_line == block.prompt_line && vr > 0 {
                    // 块首行顶部分隔线。
                    instances.push(rect::RectInstance {
                        pos: [pad, y],
                        size: [cols as f32 * cw, 1.0],
                        color: self.theme.foreground.to_linear_f32(0.12),
                    });
                }
            }
        }

        for (vr, row) in grid.visible_rows().enumerate() {
            let abs_line = view_top_abs + vr as u64;
            for (c, cell) in row.cells().iter().enumerate().take(cols) {
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                let (fg, bg) = self.theme.cell_colors(cell);
                let w = if cell.flags.contains(CellFlags::WIDE) {
                    cw * 2.0
                } else {
                    cw
                };
                let (x, y) = (pad + c as f32 * cw, pad + vr as f32 * ch);
                let selected = selection.is_some_and(|s| !s.is_empty() && s.contains(abs_line, c));
                if selected {
                    instances.push(rect::RectInstance {
                        pos: [x, y],
                        size: [w, ch],
                        color: self.theme.selection.to_linear_f32(1.0),
                    });
                } else if bg != self.theme.background {
                    instances.push(rect::RectInstance {
                        pos: [x, y],
                        size: [w, ch],
                        color: bg.to_linear_f32(1.0),
                    });
                }
                if cell.flags.contains(CellFlags::UNDERLINE) {
                    instances.push(rect::RectInstance {
                        pos: [x, y + ch - 2.0],
                        size: [w, 1.5],
                        color: fg.to_linear_f32(1.0),
                    });
                }
                if cell.flags.contains(CellFlags::STRIKE) {
                    instances.push(rect::RectInstance {
                        pos: [x, y + ch * 0.5],
                        size: [w, 1.5],
                        color: fg.to_linear_f32(1.0),
                    });
                }
            }
        }
        // 选中块高亮叠在全部单元格背景之上（半透明 tint）。
        instances.extend(block_tints);
        // 光标：跟随底部时绘制在可视区对应行（半透明块，文字仍可见）；
        // 落在宽字符上时画两格宽。
        if cursor_visible {
            let cursor_view_row = grid.display_offset() + cur_row;
            if cur_row < rows && cur_col < cols && cursor_view_row < rows {
                let on_wide = grid
                    .row(cur_row)
                    .cells()
                    .get(cur_col)
                    .is_some_and(|c| c.flags.contains(CellFlags::WIDE));
                instances.push(rect::RectInstance {
                    pos: [pad + cur_col as f32 * cw, pad + cursor_view_row as f32 * ch],
                    size: [if on_wide { cw * 2.0 } else { cw }, ch],
                    color: self.theme.cursor.to_linear_f32(0.55),
                });
            }
        }
        // ---- M4.1 批C：footer 矩形（feature = "input-editor"）——设计稿 §7.1 ----
        // 同一 render pass：卡片背景（不透明）/ 上边框 1px / 竖条光标 ~2px /
        // 选区高亮——全部走现有 RectInstance，零新 GPU 管线。
        #[cfg(feature = "input-editor")]
        if footer_px > 0.0 {
            if let Some(cv) = composer {
                if cv.is_visible() {
                    let footer_top = target_h as f32 - footer_px;
                    let footer_w = target_w as f32;
                    let fp = self.padding * 0.4; // footer 内边距（与 footer_px 计算一致）

                    // 卡片背景：主题背景色略深（alpha 混合到离屏纹理上）。
                    // 取主题背景 RGB，提高亮度或改变色调。
                    // 简化：用前景色 5% 透明度叠加（视觉上区别于 grid 区）。
                    let bg = self.theme.background;
                    instances.push(rect::RectInstance {
                        pos: [0.0, footer_top],
                        size: [footer_w, footer_px],
                        color: bg.to_linear_f32(1.0),
                    });
                    // 上边框 1px（问题5：使用 panel_outline 描边色，
                    // 由外壳层在主题切换时通过 set_footer_border_color 注入）。
                    instances.push(rect::RectInstance {
                        pos: [0.0, footer_top],
                        size: [footer_w, 1.0],
                        color: self.footer_border_color.to_linear_f32(1.0),
                    });

                    // Compose 态：选区高亮（在光标/IME 之前画，z 序在文字底下）+ 竖条光标。
                    use composer_view::FooterKind;
                    if cv.kind == FooterKind::Composer {
                        // M4.1 批F：选区高亮矩形（先于光标入队，z 序在文字底下，与终端选区同惯例）。
                        if let Some(sel) = &cv.selection {
                            let sel_rects = composer_view::selection_rects(
                                sel, &cv.lines, footer_top, fp, footer_w, cw, ch,
                            );
                            for (sx, sy, sw, sh) in sel_rects {
                                instances.push(rect::RectInstance {
                                    pos: [sx, sy],
                                    size: [sw, sh],
                                    // 与终端选区同色同 alpha（theme.selection，alpha=1.0）
                                    color: self.theme.selection.to_linear_f32(1.0),
                                });
                            }
                        }

                        let (cur_line, cur_byte) = cv.cursor;
                        // 安全：lines 至少有 1 行（compose_empty 保证）。
                        let line_text = cv.lines.get(cur_line).map(|s| s.as_str()).unwrap_or("");
                        // 使用 footer_byte_to_col 统一字节→列换算（CJK/emoji 宽字符各占 2 列）。
                        // 与选区几何 selection_rects 调用的是同一函数，保证光标与选区端点对齐。
                        // 取代原 chars().count()——后者对 CJK 每字符计 1 列而非 2 列，会低估列数。
                        let col = composer_view::footer_byte_to_col(line_text, cur_byte) as f32;
                        let cursor_x = fp + col * cw;
                        let cursor_y = footer_top + fp + cur_line as f32 * ch;
                        if cursor_x < footer_w && cursor_y + ch <= target_h as f32 {
                            instances.push(rect::RectInstance {
                                pos: [cursor_x, cursor_y],
                                size: [2.0_f32.max(cw * 0.12), ch],
                                color: self.theme.foreground.to_linear_f32(0.9),
                            });
                        }

                        // M4.1 批D2：IME preedit 下划线。
                        // preedit.text 绘制在光标处（内嵌方式），下划线跨越整个预编辑段。
                        // 使用 footer_byte_to_col 计算 preedit 显示列宽（CJK 各 2 列，不再用 chars().count()）。
                        if let Some(pre) = &cv.preedit {
                            if !pre.text.is_empty() {
                                // preedit 整段显示列宽（byte=text.len() → 全段宽度）。
                                let pre_col_w =
                                    composer_view::footer_byte_to_col(&pre.text, pre.text.len())
                                        as f32;
                                let underline_x = cursor_x;
                                let underline_y = cursor_y + ch - 2.0;
                                let underline_w = (pre_col_w * cw).min(footer_w - underline_x);
                                if underline_w > 0.0 && underline_y < target_h as f32 {
                                    // 下划线：前景色 70% 透明度，高 1.5px。
                                    instances.push(rect::RectInstance {
                                        pos: [underline_x, underline_y],
                                        size: [underline_w, 1.5],
                                        color: self.theme.foreground.to_linear_f32(0.7),
                                    });
                                }
                            }
                        }

                        // M4.1 批D2：退出码角标（exit_badge）。
                        // 显示在 footer 右侧：✓（绿）或 ✗（红）+ 耗时。
                        // 用一个小色块（角标背景）在右角提示。
                        if let Some(badge) = &cv.exit_badge {
                            // 角标色：成功绿 = ansi[2]，失败红 = ansi[1]。
                            let badge_color = if badge.exit_code == 0 {
                                self.theme.ansi[2] // 绿
                            } else {
                                self.theme.ansi[1] // 红
                            };
                            // 角标宽度：约 3 个字符宽（✓/✗ + 空格 + 耗时简写）。
                            // 精确文字宽度 M4.2 精化；此处固定小色块。
                            let badge_w = cw * 6.0_f32.min(footer_w / 4.0);
                            let badge_h = ch * 0.8;
                            let badge_x = (footer_w - badge_w - fp).max(fp);
                            let badge_y = footer_top + (footer_px - badge_h) / 2.0;
                            if badge_x > fp && badge_y > footer_top {
                                instances.push(rect::RectInstance {
                                    pos: [badge_x, badge_y],
                                    size: [badge_w, badge_h],
                                    color: badge_color.to_linear_f32(0.25),
                                });
                                // 角标左缘细竖线（1px，颜色全亮）。
                                instances.push(rect::RectInstance {
                                    pos: [badge_x, badge_y],
                                    size: [1.5, badge_h],
                                    color: badge_color.to_linear_f32(0.9),
                                });
                            }
                        }
                    }
                }
            }
        }

        self.rects
            .prepare(&self.device, &self.queue, (target_w, target_h), &instances);

        // ---- 文本排版：按网格分段强制对齐，行级缓存 ----
        // 窄字符连续成段、宽字符（CJK 等）与一切非 ASCII 字符单独成段，
        // 每段起点钉死在 col * cell_w（回退字形 advance != cell_w 时偏差
        // 不跨段累计）。行内容哈希不变则复用上一帧的排版结果。
        let metrics = Metrics::new(self.font_size, self.cell_h);
        let family = self.font_family.clone();
        let base_attrs = Attrs::new().family(Family::Name(&family));

        // 本窗格的行级排版缓存（按会话 id 隔离：多窗格同帧渲染若共享
        // 一份缓存会互相踢行哈希，等于没有缓存）。
        let row_segs = self.row_caches.entry(id).or_default();
        if row_segs.len() != rows {
            row_segs.resize_with(rows, || RowSegs {
                hash: None,
                segs: Vec::new(),
            });
        }

        for (vr, row) in grid.visible_rows().enumerate() {
            let h = hash_row(row, cols);
            if row_segs[vr].hash == Some(h) {
                continue;
            }
            row_segs[vr].hash = Some(h);
            // 取出 segs 重建（旧 buffer 复用，超出部分截断）。
            let mut segs = std::mem::take(&mut row_segs[vr].segs);
            let mut seg_count = 0usize;

            let cells = row.cells();
            let row_len = cols.min(cells.len());
            let mut c = 0usize;

            while c < row_len {
                let cell = &cells[c];
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    c += 1;
                    continue;
                }

                let cell_attrs = |cell: &lumen_term::Cell| {
                    let (fg, _) = self.theme.cell_colors(cell);
                    let mut attrs = base_attrs.clone().color(fg.to_glyphon());
                    if cell.flags.contains(CellFlags::BOLD) {
                        attrs = attrs.weight(Weight::BOLD);
                    }
                    if cell.flags.contains(CellFlags::ITALIC) {
                        attrs = attrs.style(Style::Italic);
                    }
                    attrs
                };

                // 段构建：单字符段（宽字符/非 ASCII）或 ASCII run。
                let start_col = c;
                let mut line = String::new();
                let mut spans: Vec<(usize, usize, Attrs)> = Vec::new();

                if cell.flags.contains(CellFlags::WIDE) || !cell.ch.is_ascii() {
                    line.push(cell.ch);
                    spans.push((0, line.len(), cell_attrs(cell)));
                    c += if cell.flags.contains(CellFlags::WIDE) {
                        2
                    } else {
                        1
                    };
                } else {
                    let mut run_start = 0usize;
                    let mut run_attrs: Option<Attrs> = None;
                    while c < row_len {
                        let cell = &cells[c];
                        if cell
                            .flags
                            .intersects(CellFlags::WIDE | CellFlags::WIDE_SPACER)
                            || !cell.ch.is_ascii()
                        {
                            break;
                        }
                        let attrs = cell_attrs(cell);
                        if run_attrs.as_ref() != Some(&attrs) {
                            if line.len() > run_start {
                                if let Some(a) = run_attrs.take() {
                                    spans.push((run_start, line.len(), a));
                                }
                            }
                            run_start = line.len();
                            run_attrs = Some(attrs);
                        }
                        line.push(cell.ch);
                        c += 1;
                    }
                    if line.len() > run_start {
                        if let Some(a) = run_attrs.take() {
                            spans.push((run_start, line.len(), a));
                        }
                    }
                    // 全空白段不排版（空格无字形，背景色块单独绘制）。
                    let trimmed = line.trim_end().len();
                    if trimmed == 0 {
                        continue;
                    }
                    line.truncate(trimmed);
                    spans.retain_mut(|(s, e, _)| {
                        *e = (*e).min(trimmed);
                        *s < trimmed
                    });
                }

                // 复用旧 buffer 或新建。
                if seg_count >= segs.len() {
                    segs.push((0, TextBuffer::new(&mut self.font_system, metrics)));
                }
                let (col_slot, buf) = &mut segs[seg_count];
                *col_slot = start_col;
                buf.set_metrics(&mut self.font_system, metrics);
                buf.set_size(&mut self.font_system, None, Some(ch));
                buf.set_rich_text(
                    &mut self.font_system,
                    spans.iter().map(|(s, e, a)| (&line[*s..*e], a.clone())),
                    &base_attrs,
                    Shaping::Advanced,
                    None,
                );
                seg_count += 1;
            }
            segs.truncate(seg_count);
            row_segs[vr].segs = segs;
        }

        let fg_default = self.theme.foreground.to_glyphon();
        let width = target_w as i32;

        // ---- M4.1 批C：footer 文本排版（feature = "input-editor"）——设计稿 §7.1 ----
        // 独立 glyphon TextBuffer 池，不混入 row_segs 行缓存（哈希语义不耦合）。
        // 行数少，每帧重排无性能问题（设计稿明确）。纯文本单色（高亮 M4.2）。
        // 生命周期纪律：footer_buffers 必须先于 text_areas 声明，二者同作用域，
        // TextArea 借用 footer_buffers 中的元素，在 prepare 调用后一起 drop。
        #[cfg(feature = "input-editor")]
        let footer_buffers: Vec<(usize, TextBuffer, f32, i32, f32)> = {
            // 元素：(line_idx, buf, text_y, bottom_clamp, left_x)
            let mut bufs: Vec<(usize, TextBuffer, f32, i32, f32)> = Vec::new();
            if footer_px > 0.0 {
                if let Some(cv) = composer {
                    if cv.is_visible() {
                        let footer_top = target_h as f32 - footer_px;
                        let fp = self.padding * 0.4;

                        // 编辑器正文行（Compose 态/Running 态文案）。
                        for (li, line_text) in cv.lines.iter().enumerate() {
                            if line_text.is_empty() {
                                continue;
                            }
                            // Compose 态：若有 preedit，在正文末尾内嵌预编辑文本。
                            let display_text = if cv.kind == composer_view::FooterKind::Composer {
                                if let Some(pre) = &cv.preedit {
                                    if li == cv.cursor.0 {
                                        // 在光标处插入预编辑文本（内嵌）。
                                        let base = line_text.clone();
                                        let byte_pos = cv.cursor.1.min(base.len());
                                        let mut s =
                                            String::with_capacity(base.len() + pre.text.len());
                                        s.push_str(&base[..byte_pos]);
                                        s.push_str(&pre.text);
                                        s.push_str(&base[byte_pos..]);
                                        s
                                    } else {
                                        line_text.clone()
                                    }
                                } else {
                                    line_text.clone()
                                }
                            } else {
                                line_text.clone()
                            };

                            let mut buf = TextBuffer::new(&mut self.font_system, metrics);
                            buf.set_size(&mut self.font_system, None, Some(ch));
                            buf.set_text(
                                &mut self.font_system,
                                &display_text,
                                &base_attrs,
                                Shaping::Advanced,
                                None,
                            );
                            let text_y = footer_top + fp + li as f32 * ch;
                            let bottom_clamp = ((text_y + ch) as i32).min(target_h as i32);
                            bufs.push((li, buf, text_y, bottom_clamp, fp));
                        }

                        // M4.1 批E：placeholder（空缓冲占位提示）+ ghost text 绘制。
                        // 两者均用 fg_dim 色（前景色 50% alpha）。
                        // glyphon 不支持 per-buffer 颜色覆写，用独立 Attrs color。
                        if cv.kind == composer_view::FooterKind::Composer {
                            let fg = self.theme.foreground;
                            // fg_dim = 前景色 50% alpha（与外壳 fg_dim 同语义）。
                            let dim_color = glyphon::Color::rgba(fg.0, fg.1, fg.2, 128);
                            let dim_attrs = base_attrs.clone().color(dim_color);

                            // placeholder：仅当所有行均为空时显示。
                            let all_empty = cv.lines.iter().all(|l| l.is_empty());
                            if all_empty {
                                if let Some(ph) = &cv.placeholder {
                                    if !ph.is_empty() {
                                        let mut buf =
                                            TextBuffer::new(&mut self.font_system, metrics);
                                        buf.set_size(&mut self.font_system, None, Some(ch));
                                        buf.set_text(
                                            &mut self.font_system,
                                            ph,
                                            &dim_attrs,
                                            Shaping::Advanced,
                                            None,
                                        );
                                        // 占位文字与光标同行（行 0，光标在行首）
                                        let text_y = footer_top + fp;
                                        let bottom_clamp =
                                            ((text_y + ch) as i32).min(target_h as i32);
                                        // line_idx = usize::MAX - 1 区分 placeholder 行
                                        bufs.push((usize::MAX - 1, buf, text_y, bottom_clamp, fp));
                                    }
                                }
                            }

                            // ghost text：光标在文末时在光标后追加渲染（fg_dim 色）。
                            // 条件：cursor 行有 ghost 字段且光标字节偏移 ≥ 该行长度。
                            if let Some(ghost) = &cv.ghost {
                                if !ghost.is_empty() {
                                    let (cur_line, cur_byte) = cv.cursor;
                                    let line_text =
                                        cv.lines.get(cur_line).map(|s| s.as_str()).unwrap_or("");
                                    // 光标在文末（字节偏移 ≥ 行长度）
                                    if cur_byte >= line_text.len() {
                                        // ghost 文字的 x 位置 = 行末列 × cell_w + fp
                                        // footer_byte_to_col(byte=len) 算出行末显示列（CJK 各 2 列）。
                                        let col = composer_view::footer_byte_to_col(
                                            line_text,
                                            line_text.len(),
                                        ) as f32;
                                        let ghost_x = fp + col * cw;
                                        let ghost_y = footer_top + fp + cur_line as f32 * ch;
                                        let bottom_clamp =
                                            ((ghost_y + ch) as i32).min(target_h as i32);
                                        // 超宽由 TextBounds 自然裁剪（right = target_w）
                                        let mut buf =
                                            TextBuffer::new(&mut self.font_system, metrics);
                                        buf.set_size(&mut self.font_system, None, Some(ch));
                                        buf.set_text(
                                            &mut self.font_system,
                                            ghost,
                                            &dim_attrs,
                                            Shaping::Advanced,
                                            None,
                                        );
                                        // line_idx = usize::MAX - 2 区分 ghost 行
                                        bufs.push((
                                            usize::MAX - 2,
                                            buf,
                                            ghost_y,
                                            bottom_clamp,
                                            ghost_x,
                                        ));
                                    }
                                }
                            }
                        }

                        // M4.1 批D2：退出码角标文字（✓/✗ + 耗时）。
                        // 仅 Compose 态且有角标时绘制（Running 态不显示角标文字）。
                        if cv.kind == composer_view::FooterKind::Composer {
                            if let Some(badge) = &cv.exit_badge {
                                let symbol = if badge.exit_code == 0 { "✓" } else { "✗" };
                                let ms = badge.duration_ms;
                                let duration_str = if ms < 1000 {
                                    format!("{ms}ms")
                                } else {
                                    format!("{:.1}s", ms as f64 / 1000.0)
                                };
                                let badge_text = format!("{symbol} {duration_str}");
                                let mut buf = TextBuffer::new(&mut self.font_system, metrics);
                                buf.set_size(&mut self.font_system, None, Some(ch));
                                buf.set_text(
                                    &mut self.font_system,
                                    &badge_text,
                                    &base_attrs,
                                    Shaping::Advanced,
                                    None,
                                );
                                // 角标文字靠右对齐：近似用 badge_w 对齐
                                let badge_w = cw * 6.0_f32.min(target_w as f32 / 4.0);
                                let badge_x = (target_w as f32 - badge_w - fp - fp).max(fp + fp);
                                let text_y = footer_top + (footer_px - ch) / 2.0;
                                let bottom_clamp = ((text_y + ch) as i32).min(target_h as i32);
                                // 用一个特殊 line_idx=usize::MAX 区分角标行
                                bufs.push((usize::MAX, buf, text_y, bottom_clamp, badge_x));
                            }
                        }
                    }
                }
            }
            bufs
        };

        // 终端网格 TextArea 列表 + footer TextArea（借用上方 footer_buffers）。
        let mut text_areas: Vec<TextArea> = row_segs
            .iter()
            .take(rows)
            .enumerate()
            .flat_map(|(vr, entry)| {
                let y = pad + vr as f32 * ch;
                entry.segs.iter().map(move |(col, buf)| TextArea {
                    buffer: buf,
                    left: pad + *col as f32 * cw,
                    top: y,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: y as i32,
                        right: width,
                        bottom: (y + ch) as i32,
                    },
                    default_color: fg_default,
                    custom_glyphs: &[],
                })
            })
            .collect();

        // footer TextArea（借用 footer_buffers 元素，生命周期随 text_areas drop 同帧结束）。
        #[cfg(feature = "input-editor")]
        {
            for (_li, buf, text_y, bottom_clamp, left_x) in &footer_buffers {
                text_areas.push(TextArea {
                    buffer: buf,
                    left: *left_x,
                    top: *text_y,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: *text_y as i32,
                        right: width,
                        bottom: *bottom_clamp,
                    },
                    default_color: fg_default,
                    custom_glyphs: &[],
                });
            }
        }

        self.viewport.update(
            &self.queue,
            Resolution {
                width: target_w,
                height: target_h,
            },
        );
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .context("glyphon prepare 失败")?;

        // ---- 编码与提交 ----
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lumen frame"),
            });
        {
            // 背景图模式（P13）：透明背景时 Clear 用 TRANSPARENT（RGBA=0），
            // 让 egui 层的背景图透出。不能用主题色 Clear（即使 A=0 RGB≠0
            // 也会因 premultiplied blend 把颜色加色叠加到背景图上）。
            let clear_color = if self.transparent_background {
                wgpu::Color::TRANSPARENT
            } else {
                self.theme.background.to_wgpu()
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lumen pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.rects.render(&mut pass);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("glyphon render 失败")?;
        }
        self.queue.submit(Some(encoder.finish()));
        self.atlas.trim();
        Ok(())
    }
}

/// 行内容哈希（FNV-1a）：字符 + 前景/背景色 + 样式标志。
/// 用于行级排版缓存命中判断。
fn hash_row(row: &lumen_term::Row, cols: usize) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    fn color_code(c: lumen_term::Color) -> u32 {
        match c {
            lumen_term::Color::Default => 0,
            lumen_term::Color::Indexed(i) => 0x100 | i as u32,
            lumen_term::Color::Rgb(r, g, b) => {
                0x0200_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
            }
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for cell in row.cells().iter().take(cols) {
        for v in [
            cell.ch as u32,
            color_code(cell.fg),
            color_code(cell.bg),
            cell.flags.bits() as u32,
        ] {
            h = (h ^ v as u64).wrapping_mul(PRIME);
        }
    }
    h
}

/// 解析请求的字体家族名：非空且系统中存在则用之，否则回退
/// [`pick_mono_family`] 的默认选择（设置页字体名无效不崩）。
fn resolve_family(font_system: &FontSystem, wanted: &str) -> String {
    let wanted = wanted.trim();
    if !wanted.is_empty() {
        let found = font_system.db().faces().any(|f| {
            f.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(wanted))
        });
        if found {
            return wanted.to_owned();
        }
        log::warn!("系统中未找到字体「{wanted}」，回退默认等宽字体");
    }
    pick_mono_family(font_system)
}

/// 在系统字体库中挑选等宽字体：Cascadia Mono → Consolas → 任意 Monospace。
fn pick_mono_family(font_system: &FontSystem) -> String {
    let db = font_system.db();
    for wanted in ["Cascadia Mono", "Consolas"] {
        let found = db.faces().any(|f| {
            f.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(wanted))
        });
        if found {
            return wanted.to_owned();
        }
    }
    "monospace".to_owned()
}

/// 用参考字符测量单元格物理尺寸。
fn measure_cell(font_system: &mut FontSystem, family: &str, font_size: f32) -> (f32, f32) {
    let line_height = (font_size * LINE_HEIGHT_FACTOR).ceil();
    let metrics = Metrics::new(font_size, line_height);
    let mut buf = TextBuffer::new(font_system, metrics);
    buf.set_text(
        font_system,
        "M",
        &Attrs::new().family(Family::Name(family)),
        Shaping::Advanced,
        None,
    );
    let w = buf
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().map(|g| g.w))
        .unwrap_or(font_size * 0.6);
    (w, line_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── grid_size_for_with_footer 纯逻辑测试（不依赖 GPU）─────────────────

    /// 模拟 grid_size_for_with_footer 的核心数学逻辑（提取为独立函数以便单测）。
    fn compute_grid(
        width: u32,
        height: u32,
        padding: f32,
        cell_w: f32,
        cell_h: f32,
        footer_px: f32,
    ) -> (usize, usize) {
        let usable_h = (height as f32 - padding * 2.0 - footer_px).max(0.0);
        let usable_w = (width as f32 - padding * 2.0).max(0.0);
        let rows = (usable_h / cell_h).floor() as usize;
        let cols = (usable_w / cell_w).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// 无 footer 时行列数与旧接口一致。
    #[test]
    fn grid_无_footer_等价旧接口() {
        // padding=10, cell=20x20, 窗格 640x480
        let (r0, c0) = compute_grid(640, 480, 10.0, 20.0, 20.0, 0.0);
        // usable_h = 480-20=460, rows=floor(460/20)=23
        // usable_w = 640-20=620, cols=floor(620/20)=31
        assert_eq!(r0, 23, "行数无 footer");
        assert_eq!(c0, 31, "列数无 footer");
    }

    /// 有 footer 时行数减少，列数不变（footer 只占高度）。
    #[test]
    fn grid_有_footer_行数减少() {
        // 假设 footer 占 32px（1 行 cell_h=20 + padding*2=6*2）
        let footer_px = 32.0_f32;
        let (r_with, c_with) = compute_grid(640, 480, 10.0, 20.0, 20.0, footer_px);
        let (r_without, c_without) = compute_grid(640, 480, 10.0, 20.0, 20.0, 0.0);
        assert!(
            r_with < r_without,
            "有 footer 时行数 {r_with} 应少于无 footer 时 {r_without}"
        );
        assert_eq!(c_with, c_without, "footer 不影响列数");
    }

    /// footer 存在时行数减少量等于 ceil(footer_px / cell_h)（常驻等高铁律验证）。
    ///
    /// footer_px = cell_h + padding*2（1 行内容 + 内边距），
    /// 减少行数 = ceil(footer_px / cell_h)。
    /// 常驻等高铁律保证：Compose↔Running 切换 footer_px 不变，行数不变。
    #[test]
    fn grid_footer_减少行数等于ceil除以cell_h() {
        let cell_h = 20.0_f32;
        let fp = 6.0_f32; // footer_padding
        let footer_px = cell_h + fp * 2.0; // 32px
        let (r_without, _) = compute_grid(640, 480, 10.0, 10.0, cell_h, 0.0);
        let (r_with, _) = compute_grid(640, 480, 10.0, 10.0, cell_h, footer_px);
        let expected_reduction = (footer_px / cell_h).ceil() as usize;
        assert_eq!(
            r_without - r_with,
            expected_reduction,
            "footer {footer_px}px / cell_h {cell_h}px 应减少 {expected_reduction} 行"
        );
    }

    /// 渲染字号必须与测量字号一致：用测量出的 cell_w 对照同字号下
    /// 长串 ASCII 的排版宽度，偏差超过 0.5px 即说明 advance 不匹配，
    /// 长行下光标会与文字逐字漂移（曾因从取整行高反推字号而出过此 bug）。
    #[test]
    fn 长行排版宽度与网格一致() {
        let mut fs = FontSystem::new();
        let family = pick_mono_family(&fs);
        let font_size = 15.0_f32;
        let (cell_w, cell_h) = measure_cell(&mut fs, &family, font_size);

        let n = 60usize;
        let text: String = "a".repeat(n);
        let mut buf = TextBuffer::new(&mut fs, Metrics::new(font_size, cell_h));
        buf.set_size(&mut fs, None, Some(cell_h));
        buf.set_text(
            &mut fs,
            &text,
            &Attrs::new().family(Family::Name(&family)),
            Shaping::Advanced,
            None,
        );
        let width = buf
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.last().map(|g| g.x + g.w))
            .expect("排版失败");
        let expected = n as f32 * cell_w;
        assert!(
            (width - expected).abs() < 0.5,
            "排版宽度 {width} 与网格宽度 {expected} 偏差过大"
        );
    }
}
