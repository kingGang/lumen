# Lumen

**A modern, GPU-accelerated terminal** — written in Rust, Windows-first, in the spirit of Warp.

**English** · [简体中文](README.zh-CN.md)

<p align="center">
  <img src="docs/demo.gif" alt="Lumen demo: syntax-highlighted commands, live output, up to 6 split panes (3 top / 3 bottom)" width="880">
</p>

Lumen brings GPU-smooth rendering, command blocks, a modern input editor, multi-pane
splits, built-in themes, and a multilingual UI together in a native Windows terminal —
the goal is to make *typing commands* feel as fluid as working in a modern code editor.

> Status: the terminal core, the app shell, and the modern input editor are all in
> place and being actively polished.

---

## ✨ Features

### Terminal core
- **PowerShell over ConPTY**: prefers `pwsh`, falls back to `powershell`.
- **Full VT100/ANSI parsing**: SGR 16/256/true color, cursor control, erase, scroll
  regions, alternate screen (vim/less and other full-screen apps), bracketed paste,
  DEC 2026 synchronized updates.
- **GPU rendering**: wgpu + glyphon text + a custom rectangle pipeline (background
  cells / cursor / underline); no redraws when idle, with frame coalescing and rate
  limiting — it never busy-spins the CPU.
- **10k-line scrollback**: mouse wheel, `Shift+PgUp/PgDn`.
- **CJK IME input**: inline preedit, candidate window that follows the cursor.
- **Command Blocks**: command boundaries captured via shell-integration OSC 133; a
  left-edge status bar marks running (blue) / success (green) / failure (red); click a
  block to select its output.

### Modern input editor
- **Multi-line command editing**: a dedicated footer input area; `Shift+Enter` for newlines.
- **PowerShell syntax highlighting**: commands / keywords / parameters / variables /
  numbers / strings / comments / operators each get their own color, matched to the
  active theme.
- **Continuation detection**: while quotes / pipes are unclosed, Enter inserts a newline
  instead of submitting.
- **Command history**: `↑/↓` navigation, draft recovery; PSReadLine history is imported
  on first launch.
- **Fuzzy history search**: `Ctrl+R` opens a search panel (subsequence matching +
  frequency/recency weighting, match highlighting, exit-code badges); Enter fills the
  input area without executing.
- **Tab completion**: local file-path completion + background `pwsh` sidecar command
  completion.
- **Ghost text**: the best matching history entry is shown inline in gray; press `→`/`End` to accept.
- **Exit-code badge**: ✓/✗ and elapsed time shown when a command finishes.
- **Classic passthrough mode**: `Ctrl+Shift+E` switches back to traditional
  byte-by-byte passthrough (via PSReadLine).

### UI & appearance
- **Custom title bar**: borderless window with the title bar fused into the top bar
  (Warp/VSCode style); drag, double-click to maximize, Win11 Snap Layouts.
- **11 built-in themes**: Lumen Dark/Light, Tokyo Night (dark/light), Dracula, Nord,
  Gruvbox, Solarized (dark/light), Catppuccin, One Dark; a theme gallery preview in
  settings, plus **Sync with OS** to follow the system light/dark mode automatically.
- **Internationalization (i18n)**: Simplified Chinese / Traditional Chinese / English,
  switchable instantly in settings and persisted.
- **Terminal background image**: pick a local image with opacity / dimming sliders
  (keeping text readable).
- **File tree**: right-click to create files/folders, delete to the Recycle Bin, enter a
  folder (cd), copy its absolute/relative path, reveal in File Explorer; drag a file onto
  the terminal to insert its path; recursive search at the top.
- **Clickable links**: URLs / file paths (including `:line:col`) / OSC 8 hyperlinks in the
  terminal show an underline and tooltip on hover; `Ctrl+Click` to open (URL → browser,
  file → default program).
- **System notifications**: auto-dismissing toasts in the bottom-right corner, with
  severity levels (info / warning / error).

### Multi-pane splits
- Up to **6 panes** per session, with fixed even layouts (1 full / 2 side-by-side / … /
  6 as 3-top-3-bottom).
- Pane ratios are **drag-adjustable** (dividers change the cursor on hover, double-click
  restores the even split).
- Each pane has its own title bar (showing cwd, close / maximize buttons); **drag a
  title bar to swap pane positions**.
- **Maximize/restore** a pane; click a pane to focus it — keyboard / IME / file tree all
  follow the focused pane.

### Sessions & window
- **Session persistence**: the session list and each pane's cwd / ratio / maximized state
  are restored on restart (screen contents are not restored; shells are relaunched).
- **Single instance**: the release build is single-instance (a second instance brings the
  existing window to the front and then exits); the debug build, or `--multi-instance` /
  `LUMEN_MULTI_INSTANCE=1`, allows multiple instances.
- **Maximized on launch by default**; sidebar / file-tree column widths are
  drag-adjustable and persisted.
- **Auto-update**: checks the latest GitHub Release on launch, prompts when a new version
  is available, and downloads + installs + restarts in one click (toggle / manual check
  under Settings → About → Updates).

---

## 📦 Build & run

Requires **Rust 1.85+** and **Windows 10 1809+** (ConPTY).

```powershell
# Dev run (recommended — fast compile, with console logging)
cargo run

# Release build
cargo build --release
# Output: target\release\lumen.exe
```

The optional `input-editor` feature (the modern input editor) is on by default;
`--no-default-features` removes it entirely, falling back to a traditional byte-stream
terminal.

---

## ⌨️ Keyboard shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+T` | New session |
| `Ctrl+W` | Close current session |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous session |
| `Ctrl+B` | Toggle file tree |
| `Ctrl+,` | Open / close settings |
| `Ctrl+↑` / `Ctrl+↓` | Jump between command blocks |
| `Ctrl+C` | Copy selection / selected block output; send interrupt when nothing is selected |
| `Ctrl+V` / `Shift+Insert` | Paste |
| `Shift+PgUp` / `Shift+PgDn` | Scroll up / down |
| `Esc` | Close settings / overlay |
| **Splits** | |
| `Ctrl+Shift+D` | Add pane |
| `Ctrl+Shift+W` | Close pane |
| `Ctrl+Shift+Enter` | Maximize / restore pane |
| **Input editor** | |
| `↑` / `↓` | Command history navigation |
| `Ctrl+R` | Fuzzy history search |
| `Tab` | Completion (file path / command) |
| `Shift+Enter` | Multi-line newline |
| `Ctrl+Shift+E` | Toggle classic passthrough mode |
| `Ctrl+Click` | Open a link / file in the terminal |

---

## 🏗️ Architecture

```
crates/
├── lumen-pty/       # PTY abstraction (portable-pty / ConPTY)
├── lumen-term/      # VT parsing + Grid + Block model (pure data, no graphics)
├── lumen-editor/    # Input-editor state machine (multi-line / cursor / undo / highlighting, pure logic)
├── lumen-renderer/  # wgpu + glyphon rendering
└── lumen-app/       # winit main program + egui shell (top bar / sidebar / file tree / settings / splits)
```

Data flow: PTY bytes → the `lumen-term` state machine → `Grid` → `lumen-renderer` to the
screen; keyboard / mouse / IME → `lumen-app` routing → (in editor mode) `lumen-editor` →
submitted back to the PTY.

See [docs/架构设计.md](docs/架构设计.md) and
[docs/输入编辑器设计.md](docs/输入编辑器设计.md) for details (in Chinese).

---

## 🗺️ Roadmap

- **Terminal core** ✅ ConPTY / VT parsing / GPU rendering / Blocks
- **App shell** ✅ Custom title bar / splits / theme library / i18n / file tree / background image
- **Modern input editor** ✅ Multi-line editing / syntax highlighting / history search / completion / clickable links
- **Auto-update** ✅ GitHub Release auto-update
- **AI integration** 🔭 Natural-language-to-command, error explanation (planned)
- **Cloud sync / remote** 🔭 Per-user data sync across devices after sign-in, plus a state & control protocol (planned)

---

## 📄 License

[Apache-2.0](LICENSE) © jimhy
