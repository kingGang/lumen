//! 客户端与 `lumen-server` 的 REST 通道（M5.1）。
//!
//! 用 `ureq` 同步阻塞请求，调用方在后台线程里使用（见 `shell/login_ui.rs`），
//! 不阻塞 UI 帧——与 F3 热更的后台线程模式一致，客户端不引入 tokio。
//!
//! 设备 id 持久化在应用数据目录、**登出后保留**，使同一物理机跨登录复用
//! 同一设备记录（避免在服务端重复登记设备）。

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use lumen_protocol::{
    routes, ApiError, AuthResponse, DeviceListResponse, HistoryEntry, HistoryPullResponse,
    HistoryPushRequest, LoginRequest, RefreshResponse, RegisterRequest, RenameDeviceRequest,
    SettingsSync, UserInfo,
};

/// 进程内的服务端基址（懒初始化：环境变量 > 持久化(设置页) > **空**）。
/// **发布版不预设任何默认服务端地址**（含 localhost）——未配置时为空串，用户须在
/// 设置页填服务端地址；开发时用环境变量 `LUMEN_SERVER_URL` 指向本地服务端。
static SERVER_URL: RwLock<Option<String>> = RwLock::new(None);

/// 取服务端基址（已规整：去尾 `/`、缺协议补 `http://`；**未配置返回空串**）。
///
/// 首次读取时按「`LUMEN_SERVER_URL` 环境变量 > 持久化(设置页) > 空」初始化；
/// 之后由 [`set_server_url`]（设置页输入）覆盖。供 `login_ui` / `remote` 共用。
/// 返回空 = 未配置服务端，调用方应提示用户先在设置里填地址。
pub fn server_url() -> String {
    if let Some(u) = SERVER_URL.read().ok().and_then(|g| g.clone()) {
        return u;
    }
    let raw = std::env::var("LUMEN_SERVER_URL").ok().unwrap_or_default();
    let normalized = normalize_url(&raw);
    if let Ok(mut g) = SERVER_URL.write() {
        *g = Some(normalized.clone());
    }
    normalized
}

/// 设置服务端基址（设置页输入用）：更新进程内全局。持久化由 settings.json 负责。
pub fn set_server_url(url: &str) {
    let normalized = normalize_url(url);
    if let Ok(mut g) = SERVER_URL.write() {
        *g = Some(normalized);
    }
}

/// 规整地址：去首尾空白与尾 `/`；用户只填 `IP:端口` 时自动补 `http://`；**空则返回
/// 空串**（不再回退任何默认地址——发布版不预设服务端）。
fn normalize_url(raw: &str) -> String {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() {
        String::new()
    } else if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}


/// 网络/协议错误。
#[derive(Debug, Clone)]
pub enum CloudError {
    /// 网络层错误（连不上、超时等）。
    Network(String),
    /// 服务端返回的业务错误（含机器码）。
    Api {
        /// HTTP 状态码。
        status: u16,
        /// 机器可读 code（如 `user_not_found`）。
        code: String,
        /// 人类可读说明。
        message: String,
    },
    /// 响应体解析失败。
    Decode(String),
}

impl CloudError {
    /// 机器可读错误码（非 Api 变体给占位）。
    pub fn code(&self) -> &str {
        match self {
            CloudError::Api { code, .. } => code,
            CloudError::Network(_) => "network",
            CloudError::Decode(_) => "decode",
        }
    }

    /// 面向用户的中文提示。
    pub fn user_message(&self) -> String {
        match self {
            CloudError::Network(_) => "无法连接服务器，请检查网络或服务端地址".to_string(),
            CloudError::Decode(_) => "服务器响应异常".to_string(),
            CloudError::Api { code, message, .. } => match code.as_str() {
                "invalid_credentials" => "密码错误".to_string(),
                "email_taken" => "该邮箱已注册".to_string(),
                "bad_request" => "邮箱或密码格式不正确".to_string(),
                _ => message.clone(),
            },
        }
    }
}

/// 与 `lumen-server` 通信的客户端。
pub struct CloudClient {
    base: String,
    agent: ureq::Agent,
}

impl CloudClient {
    /// 以服务端基址新建客户端（连接 10s / 读 20s 超时）。
    pub fn new(base: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(20))
            .build();
        Self {
            base: base.into(),
            agent,
        }
    }

    /// 发一次请求，返回响应体文本；非 2xx 映射为 [`CloudError::Api`]。
    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> Result<String, CloudError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let result = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        match result {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| CloudError::Network(e.to_string())),
            Err(ureq::Error::Status(status, resp)) => {
                let txt = resp.into_string().unwrap_or_default();
                let api: Option<ApiError> = serde_json::from_str(&txt).ok();
                Err(CloudError::Api {
                    status,
                    code: api
                        .as_ref()
                        .map(|a| a.code.clone())
                        .unwrap_or_else(|| "http_error".to_string()),
                    message: api.map(|a| a.message).unwrap_or(txt),
                })
            }
            Err(ureq::Error::Transport(t)) => Err(CloudError::Network(t.to_string())),
        }
    }

    /// 反序列化 JSON 响应。
    fn decode<T: serde::de::DeserializeOwned>(txt: &str) -> Result<T, CloudError> {
        serde_json::from_str(txt).map_err(|e| CloudError::Decode(e.to_string()))
    }

    /// 序列化请求体。
    fn encode(v: &impl serde::Serialize) -> Result<String, CloudError> {
        serde_json::to_string(v).map_err(|e| CloudError::Decode(e.to_string()))
    }

    /// 注册账户。
    pub fn register(&self, email: &str, password: &str) -> Result<UserInfo, CloudError> {
        let body = Self::encode(&RegisterRequest {
            email: email.to_string(),
            password: password.to_string(),
        })?;
        let txt = self.send("POST", routes::REGISTER, None, Some(&body))?;
        Self::decode(&txt)
    }

    /// 登录（携带本设备信息）。
    pub fn login(&self, req: &LoginRequest) -> Result<AuthResponse, CloudError> {
        let body = Self::encode(req)?;
        let txt = self.send("POST", routes::LOGIN, None, Some(&body))?;
        Self::decode(&txt)
    }

    /// 设备列表。
    pub fn list_devices(&self, token: &str) -> Result<DeviceListResponse, CloudError> {
        let txt = self.send("GET", routes::DEVICES, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 设备心跳（保持本设备在线，刷新服务端 `last_seen`）。
    pub fn heartbeat(&self, token: &str) -> Result<(), CloudError> {
        self.send("POST", routes::HEARTBEAT, Some(token), None)?;
        Ok(())
    }

    /// 重命名设备。
    pub fn rename_device(&self, token: &str, id: &str, name: &str) -> Result<(), CloudError> {
        let body = Self::encode(&RenameDeviceRequest {
            name: name.to_string(),
        })?;
        self.send("PATCH", &routes::device(id), Some(token), Some(&body))?;
        Ok(())
    }

    /// 删除设备。
    pub fn delete_device(&self, token: &str, id: &str) -> Result<(), CloudError> {
        self.send("DELETE", &routes::device(id), Some(token), None)?;
        Ok(())
    }

    /// 拉取偏好设置。
    pub fn get_settings(&self, token: &str) -> Result<SettingsSync, CloudError> {
        let txt = self.send("GET", routes::SYNC_SETTINGS, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 推送偏好设置（返回服务端权威值）。
    pub fn put_settings(&self, token: &str, s: &SettingsSync) -> Result<SettingsSync, CloudError> {
        let body = Self::encode(s)?;
        let txt = self.send("PUT", routes::SYNC_SETTINGS, Some(token), Some(&body))?;
        Self::decode(&txt)
    }

    /// 用现有**有效** token 续期，返回新 token + 到期时间。客户端在 token 快到期时调用，免 7 天
    /// 到期后全面 401 掉线。旧 token 已过期则服务端 401（续期失败，需重新登录）。
    pub fn refresh_token(&self, token: &str) -> Result<RefreshResponse, CloudError> {
        let txt = self.send("POST", routes::REFRESH, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 拉取增量历史（`since` 为毫秒水位线）。
    pub fn pull_history(&self, token: &str, since: i64) -> Result<HistoryPullResponse, CloudError> {
        let path = format!("{}?since={since}", routes::SYNC_HISTORY);
        let txt = self.send("GET", &path, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 推送历史（返回服务端新插入条数）。
    pub fn push_history(
        &self,
        token: &str,
        entries: Vec<HistoryEntry>,
    ) -> Result<u64, CloudError> {
        let body = Self::encode(&HistoryPushRequest { entries })?;
        let txt = self.send("POST", routes::SYNC_HISTORY, Some(token), Some(&body))?;
        let v: serde_json::Value = Self::decode(&txt)?;
        Ok(v.get("inserted").and_then(serde_json::Value::as_u64).unwrap_or(0))
    }
}

// ——— 设备 id 持久化（登出后保留，跨登录复用）———

/// 设备 id 文件路径（应用数据目录下 `device_id`）。
fn device_id_path() -> Option<PathBuf> {
    crate::paths::data_file("device_id")
}

/// 读取持久化的设备 id（首次登录前为 None）。
pub fn load_device_id() -> Option<String> {
    load_device_id_from(&device_id_path()?)
}

/// 从指定路径读设备 id（拆出来供单测注入临时路径，不碰真实数据目录）。
/// 空 / 纯空白（老实现「半写成空」的残留）视作 `None`。
fn load_device_id_from(path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim_start_matches('\u{feff}').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 保存设备 id（登录成功后调用；**登出不删**，保持设备稳定）。
///
/// **原子写**（同目录临时文件 + rename，与 [`crate::profile`] 同款）：老实现用 `fs::write`
/// 截断写又静默吞错，一旦半写成空 / 写失败而 profile.json 仍有值，就会长期潜伏、直到某次
/// 重登带空 device_id、被服务端当新机造出「幽灵设备」。改原子写并把错误上抛由调用方处置。
/// 数据目录不可用时视为「本次运行不持久化」，返回 `Ok`。
pub fn save_device_id(id: &str) -> std::io::Result<()> {
    match device_id_path() {
        Some(p) => save_device_id_to(&p, id),
        None => Ok(()), // 数据目录不可用：本次运行不持久化（与 profile 同语义）。
    }
}

/// 原子写设备 id 到指定路径（同目录临时文件 + rename），拆出来供单测注入临时路径。
fn save_device_id_to(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, id.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 启动对账：把 `device_id` 独立文件与 profile.json 里的镜像值收敛到一致。
///
/// 独立文件与 `profile.device_id` 是同一个 id 的两处副本，但重登只读独立文件、运行期又
/// 从不再读它——一旦独立文件缺失 / 为空而 profile 仍有 id（历史写失败 / 外部清理 / 曾在别的
/// 数据目录写过），下次重登就会带空 id 造出幽灵。启动时若独立文件读不到而 profile 有值，
/// 用 profile 的值回写修复，把背离消灭在爆发之前。返回是否发生了修复（供日志）。
pub fn reconcile_device_id(profile_device_id: Option<&str>) -> bool {
    if load_device_id().is_some() {
        return false; // 独立文件已有值：无需修复。
    }
    let Some(pid) = profile_device_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false; // profile 也没有：首次登录前的正常状态。
    };
    match save_device_id(pid) {
        Ok(()) => {
            log::info!("已用 profile.device_id 回写修复缺失的 device_id 文件");
            true
        }
        Err(e) => {
            log::warn!("回写 device_id 文件失败: {e}");
            false
        }
    }
}

/// **稳定硬件标识**：Windows 读注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`。
///
/// 该值对「更新 app / 删本地文件 / 换数据目录 / 服务端 DB 重置」全都不变，只在重装系统时
/// 才变，是理想的「同一物理机」稳定标识。服务端据 `(user_id, hw_id)` 幂等认领设备，从根上
/// 杜绝「客户端带空 / 异 device_id 就分裂出幽灵设备」。读不到（受限机器 / 非 Windows）返回
/// `None`，服务端退化回按 `device_id` 处理（无回归）。结果进程内缓存（该值恒定）。
pub fn hardware_id() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(read_machine_guid).clone()
}

/// 读 `MachineGuid`（Windows 实现）。
#[cfg(windows)]
fn read_machine_guid() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
    };
    // MachineGuid 是 36 字符 GUID 串，128 个 u16 足够容纳（含结尾 NUL）。
    let mut buf = [0u16; 128];
    let mut cb = u32::try_from(std::mem::size_of_val(&buf)).unwrap_or(0);
    // SAFETY: buf/cb 均为本地栈缓冲，指针在调用期间有效；RegGetValueW 至多写 cb 字节到 buf
    // 并把实际字节数回写 cb。子键 / 值名为静态宽字符串常量。RRF_SUBKEY_WOW6464KEY 强制 64 位
    // 视图（本应用 x64，避免 WOW64 重定向）。
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Cryptography"),
            w!("MachineGuid"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut cb),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    // cb 为字节数（含结尾 NUL）→ 元素数；去尾部 NUL 后按 UTF-16 解码。
    let elems = (usize::try_from(cb).unwrap_or(0) / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..elems]);
    let s = s.trim_end_matches('\u{0}').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 读稳定机器标识（Linux 实现）：`/etc/machine-id`，回退
/// `/var/lib/dbus/machine-id`（无 systemd 的老发行版）。该值由
/// systemd/dbus 在系统首次启动时生成，跨重启 / 更新恒定，语义上等价
/// Windows 的 `MachineGuid`。读不到（受限容器 / 权限）返回 `None`。
#[cfg(target_os = "linux")]
fn read_machine_guid() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 读稳定机器标识（macOS 实现）：`ioreg` 读 `IOPlatformExpertDevice` 的
/// `IOPlatformUUID`。该 UUID 绑定主板、跨系统更新恒定，语义等价
/// `MachineGuid`。`ioreg` 是 macOS 自带命令，零第三方依赖；读不到返回 `None`。
#[cfg(target_os = "macos")]
fn read_machine_guid() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 目标行形如：    "IOPlatformUUID" = "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
    // 取 `IOPlatformUUID` 之后第一对引号中间的内容（split('"').nth(1)）。
    for line in text.lines() {
        let Some((_, after)) = line.split_once("IOPlatformUUID") else {
            continue;
        };
        if let Some(uuid) = after.split('"').nth(1) {
            let uuid = uuid.trim();
            if !uuid.is_empty() {
                return Some(uuid.to_string());
            }
        }
    }
    None
}

/// 其余平台（BSD 等）：无统一稳定标识源，返回 `None`（服务端退化按 `device_id`）。
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn read_machine_guid() -> Option<String> {
    None
}

/// 本机设备显示名。Windows 取 `COMPUTERNAME`；unix 优先 `HOSTNAME`
/// 环境变量、回退 `hostname` 命令；一律兜底 `Lumen-PC`。
pub fn device_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Lumen-PC".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(unix_hostname)
            .unwrap_or_else(|| "Lumen-PC".to_string())
    }
}

/// unix：调 `hostname` 命令取主机名（`HOSTNAME` 环境变量常未导出到进程）。
#[cfg(not(windows))]
fn unix_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立临时目录，避免并行互踩，且绝不碰真实数据目录。
    fn temp_devid_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen_devid_test_{}_{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("device_id")
    }

    #[test]
    fn 设备id原子写往返() {
        let p = temp_devid_path("roundtrip");
        let _ = std::fs::remove_file(&p);
        // 缺失 → None。
        assert_eq!(load_device_id_from(&p), None);
        // 写入 → 读回一致；rename 后不应残留 .tmp。
        save_device_id_to(&p, "dev-123").expect("写盘");
        assert_eq!(load_device_id_from(&p), Some("dev-123".to_string()));
        assert!(!p.with_extension("tmp").exists(), "原子写后不应残留 tmp");
        // 覆盖写生效。
        save_device_id_to(&p, "dev-456").expect("覆盖");
        assert_eq!(load_device_id_from(&p), Some("dev-456".to_string()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 空白设备id视作缺失() {
        // 老实现「半写成空/纯空白」的残留必须被当成 None——否则重登会带空 id 造幽灵。
        let p = temp_devid_path("blank");
        save_device_id_to(&p, "   \r\n").expect("写空白");
        assert_eq!(load_device_id_from(&p), None);
        // BOM 前缀也应被剥离后判空/取值。
        std::fs::write(&p, "\u{feff}dev-bom").expect("写 BOM");
        assert_eq!(load_device_id_from(&p), Some("dev-bom".to_string()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 硬件标识幂等() {
        // 值因机而异不可硬断言具体值，但同进程多次调用必一致（进程内缓存 + 机器恒定）。
        assert_eq!(hardware_id(), hardware_id());
        // Windows 上 MachineGuid 恒存在：必须真的读到一个合法 GUID（36 字符、含连字符），
        // 否则说明注册表读取失效、hw_id 会退化、治本落空——此断言守住这条底线。
        #[cfg(windows)]
        {
            let hw = hardware_id().expect("Windows 应能读到 MachineGuid");
            assert_eq!(hw.len(), 36, "MachineGuid 应为 36 字符 GUID：{hw}");
            assert_eq!(hw.matches('-').count(), 4, "GUID 应含 4 个连字符：{hw}");
        }
    }

    #[test]
    fn 错误码映射() {
        let e = CloudError::Api {
            status: 404,
            code: "user_not_found".to_string(),
            message: "x".to_string(),
        };
        assert_eq!(e.code(), "user_not_found");
        let net = CloudError::Network("boom".to_string());
        assert!(net.user_message().contains("无法连接"));
    }

    #[test]
    fn normalize_url_规整与无默认() {
        assert_eq!(normalize_url("1.2.3.4:8787/"), "http://1.2.3.4:8787");
        assert_eq!(normalize_url("https://x.com/"), "https://x.com");
        assert_eq!(normalize_url("http://a.b:8787"), "http://a.b:8787");
        // 空 / 纯空白 → 空串（发布版不预设默认服务端地址）。
        assert_eq!(normalize_url(""), "");
        assert_eq!(normalize_url("   "), "");
        // server_url 规整后绝不带尾斜杠。
        assert!(!server_url().ends_with('/'));
    }
}
