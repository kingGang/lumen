# Lumen

一个类 Warp 的现代 GPU 加速终端。Rust 编写，Windows 优先。

## 当前状态：M1 终端底座 ✅

- ConPTY 跑 PowerShell（优先 pwsh，回退 powershell）
- VT100/ANSI 解析：光标控制、SGR 16/256/真彩、擦除、滚动区、备用屏幕
- wgpu GPU 渲染：glyphon 文本 + 自绘矩形管线（背景色块/光标/下划线）
- 10k 行 scrollback，鼠标滚轮 / Shift+PgUp/PgDn 翻屏
- IME 中文输入、DSR/DA 终端应答、OSC 0/2 窗口标题
- OSC 133 命令块边界采集（M2 Blocks UI 的数据基础）

## 构建运行

```powershell
cargo run --release
```

要求：Rust 1.85+，Windows 10 1809+（ConPTY）。

## 架构

```
crates/
├── lumen-pty/       # PTY 抽象（portable-pty / ConPTY）
├── lumen-term/      # VT 解析 + Grid + Block 模型（纯数据，无图形依赖）
├── lumen-renderer/  # wgpu + glyphon 渲染
└── lumen-app/       # winit 主程序
```

详见 [docs/架构设计.md](docs/架构设计.md)。

## 路线图

- **M1** 终端底座 ✅
- **M2** Blocks UI：块折叠/复制/跳转、shell integration 注入
- **M3** 现代输入编辑器：多行编辑、高亮、补全、历史搜索
- **M4** AI 集成：自然语言转命令、报错解释
