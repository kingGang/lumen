//! build.rs — lumen-app 构建脚本
//!
//! Windows 目标：用 winresource 把 lumen.ico 嵌入 PE 资源区段，
//! 使文件管理器缩略图、快捷方式、任务栏非运行态均显示应用图标。
//! 非 Windows 目标：本脚本为空操作，不引入任何额外依赖。

fn main() {
    #[cfg(target_os = "windows")]
    {
        embed_icon();
        copy_conpty_assets();
    }
}

/// 把随仓库 vendored 的 ConPTY 宿主（`conpty.dll` + `OpenConsole.exe`）拷到
/// 构建输出目录（`lumen.exe` 同目录）。
///
/// portable-pty 0.9 会**优先加载同目录的 `conpty.dll`**（见其 `psuedocon.rs`
/// `load_conpty`），用现代 `OpenConsole.exe` 托管 ConPTY，替代 Windows 10 上
/// 偏旧的系统 conhost。旧系统 conhost 会让 Claude Code 等 TUI 判定终端「会
/// ConPTY 重渲染」而降级——不进备用屏、不开鼠标上报——导致 Win10 上会话内容
/// 无法用滚轮滚动（海风哥 2026-06-30 实测：同一份 Lumen，Win11 正常、Win10
/// 失败，差异仅在 ConPTY 宿主；WT/Warp 也是靠自带 OpenConsole 规避）。
/// 二进制取自微软可再发行的 ConPTY 包（x64）。
#[cfg(target_os = "windows")]
fn copy_conpty_assets() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let asset_dir = std::path::Path::new(&manifest_dir)
        .join("assets")
        .join("windows")
        .join("x64");

    // 由 OUT_DIR 反推构建输出目录 target/<profile>/（lumen.exe 所在）：
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out，上溯 3 级即得。
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let Some(target_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) else {
        eprintln!("cargo:warning=无法由 OUT_DIR 定位输出目录，跳过 ConPTY 资产拷贝");
        return;
    };

    for file in ["conpty.dll", "OpenConsole.exe"] {
        let src = asset_dir.join(file);
        let dst = target_dir.join(file);
        // 资产变化时重跑本脚本。
        println!("cargo:rerun-if-changed={}", src.display());
        if let Err(e) = std::fs::copy(&src, &dst) {
            // 拷贝失败仅警告：Win11 因系统 conhost 已够新仍可工作，
            // 仅 Win10 受影响——不阻断构建。
            eprintln!(
                "cargo:warning=拷贝 {file} 到 {} 失败（Win10 上 Claude 可能无法滚动）：{e}",
                dst.display()
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn embed_icon() {
    // CARGO_MANIFEST_DIR 指向 crates/lumen-app/（build.rs 的 crate 根）。
    // icons/ 目录位于工作区根，相对路径为 ../../icons/lumen.ico。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icon_path = std::path::Path::new(&manifest_dir)
        .join("..") // crates/
        .join("..") // workspace root
        .join("icons")
        .join("lumen.ico");

    // 告知 Cargo：图标文件变化时重跑本脚本。
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon_path.to_str().expect("icon path is valid UTF-8"));
    if let Err(e) = res.compile() {
        // 图标嵌入失败仅打印警告，不阻断构建。
        // 常见原因：CI 环境缺少 Windows SDK rc.exe；本机开发一般不会触发。
        eprintln!("cargo:warning=嵌入 exe 图标失败（不影响功能）：{e}");
    }
}
