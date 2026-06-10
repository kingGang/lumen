//! Lumen 的渲染层：wgpu surface 管理 + glyphon 文本渲染 + 矩形管线。
//!
//! 每帧流程：Grid → (背景/光标/下划线矩形, 每行 rich text) → GPU。

mod rect;
mod theme;

use anyhow::{Context, Result};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use lumen_term::{CellFlags, Terminal};

pub use theme::{Rgb, Theme};
pub use wgpu;

/// 字号（逻辑像素）与行高倍率。
const FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT_FACTOR: f32 = 1.35;

/// 终端渲染器。持有 GPU 资源与字体系统。
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// 文本段排版 buffer 池（跨帧复用，按需增长）。
    text_buffers: Vec<TextBuffer>,
    rects: rect::RectRenderer,

    theme: Theme,
    font_family: String,
    cell_w: f32,
    cell_h: f32,
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
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
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
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            text_buffers: Vec::new(),
            rects,
            theme: Theme::default(),
            font_family,
            cell_w,
            cell_h,
        })
    }

    /// 单元格物理像素尺寸（app 用它换算窗口尺寸 ↔ 行列数）。
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// 当前能容纳的 (rows, cols)。
    pub fn grid_size(&self) -> (usize, usize) {
        let rows = (self.config.height as f32 / self.cell_h).floor() as usize;
        let cols = (self.config.width as f32 / self.cell_w).floor() as usize;
        (rows.max(1), cols.max(1))
    }

    /// 窗口物理尺寸变化。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// 渲染一帧。
    pub fn render(&mut self, term: &Terminal) -> Result<()> {
        use wgpu::CurrentSurfaceTexture;
        let frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(f) | CurrentSurfaceTexture::Suboptimal(f) => f,
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            // Timeout/Occluded/校验错误：跳过本帧。
            _ => return Ok(()),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let grid = term.grid();
        let rows = grid.rows();
        let cols = grid.cols();
        let (cw, ch) = (self.cell_w, self.cell_h);

        // ---- 收集矩形：背景色块、下划线/删除线、光标 ----
        let mut instances: Vec<rect::RectInstance> = Vec::new();
        for (vr, row) in grid.visible_rows().enumerate() {
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
                let (x, y) = (c as f32 * cw, vr as f32 * ch);
                if bg != self.theme.background {
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
        // 光标：跟随底部时绘制在可视区对应行（半透明块，文字仍可见）；
        // 落在宽字符上时画两格宽。
        let cursor = grid.cursor;
        let cursor_view_row = grid.display_offset() + cursor.row;
        if cursor.visible && cursor_view_row < rows {
            let on_wide = grid
                .row(cursor.row)
                .cells()
                .get(cursor.col)
                .is_some_and(|c| c.flags.contains(CellFlags::WIDE));
            instances.push(rect::RectInstance {
                pos: [cursor.col as f32 * cw, cursor_view_row as f32 * ch],
                size: [if on_wide { cw * 2.0 } else { cw }, ch],
                color: self.theme.cursor.to_linear_f32(0.55),
            });
        }
        self.rects.prepare(
            &self.device,
            &self.queue,
            (self.config.width, self.config.height),
            &instances,
        );

        // ---- 文本排版：按网格分段强制对齐 ----
        // 窄字符连续成段、宽字符（CJK 等）单独成段，每段起点钉死在
        // col * cell_w。CJK fallback 字体的字形宽度往往 ≠ 2*cell_w，
        // 整行自由排版会让偏差逐字累计（光标与文字渐行渐远）。
        let font_size = self.cell_h / LINE_HEIGHT_FACTOR;
        let metrics = Metrics::new(font_size, self.cell_h);
        let family = self.font_family.clone();
        let base_attrs = Attrs::new().family(Family::Name(&family));

        // (buffer 池索引, 像素 x, 像素 y)
        let mut placed: Vec<(usize, f32, f32)> = Vec::new();
        let mut used = 0usize;

        for (vr, row) in grid.visible_rows().enumerate() {
            let cells = row.cells();
            let y = vr as f32 * ch;
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

                if cell.flags.contains(CellFlags::WIDE) {
                    // 宽字符单独成段，独立定位。
                    let mut tmp = [0u8; 4];
                    let s: &str = cell.ch.encode_utf8(&mut tmp);
                    let attrs = cell_attrs(cell);
                    while used >= self.text_buffers.len() {
                        self.text_buffers
                            .push(TextBuffer::new(&mut self.font_system, metrics));
                    }
                    let buf = &mut self.text_buffers[used];
                    buf.set_metrics(&mut self.font_system, metrics);
                    buf.set_size(&mut self.font_system, None, Some(ch));
                    buf.set_rich_text(
                        &mut self.font_system,
                        [(s, attrs)],
                        &base_attrs,
                        Shaping::Advanced,
                        None,
                    );
                    placed.push((used, c as f32 * cw, y));
                    used += 1;
                    c += 2; // 跳过右半占位格
                    continue;
                }

                // 窄字符 run：收集到下一个宽字符或行尾，段内按属性切 span。
                let start_col = c;
                let mut line = String::new();
                let mut spans: Vec<(usize, usize, Attrs)> = Vec::new();
                let mut run_start = 0usize;
                let mut run_attrs: Option<Attrs> = None;
                while c < row_len {
                    let cell = &cells[c];
                    if cell
                        .flags
                        .intersects(CellFlags::WIDE | CellFlags::WIDE_SPACER)
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
                // 全空白段不排版（空格无字形，背景色块已单独绘制）。
                let trimmed_len = line.trim_end().len();
                if trimmed_len == 0 {
                    continue;
                }
                while used >= self.text_buffers.len() {
                    self.text_buffers
                        .push(TextBuffer::new(&mut self.font_system, metrics));
                }
                let buf = &mut self.text_buffers[used];
                buf.set_metrics(&mut self.font_system, metrics);
                buf.set_size(&mut self.font_system, None, Some(ch));
                buf.set_rich_text(
                    &mut self.font_system,
                    spans
                        .iter()
                        .filter(|(s, _, _)| *s < trimmed_len)
                        .map(|(s, e, a)| (&line[*s..(*e).min(trimmed_len)], a.clone())),
                    &base_attrs,
                    Shaping::Advanced,
                    None,
                );
                placed.push((used, start_col as f32 * cw, y));
                used += 1;
            }
        }

        let text_areas: Vec<TextArea> = placed
            .iter()
            .map(|&(i, x, y)| TextArea {
                buffer: &self.text_buffers[i],
                left: x,
                top: y,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: y as i32,
                    right: self.config.width as i32,
                    bottom: (y + ch) as i32,
                },
                default_color: self.theme.foreground.to_glyphon(),
                custom_glyphs: &[],
            })
            .collect();

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
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
        frame.present();
        self.atlas.trim();
        Ok(())
    }
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
