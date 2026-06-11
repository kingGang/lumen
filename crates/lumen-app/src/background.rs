//! 终端背景图片加载与 GPU 纹理管理（P13）。
//!
//! 职责：从文件解码图片 → 转 premultiplied RGBA8 → 上传 wgpu 纹理 →
//! 注册 egui TextureId。启动时（enabled 且有 path）与路径变更时加载；
//! 加载失败 toast error，本次运行视为未启用（不改写 settings）；
//! 换图/清除时旧纹理 free 防泄漏；边长 >8192 拒绝并 toast。
//!
//! 本模块纯 CPU 解码（`image` crate），无 GPU 解码。jpeg 全不透明，
//! premultiply 转换无损；png/webp/bmp 带 alpha 时按标准公式转换：
//! `premul = round(c * a / 255)`（在 sRGB 字节域执行，与纹理格式 Rgba8Unorm 配合
//! ——GPU 不对 Rgba8Unorm 自动反伽马，egui 着色器期望非 sRGB-aware 输入）。
//!
//! wgpu 纹理格式：`Rgba8Unorm`（egui 着色器注释："We expect normal textures
//! that are NOT sRGB-aware"，`register_native_texture` 同样要求 Rgba8Unorm）。
//! FilterMode：`Linear`（图片缩放抗锯齿）。

use lumen_renderer::wgpu;

use crate::settings::BACKGROUND_MAX_DIM;

/// 背景图加载结果。
pub struct BgTexture {
    /// 在 egui 渲染器注册的纹理 id（用于 `painter.image`）。
    pub texture_id: egui::TextureId,
    /// 图片原始宽度（像素）。
    pub width: u32,
    /// 图片原始高度（像素）。
    pub height: u32,
}

/// 图片加载错误（内部，转 `anyhow::Error` 前收集原因）。
#[derive(Debug)]
enum LoadError {
    /// 图片边长超过安全上限，拒绝加载。
    TooLarge(u32, u32),
    /// 文件读取或图片解码失败。
    Decode(anyhow::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(w, h) => write!(
                f,
                "图片尺寸 {w}×{h} 超过安全上限 {BACKGROUND_MAX_DIM}px，已拒绝加载"
            ),
            Self::Decode(e) => write!(f, "图片解码失败：{e:#}"),
        }
    }
}

/// 从文件路径加载图片并上传 GPU，返回注册后的 egui 纹理。
///
/// # Errors
///
/// - 文件不存在或无读权限 → `LoadError::Decode`
/// - 图片格式不支持或损坏 → `LoadError::Decode`
/// - 图片任意边长 > [`BACKGROUND_MAX_DIM`] → `LoadError::TooLarge`
pub fn load_background_texture(
    path: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    egui_renderer: &mut egui_wgpu::Renderer,
) -> Result<BgTexture, String> {
    let result = load_inner(path, device, queue, egui_renderer);
    result.map_err(|e| e.to_string())
}

fn load_inner(
    path: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    egui_renderer: &mut egui_wgpu::Renderer,
) -> Result<BgTexture, LoadError> {
    // 解码图片为 RGBA8。
    let img = image::open(path)
        .map_err(|e| LoadError::Decode(anyhow::anyhow!("{e}")))?
        .into_rgba8();
    let (w, h) = img.dimensions();

    // 边长安全检查：防止显存炸。
    if w > BACKGROUND_MAX_DIM || h > BACKGROUND_MAX_DIM {
        return Err(LoadError::TooLarge(w, h));
    }

    // 转 premultiplied alpha：egui 纹理采样约定。
    // jpeg 全不透明（alpha=255），premultiply 无损；
    // png/webp/bmp 带 alpha 时按公式转换。
    let premul = premultiply_rgba8(img.as_raw(), w, h);

    // 上传 wgpu 纹理（Rgba8Unorm：egui 着色器期望非 sRGB-aware 输入，
    // register_native_texture 文档要求此格式；使用 Rgba8UnormSrgb 会导致
    // GPU 自动反伽马与 egui 着色器的 gamma→linear 转换叠加，画面整体偏暗）。
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lumen bg texture"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &premul,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * w),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let texture_id = egui_renderer.register_native_texture(device, &view, wgpu::FilterMode::Linear);

    Ok(BgTexture {
        texture_id,
        width: w,
        height: h,
    })
}

/// 将直接 RGBA8 数组转为 premultiplied RGBA8。
///
/// egui 的纹理采样约定为 premultiplied alpha：
/// `out_r = round(in_r * alpha / 255)`，GB 同理，A 不变。
/// jpeg 全不透明（alpha=255），out == in；png/webp 带 alpha 时转换有效。
///
/// 配合 `Rgba8Unorm` 纹理格式使用：GPU 不对 Rgba8Unorm 自动反伽马，
/// 因此在 sRGB 字节域直接执行 `round(c * a / 255)` 是正确的——
/// 存入纹理的 sRGB 字节与 egui 着色器期望的"gamma 编码 premultiplied"完全一致。
///
/// # 示例
///
/// - alpha=255：`premultiply_rgba8(&[200,100,50,255],1,1)` → `[200,100,50,255]`（不变）
/// - alpha=0：`premultiply_rgba8(&[200,100,50,0],1,1)` → `[0,0,0,0]`（全零）
/// - alpha=128：各通道约减半，详见单测 `premultiply_alpha128_近似half`
pub fn premultiply_rgba8(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    // width 和 height 用于计算容量预分配，节省重分配开销。
    let pixel_count = (width as usize).saturating_mul(height as usize);
    let mut out = Vec::with_capacity(pixel_count * 4);
    for chunk in data.chunks_exact(4) {
        let (r, g, b, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        if a == 255 {
            // 全不透明：无需运算。
            out.extend_from_slice(chunk);
        } else if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let af = a as u16;
            out.push(((r as u16 * af + 127) / 255) as u8);
            out.push(((g as u16 * af + 127) / 255) as u8);
            out.push(((b as u16 * af + 127) / 255) as u8);
            out.push(a);
        }
    }
    out
}

/// 计算背景图的 cover UV（保宽高比、居中裁剪填满 `rect_size`）。
///
/// 返回 `(min_uv, max_uv)` 供 `painter.image(tex, rect, [min_uv, max_uv], tint)` 使用。
/// cover 语义：图片短边缩放到恰好填满 rect，超出 rect 的长边两侧各裁一半。
///
/// # 示例
///
/// - 图片与 rect 同比（800×600 图 + 800×600 rect）：UV 全覆盖 \[0,0\]～\[1,1\]
/// - 横图 2:1 填方形 rect：高对齐，UV x 从 0.25 到 0.75，详见单测
/// - 竖图 1:2 填方形 rect：宽对齐，UV y 从 0.25 到 0.75，详见单测
pub fn cover_uv(img_w: f32, img_h: f32, rect_w: f32, rect_h: f32) -> ([f32; 2], [f32; 2]) {
    if img_w <= 0.0 || img_h <= 0.0 || rect_w <= 0.0 || rect_h <= 0.0 {
        return ([0.0, 0.0], [1.0, 1.0]);
    }
    let img_ratio = img_w / img_h;
    let rect_ratio = rect_w / rect_h;
    // scale = 图片在 rect 内覆盖时的缩放比（短边对齐 rect 对应边）。
    let (u_size, v_size) = if img_ratio > rect_ratio {
        // 横图比 rect 更宽：高对齐，左右裁。
        let u_size = rect_ratio / img_ratio;
        (u_size, 1.0)
    } else {
        // 竖图比 rect 更高：宽对齐，上下裁。
        let v_size = img_ratio / rect_ratio;
        (1.0, v_size)
    };
    let u0 = (1.0 - u_size) / 2.0;
    let v0 = (1.0 - v_size) / 2.0;
    ([u0, v0], [u0 + u_size, v0 + v_size])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_alpha255_不变() {
        let src = [200u8, 150, 80, 255];
        let out = premultiply_rgba8(&src, 1, 1);
        assert_eq!(out, src);
    }

    #[test]
    fn premultiply_alpha0_全零() {
        let out = premultiply_rgba8(&[200u8, 150, 80, 0], 1, 1);
        assert_eq!(out, [0, 0, 0, 0]);
    }

    #[test]
    fn premultiply_alpha128_近似half() {
        let out = premultiply_rgba8(&[200u8, 100, 50, 128], 1, 1);
        assert_eq!(out[3], 128);
        // 200 * 128 / 255 ≈ 100.4 → 100
        assert!((out[0] as i32 - 100).abs() <= 1, "R premul = {}", out[0]);
        // 100 * 128 / 255 ≈ 50.2 → 50
        assert!((out[1] as i32 - 50).abs() <= 1, "G premul = {}", out[1]);
        // 50 * 128 / 255 ≈ 25.1 → 25
        assert!((out[2] as i32 - 25).abs() <= 1, "B premul = {}", out[2]);
    }

    #[test]
    fn premultiply_多像素() {
        // 第一像素全不透明，第二像素全透明。
        let src = [255u8, 128, 64, 255, 100, 50, 25, 0];
        let out = premultiply_rgba8(&src, 2, 1);
        assert_eq!(&out[0..4], &[255, 128, 64, 255]);
        assert_eq!(&out[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn cover_uv_同比() {
        let (mn, mx) = cover_uv(800.0, 600.0, 800.0, 600.0);
        assert!((mn[0]).abs() < 1e-6);
        assert!((mn[1]).abs() < 1e-6);
        assert!((mx[0] - 1.0).abs() < 1e-6);
        assert!((mx[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cover_uv_横图填方形_裁左右() {
        // 图 2:1，rect 1:1 → 高对齐，裁左右各 25%。
        let (mn, mx) = cover_uv(200.0, 100.0, 100.0, 100.0);
        assert!((mn[0] - 0.25).abs() < 1e-6, "u0 = {}", mn[0]);
        assert!((mn[1]).abs() < 1e-6, "v0 = {}", mn[1]);
        assert!((mx[0] - 0.75).abs() < 1e-6, "u1 = {}", mx[0]);
        assert!((mx[1] - 1.0).abs() < 1e-6, "v1 = {}", mx[1]);
    }

    #[test]
    fn cover_uv_竖图填方形_裁上下() {
        // 图 1:2，rect 1:1 → 宽对齐，裁上下各 25%。
        let (mn, mx) = cover_uv(100.0, 200.0, 100.0, 100.0);
        assert!((mn[0]).abs() < 1e-6, "u0 = {}", mn[0]);
        assert!((mn[1] - 0.25).abs() < 1e-6, "v0 = {}", mn[1]);
        assert!((mx[0] - 1.0).abs() < 1e-6, "u1 = {}", mx[0]);
        assert!((mx[1] - 0.75).abs() < 1e-6, "v1 = {}", mx[1]);
    }

    #[test]
    fn cover_uv_极端比例_宽图() {
        // 图 4:1，rect 1:1 → 高对齐，UV x 从 0.375 到 0.625。
        let (mn, mx) = cover_uv(400.0, 100.0, 100.0, 100.0);
        let expected_u0 = 0.375f32;
        let expected_u1 = 0.625f32;
        assert!((mn[0] - expected_u0).abs() < 1e-5, "u0 = {}", mn[0]);
        assert!((mx[0] - expected_u1).abs() < 1e-5, "u1 = {}", mx[0]);
    }

    #[test]
    fn cover_uv_图片与rect宽高比相同_无裁剪() {
        // 图 16:9，rect 16:9 → 无裁剪。
        let (mn, mx) = cover_uv(1920.0, 1080.0, 1600.0, 900.0);
        assert!((mn[0]).abs() < 1e-5);
        assert!((mn[1]).abs() < 1e-5);
        assert!((mx[0] - 1.0).abs() < 1e-5);
        assert!((mx[1] - 1.0).abs() < 1e-5);
    }
}
