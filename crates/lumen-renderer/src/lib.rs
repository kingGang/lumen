//! Lumen 的渲染层：wgpu surface 管理 + glyphon 文本渲染 + 矩形管线。
//!
//! 每帧流程：Grid → (背景/光标/下划线矩形, 每行 rich text) → GPU。
//!
//! M3 起终端内容渲染到**持久离屏纹理**（egui 以 `ui.image` 把它嵌进
//! 工作区），surface 的取帧/呈现所有权上移到 app 层（egui pass 是
//! surface 的唯一写入方）。

mod rect;
mod theme;

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
    texture: wgpu::Texture,
    /// 终端管线的渲染目标视图（纹理本身格式）。
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
            texture,
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
    /// 终端内容的离屏渲染目标（尺寸 = 终端区物理像素）。
    offscreen: Offscreen,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// 行级排版缓存：行内容（哈希）不变则整行跳过 shaping。
    /// TUI 全屏界面（如 codex 的框线边框）段数巨大，没有缓存时
    /// 每帧全量整形会把打字回显拖卡。
    row_segs: Vec<RowSegs>,
    rects: rect::RectRenderer,

    theme: Theme,
    font_family: String,
    /// 渲染与测量必须用同一字号：行高经过取整，从行高反推
    /// 字号会放大 advance，长行下光标与文字逐字漂移。
    font_size: f32,
    cell_w: f32,
    cell_h: f32,
    /// 内边距（物理像素）。
    padding: f32,
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
        // 初始离屏纹理先按整窗尺寸建；app 层拿到 egui 布局的终端区
        // 矩形后用 ensure_offscreen 重建到实际尺寸。
        let offscreen = Offscreen::new(&device, format, config.width, config.height);

        let mut font_system = FontSystem::new();
        let font_family = pick_mono_family(&font_system);
        log::info!("使用等宽字体: {font_family}");

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(
            &mut atlas,
            &device,
            wgpu::MultisampleState::default(),
            None,
        );
        let rects = rect::RectRenderer::new(&device, format);

        let font_size = FONT_SIZE * scale_factor;
        let (cell_w, cell_h) = measure_cell(&mut font_system, &font_family, font_size);
        log::info!("单元格尺寸: {cell_w}x{cell_h} 物理像素");

        Ok(Self {
            device,
            queue,
            surface,
            config,
            offscreen,
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            row_segs: Vec::new(),
            rects,
            theme: Theme::default(),
            font_family,
            font_size,
            cell_w,
            cell_h,
            padding: PADDING * scale_factor,
        })
    }

    /// 单元格物理像素尺寸（app 用它换算终端区尺寸 ↔ 行列数）。
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// 给定终端区物理像素尺寸能容纳的 (rows, cols)（扣除四周内边距）。
    pub fn grid_size_for(&self, width: u32, height: u32) -> (usize, usize) {
        let usable_h = (height as f32 - self.padding * 2.0).max(0.0);
        let usable_w = (width as f32 - self.padding * 2.0).max(0.0);
        let rows = (usable_h / self.cell_h).floor() as usize;
        let cols = (usable_w / self.cell_w).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// 终端区内像素坐标（相对终端区原点）→ 视图格子坐标（行, 列），
    /// 自动夹紧到网格范围。
    pub fn cell_at(&self, px: f64, py: f64) -> (usize, usize) {
        let (rows, cols) = self.grid_size_for(self.offscreen.width, self.offscreen.height);
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

    /// 终端内容的离屏纹理。
    pub fn offscreen_texture(&self) -> &wgpu::Texture {
        &self.offscreen.texture
    }

    /// 供 egui 采样的离屏视图（非 sRGB 重解释，缘由见 [`Offscreen`]）。
    pub fn offscreen_view(&self) -> &wgpu::TextureView {
        &self.offscreen.sample_view
    }

    /// 确保离屏纹理为指定尺寸；尺寸变化时重建并返回 true
    /// （调用方需重新把新视图绑定到 egui 纹理 id）。
    pub fn ensure_offscreen(&mut self, width: u32, height: u32) -> bool {
        let (width, height) = (width.max(1), height.max(1));
        if (self.offscreen.width, self.offscreen.height) == (width, height) {
            return false;
        }
        self.offscreen = Offscreen::new(&self.device, self.config.format, width, height);
        true
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

    /// 渲染一帧终端内容到离屏纹理（不触碰 surface，呈现由 app 层的
    /// egui pass 完成）。
    ///
    /// `selection` 为当前鼠标选区（绝对行号定位）；`cursor` 为要绘制的
    /// 光标屏幕坐标（None 不画）——由上层做位置防抖后传入，不直接读
    /// grid 光标，避免把 TUI 重绘期间的临时停留位画上屏；
    /// `selected_block` 为选中命令块的 id（块背景高亮）。
    pub fn render(
        &mut self,
        term: &Terminal,
        selection: Option<&Selection>,
        cursor: Option<(usize, usize)>,
        selected_block: Option<u64>,
    ) -> Result<()> {
        // 离屏视图按值克隆（Arc 浅拷贝），避免长借用 self 卡住后续字段访问。
        let view = self.offscreen.render_view.clone();
        let (target_w, target_h) = (self.offscreen.width, self.offscreen.height);

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
        if !term.is_alt_screen() {
            let bar_x = pad * 0.2;
            let bar_w = (pad * 0.3).max(2.0);
            for vr in 0..rows {
                let abs_line = view_top_abs + vr as u64;
                let Some(block) = term.block_at_line(abs_line) else {
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
                let selected = selection
                    .is_some_and(|s| !s.is_empty() && s.contains(abs_line, c));
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
        if let Some((cur_row, cur_col)) = cursor {
            let cursor_view_row = grid.display_offset() + cur_row;
            if cur_row < rows && cur_col < cols && cursor_view_row < rows {
                let on_wide = grid
                    .row(cur_row)
                    .cells()
                    .get(cur_col)
                    .is_some_and(|c| c.flags.contains(CellFlags::WIDE));
                instances.push(rect::RectInstance {
                    pos: [
                        pad + cur_col as f32 * cw,
                        pad + cursor_view_row as f32 * ch,
                    ],
                    size: [if on_wide { cw * 2.0 } else { cw }, ch],
                    color: self.theme.cursor.to_linear_f32(0.55),
                });
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

        if self.row_segs.len() != rows {
            self.row_segs.resize_with(rows, || RowSegs {
                hash: None,
                segs: Vec::new(),
            });
        }

        for (vr, row) in grid.visible_rows().enumerate() {
            let h = hash_row(row, cols);
            if self.row_segs[vr].hash == Some(h) {
                continue;
            }
            self.row_segs[vr].hash = Some(h);
            // 取出 segs 重建（旧 buffer 复用，超出部分截断）。
            let mut segs = std::mem::take(&mut self.row_segs[vr].segs);
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
                    c += if cell.flags.contains(CellFlags::WIDE) { 2 } else { 1 };
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
            self.row_segs[vr].segs = segs;
        }

        let fg_default = self.theme.foreground.to_glyphon();
        let width = target_w as i32;
        let text_areas: Vec<TextArea> = self
            .row_segs
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
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lumen pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.theme.background.to_wgpu()),
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
