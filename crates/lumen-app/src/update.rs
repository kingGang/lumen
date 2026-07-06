//! F3 热更（自动更新）：启动查 GitHub 的 latest Release，有新版则提示 +
//! 下载 Inno Setup 安装包 + 拉起安装器（Lumen 优雅退出，安装器覆盖安装
//! 并重启）。**只走 GitHub、不发 Gitee**（海风哥 2026-06-13 拍板），
//! 不依赖自建服务端（方案见需求池 F3）。
//!
//! 本模块为**纯逻辑 + HTTP**（不含 winit/egui）：线程编排、事件唤醒、
//! toast、UI 在 `main.rs` / `settings_ui.rs`。版本解析、Release JSON
//! 解析、版本比较均可单测；网络请求用 [`ureq`]（同步，后台线程内阻塞）。
//!
//! # 前置
//! [`GITHUB_REPO`]（`jimhy/lumen`）须发布带 `.exe` 安装包资产的 Release。
//! 仓库/Release 不存在时检查静默失败（无更新），不影响正常使用。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// GitHub 仓库 slug（owner/repo）。发布只走 GitHub（海风哥 2026-06-13
/// 拍板，不发 Gitee）。来源：`git@github.com:jimhy/lumen.git`。
pub const GITHUB_REPO: &str = "jimhy/lumen";

/// 请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// 语义版本号（major.minor.patch，忽略 pre-release/build 元数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// 解析 `v1.2.3` / `1.2.3` / `1.2`（缺省补 0）。多余段忽略，非数字
    /// 段截断（如 `1.2.3-rc1` → 1.2.3）。完全无法解析返回 None。
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
        let mut it = s.split('.');
        let major = parse_leading_u32(it.next()?)?;
        let minor = it.next().and_then(parse_leading_u32).unwrap_or(0);
        let patch = it.next().and_then(parse_leading_u32).unwrap_or(0);
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 取字符串前导的十进制数字（`3-rc1` → 3，`abc` → None）。
fn parse_leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 当前运行版本（编译期 `CARGO_PKG_VERSION`）。
/// 仅 Windows 更新检查用（非 Windows [`check_for_update`] 直接返回 UpToDate，
/// 无需比对版本），故 windows-only，避免非 Windows 上「函数从未使用」告警。
#[cfg(windows)]
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// 发现的可更新版本信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// 新版本号。
    pub version: Version,
    /// 原始 tag（如 `v0.2.0`，用于「跳过此版本」记录）。
    pub tag: String,
    /// 更新日志（Release body）。
    pub notes: String,
    /// 安装包（.exe）下载地址。
    pub download_url: String,
}

/// 后台更新线程 → 主事件循环的消息（经 channel 送达，配合
/// `proxy.send_event(PtyWake)` 唤醒主循环 drain）。
#[derive(Debug, Clone)]
pub enum UpdateMsg {
    /// 检查到新版本。
    Available(UpdateInfo),
    /// 检查完成但已是最新（仅手动检查时发，用于 toast 反馈）。
    UpToDate,
    /// 检查失败（仅手动检查时发）。
    CheckFailed,
    /// 安装包下载完成（路径），主循环据此拉起安装器并优雅退出。
    DownloadDone(PathBuf),
    /// 下载失败（错误信息）。
    DownloadFailed(String),
}

/// 当前 Unix 毫秒时间戳（节流用；取不到时钟返回 0）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 启动自动检查节流：距上次检查不足 `min_interval_ms` 则跳过。
/// `last` 为 None（从未查过）一律放行。
pub fn should_auto_check(last: Option<u64>, now: u64, min_interval_ms: u64) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_sub(t) >= min_interval_ms,
    }
}

/// 从 GitHub Release JSON 解析出版本、日志与 `.exe` 安装包地址。
///
/// GitHub Release 结构含 `tag_name` / `body` / `assets[]`（每项有 `name` +
/// `browser_download_url`）。无 `.exe` 资产时回退取第一个资产；完全无资产
/// 或无 tag 返回 None。
/// 仅 Windows 更新检查（[`fetch_release`]）+ 单测用；非 Windows 非测试构建
/// 用不到（[`check_for_update`] 直接返回 UpToDate），故 `any(windows, test)`
/// 门控，避免 Linux/macOS release 构建「函数从未使用」告警。
#[cfg(any(windows, test))]
pub fn parse_release_json(body: &str) -> Option<ParsedRelease> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_owned();
    let version = Version::parse(&tag)?;
    let notes = v
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_owned();
    let assets = v.get("assets").and_then(|a| a.as_array());
    let download_url = assets.and_then(|arr| {
        // 优先 .exe（Inno Setup 安装包）；否则取第一个有下载地址的资产。
        let pick = arr
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(".exe"))
            })
            .or_else(|| arr.first());
        pick.and_then(|a| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_owned())
    });
    Some(ParsedRelease {
        version,
        tag,
        notes,
        download_url,
    })
}

/// [`parse_release_json`] 的中间结果（下载地址可能缺失）。
/// 与 [`parse_release_json`] 同门控（仅 Windows + 测试）。
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRelease {
    pub version: Version,
    pub tag: String,
    pub notes: String,
    pub download_url: Option<String>,
}

/// GitHub latest Release API URL。仅 Windows 更新检查用（见 [`check_for_update`]）。
#[cfg(windows)]
fn latest_release_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// 给 `AgentBuilder` 按需挂网络代理：`proxy` 为完整 URL（http(s)/socks5）。
/// 地址非法时记 warn 并忽略（回退直连），绝不因代理配置错误而完全断网。
fn with_proxy(builder: ureq::AgentBuilder, proxy: Option<&str>) -> ureq::AgentBuilder {
    if let Some(p) = proxy {
        match ureq::Proxy::new(p) {
            Ok(px) => return builder.proxy(px),
            Err(e) => log::warn!("F3：代理地址无效，忽略走直连: {p}（{e}）"),
        }
    }
    builder
}

/// 请求并解析 GitHub 的 latest Release。失败（网络/HTTP 非 2xx/解析）返回
/// `Err`。在后台线程内调用（阻塞）。`proxy` 为生效的网络代理（None=直连）。
/// 仅 Windows 更新检查用（非 Windows [`check_for_update`] 直接返回 UpToDate）。
#[cfg(windows)]
fn fetch_release(repo: &str, proxy: Option<&str>) -> Result<UpdateInfo, String> {
    let url = latest_release_url(repo);
    let agent = with_proxy(
        ureq::AgentBuilder::new()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("Lumen/", env!("CARGO_PKG_VERSION"))),
        proxy,
    )
    .build();
    let resp = agent
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| format!("GitHub 请求失败: {e}"))?;
    let body = resp
        .into_string()
        .map_err(|e| format!("GitHub 读取响应失败: {e}"))?;
    let parsed = parse_release_json(&body).ok_or("GitHub Release 解析失败")?;
    let download_url = parsed
        .download_url
        .ok_or("GitHub Release 无可下载安装包")?;
    Ok(UpdateInfo {
        version: parsed.version,
        tag: parsed.tag,
        notes: parsed.notes,
        download_url,
    })
}

/// 更新检查结果。
///
/// 非 Windows 上 [`check_for_update`] 只构造 `UpToDate`，`Newer`/`Failed` 仅
/// 在 `main.rs` 被模式匹配、从不构造 → 「variant never constructed」告警；但
/// 两变体又必须保留（否则 main.rs 的 match 编译不过），故非 Windows 抑制该告警。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// 有新版本。
    Newer(UpdateInfo),
    /// 已是最新（成功取到 Release 但版本不更新）。
    UpToDate,
    /// 请求失败/超时（无法判断）。
    Failed,
}

/// 查更新：请求 GitHub latest Release，与当前版本比较裁决。
/// 发布只走 GitHub（不发 Gitee，海风哥 2026-06-13 拍板）。
/// 在后台线程内调用（阻塞）。
///
/// 非 Windows：自动更新分发的是 Windows Inno Setup `.exe` 安装包（见
/// [`launch_installer`]），Linux/macOS 无从应用（走源码构建 / 平台包管理器）。
/// 故一律返回 [`CheckResult::UpToDate`]，从源头掐断「弹更新提示 → 下载 .exe →
/// 拉起安装器失败」的坏链路，整条更新 UI 流程在非 Windows 上永不触发。
pub fn check_for_update(proxy: Option<&str>) -> CheckResult {
    #[cfg(not(windows))]
    {
        let _ = proxy;
        CheckResult::UpToDate
    }
    #[cfg(windows)]
    {
        let current = current_version();
        match fetch_release(GITHUB_REPO, proxy) {
            Ok(info) if info.version > current => {
                log::info!("F3：发现新版本 {}（当前 {current}）", info.version);
                CheckResult::Newer(info)
            }
            Ok(info) => {
                log::debug!("F3：已是最新版 {current}（GitHub 上 {}）", info.version);
                CheckResult::UpToDate
            }
            Err(e) => {
                log::debug!("F3：{e}");
                CheckResult::Failed
            }
        }
    }
}

/// 下载安装包到 `dest`，`progress(downloaded, total)` 报告进度（total 为
/// `None` 表示服务端未给 Content-Length）。
pub fn download_installer(
    url: &str,
    dest: &Path,
    proxy: Option<&str>,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    // timeout_read 防慢响应/中途僵死把下载线程永久挂起（审查 finding：
    // 仅 timeout_connect 不够，连上后 read 默认可无限阻塞）。单次读阻塞
    // 上限 60s（大包逐块读，每块远快于此，不会误杀正常慢速下载）。
    let agent = with_proxy(
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .user_agent(concat!("Lumen/", env!("CARGO_PKG_VERSION"))),
        proxy,
    )
    .build();
    let result = download_to_file(&agent, url, dest, &mut progress);
    if result.is_err() {
        // 出错清半截文件（同 tag 重下会覆盖，但留半截既占空间又可能被
        // 误当成完整包拉起，统一清掉更干净）。
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// [`download_installer`] 的实际下载体（错误统一在外层清理半截文件）。
fn download_to_file(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("下载请求失败: {e}"))?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("创建安装包文件失败 {}: {e}", dest.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载读取失败: {e}"))?;
        if n == 0 {
            break;
        }
        use std::io::Write as _;
        file.write_all(&buf[..n])
            .map_err(|e| format!("写安装包失败: {e}"))?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    Ok(())
}

/// 安装包的默认落地路径（`%TEMP%/Lumen-Setup-<tag>.exe`）。
pub fn installer_dest(tag: &str) -> PathBuf {
    // tag 里的非法文件名字符替换为 `_`（防 `/` 等）。
    let safe: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("Lumen-Setup-{safe}.exe"))
}

/// 拉起安装器（不等待）。调用方随后须走优雅退出流程让安装器替换 exe。
///
/// 安装包为 Inno Setup 产物，自带 UI + 关闭运行进程 + 重启；这里只负责
/// 启动它。返回是否成功 spawn。
pub fn launch_installer(path: &Path) -> Result<(), String> {
    std::process::Command::new(path)
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("启动安装器失败 {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 版本解析_各形态() {
        assert_eq!(Version::parse("1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("v0.1.0"), Some(Version { major: 0, minor: 1, patch: 0 }));
        assert_eq!(Version::parse("V2.0"), Some(Version { major: 2, minor: 0, patch: 0 }));
        assert_eq!(Version::parse("3"), Some(Version { major: 3, minor: 0, patch: 0 }));
        // pre-release 后缀截断到数字部分。
        assert_eq!(Version::parse("1.2.3-rc1"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("not-a-version"), None);
    }

    #[test]
    fn 版本比较() {
        let v = Version::parse;
        assert!(v("0.2.0") > v("0.1.0"));
        assert!(v("1.0.0") > v("0.9.9"));
        assert!(v("0.1.10") > v("0.1.9"));
        assert!(v("1.2.3") == v("v1.2.3"));
        // 同版本不算「更新」（严格大于为 false）。
        assert!(v("0.1.0") <= v("0.1.0"));
    }

    #[test]
    fn release_json_github格式() {
        let body = r###"{
            "tag_name": "v0.2.0",
            "name": "Lumen 0.2.0",
            "body": "## 更新\n- 修了一堆 bug",
            "assets": [
                {"name": "Lumen-Setup-v0.2.0.exe", "browser_download_url": "https://example.com/setup.exe"},
                {"name": "Lumen.zip", "browser_download_url": "https://example.com/lumen.zip"}
            ]
        }"###;
        let r = parse_release_json(body).expect("应解析成功");
        assert_eq!(r.version, Version { major: 0, minor: 2, patch: 0 });
        assert_eq!(r.tag, "v0.2.0");
        assert!(r.notes.contains("修了一堆 bug"));
        // 优先选 .exe 资产。
        assert_eq!(r.download_url.as_deref(), Some("https://example.com/setup.exe"));
    }

    #[test]
    fn release_json_无exe资产回退第一个() {
        let body = r#"{
            "tag_name": "1.0.0",
            "body": "",
            "assets": [{"name": "a.zip", "browser_download_url": "https://x/a.zip"}]
        }"#;
        let r = parse_release_json(body).unwrap();
        assert_eq!(r.download_url.as_deref(), Some("https://x/a.zip"));
    }

    #[test]
    fn release_json_无tag返回none() {
        assert!(parse_release_json(r#"{"body":"x"}"#).is_none());
        assert!(parse_release_json("not json").is_none());
    }

    #[test]
    fn 安装包落地路径_清洗tag() {
        let p = installer_dest("v0.2.0");
        assert!(p.to_string_lossy().contains("Lumen-Setup-v0.2.0.exe"));
        // 含非法字符的 tag 被清洗——只断言文件名部分（清洗后）不含路径分隔符
        // 与非法字符。不能断言整条路径：unix 上 temp_dir 的目录分隔符本就是
        // `/`，会误判（跨平台移植前该测试只在 Windows 跑，故未暴露）。
        let p2 = installer_dest("v0.2/evil");
        let name2 = p2.file_name().expect("应有文件名").to_string_lossy();
        assert!(!name2.contains('/'));
        assert_eq!(name2, "Lumen-Setup-v0.2_evil.exe");
    }

    #[test]
    fn 自动检查节流() {
        // 从未检查过：放行。
        assert!(should_auto_check(None, 1_000_000, 3_600_000));
        // 间隔不足：跳过。
        assert!(!should_auto_check(Some(1_000_000), 1_001_000, 3_600_000));
        // 间隔足够：放行。
        assert!(should_auto_check(Some(1_000_000), 5_000_000, 3_600_000));
    }

    // latest_release_url 仅 Windows 编译（非 Windows 更新检查直接返回 UpToDate）。
    #[cfg(windows)]
    #[test]
    fn latest_release_url_格式() {
        assert_eq!(
            latest_release_url("a/b"),
            "https://api.github.com/repos/a/b/releases/latest"
        );
    }
}
