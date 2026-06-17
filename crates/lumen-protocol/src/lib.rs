//! Lumen 远程控制协议（M5）：客户端与 `lumen-server` 共享的线缆类型。
//!
//! 本 crate **零平台依赖**（纯 `serde` 结构体），同时被 Windows 客户端
//! （`lumen-app`，经 `ureq` 发 REST）、本地测试 server 与 Linux 生产
//! server（`lumen-server`，axum）依赖，保证两端类型不漂移。
//!
//! 所有响应带协议版本（[`PROTOCOL_VERSION`]）；REST 路径集中在 [`routes`]
//! 模块，避免客户端/服务端各写一份字符串而拼错。
//!
//! 覆盖范围按里程碑推进：**M5.1** 账户 + 设备登记 + 设置/历史同步（本文件
//! 已含）；M5.2 设备在线、M5.3 终端远程、M5.4 文件传输的消息后续在此扩展。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 协议版本号。任何破坏性变更必须递增；登录响应回传，客户端可比对。
pub const PROTOCOL_VERSION: u32 = 1;

/// REST 端点路径（客户端与服务端共用，避免字符串漂移）。
pub mod routes {
    /// 健康检查 `GET`。
    pub const HEALTH: &str = "/api/v1/health";
    /// 注册 `POST`。
    pub const REGISTER: &str = "/api/v1/auth/register";
    /// 登录 `POST`（成功即登记/更新本设备）。
    pub const LOGIN: &str = "/api/v1/auth/login";
    /// 设备列表 `GET`（需 Bearer token）。
    pub const DEVICES: &str = "/api/v1/devices";
    /// 偏好设置同步：`GET` 拉取 / `PUT` 推送。
    pub const SYNC_SETTINGS: &str = "/api/v1/sync/settings";
    /// 命令历史同步：`GET ?since=<ts_ms>` 拉取 / `POST` 推送。
    pub const SYNC_HISTORY: &str = "/api/v1/sync/history";

    /// 单设备路径（重命名 `PATCH` / 删除 `DELETE`）。
    #[must_use]
    pub fn device(id: &str) -> String {
        format!("/api/v1/devices/{id}")
    }
}

/// 统一错误响应体（HTTP 4xx/5xx 时返回）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiError {
    /// 机器可读错误码（如 `email_taken`、`invalid_credentials`）。
    pub code: String,
    /// 人类可读说明（英文，UI 侧可按 `code` 自行本地化）。
    pub message: String,
}

impl ApiError {
    /// 构造一个错误响应体。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 客户端上报的设备信息（登录时携带）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 已有设备 id（首次登录为 `None`，由服务端分配并回传，客户端持久化后续带上）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// 设备显示名（默认取机器名，用户可改）。
    pub name: String,
    /// 操作系统标识（如 `windows`）。
    pub os: String,
    /// 客户端版本（如 `0.1.9`）。
    pub app_version: String,
}

/// 注册请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// 账户邮箱。
    pub email: String,
    /// 明文密码（仅传输用，服务端 argon2 哈希后落库，绝不明文存储）。
    pub password: String,
}

/// 登录请求（成功即登记/更新本设备的 `last_seen`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    /// 账户邮箱。
    pub email: String,
    /// 明文密码（仅传输用）。
    pub password: String,
    /// 本设备信息（首次 `device_id` 为 `None`，由服务端分配）。
    pub device: DeviceInfo,
}

/// 账户公开信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInfo {
    /// 账户 id（uuid 字符串）。
    pub id: String,
    /// 邮箱。
    pub email: String,
    /// 展示名（注册时取邮箱 `@` 前段）。
    pub display_name: String,
}

/// 注册/登录成功响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    /// 服务端协议版本（客户端可比对）。
    pub protocol_version: u32,
    /// Bearer token（JWT，短期，客户端持久化用于后续鉴权）。
    pub token: String,
    /// token 过期 Unix 秒。
    pub expires_at: i64,
    /// 账户信息。
    pub user: UserInfo,
    /// 本设备 id（首次登录由服务端分配，客户端需持久化）。
    pub device_id: String,
}

/// 设备列表项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    /// 设备 id（uuid 字符串）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 操作系统标识。
    pub os: String,
    /// 客户端版本。
    pub app_version: String,
    /// 是否在线（M5.2 心跳维护；M5.1 暂以 `last_seen` 是否在阈值内粗略判定）。
    pub online: bool,
    /// 最近活跃 Unix 秒。
    pub last_seen: i64,
    /// 是否为发起请求的本设备。
    pub is_self: bool,
}

/// 设备列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListResponse {
    /// 同账户下全部设备（在线优先由客户端排序）。
    pub devices: Vec<DeviceRecord>,
}

/// 重命名设备请求（`PATCH /devices/{id}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameDeviceRequest {
    /// 新显示名。
    pub name: String,
}

/// 偏好设置同步载荷（按 `version` 做 last-write-wins 的整体 blob）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsSync {
    /// 单调递增版本（客户端每次本地变更自增；服务端只接受更大的 `version`）。
    pub version: i64,
    /// 偏好数据（客户端序列化的 JSON 字符串；服务端不解释、原样存取）。
    pub data: String,
}

/// 命令历史条目（与客户端 `history.jsonl` 对齐的同步形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// 命令文本。
    pub text: String,
    /// 录入时刻 Unix 毫秒（去重键 = `text` + `ts`）。
    pub ts: i64,
    /// 录入时 cwd（跨机仅供展示/过滤，可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 退出码（可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// 历史推送请求（多设备来源按 `text`+`ts` 去重合并）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPushRequest {
    /// 本批要上行的历史条目。
    pub entries: Vec<HistoryEntry>,
}

/// 历史拉取响应（`since` 之后的增量）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPullResponse {
    /// `since` 之后新增的历史条目（按 `ts` 升序）。
    pub entries: Vec<HistoryEntry>,
    /// 本批最大 `ts`（客户端存为下次 `since` 水位线；空批回传请求的 `since`）。
    pub watermark: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 设备路径拼接() {
        assert_eq!(routes::device("abc-123"), "/api/v1/devices/abc-123");
    }

    #[test]
    fn 鉴权响应往返() {
        let resp = AuthResponse {
            protocol_version: PROTOCOL_VERSION,
            token: "t".into(),
            expires_at: 123,
            user: UserInfo {
                id: "u1".into(),
                email: "a@b.c".into(),
                display_name: "a".into(),
            },
            device_id: "d1".into(),
        };
        let json = serde_json::to_string(&resp).expect("序列化");
        let back: AuthResponse = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.device_id, "d1");
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn 设备信息可省略id() {
        // 首次登录 device_id = None，不应出现在 JSON 里。
        let info = DeviceInfo {
            device_id: None,
            name: "PC".into(),
            os: "windows".into(),
            app_version: "0.1.9".into(),
        };
        let json = serde_json::to_string(&info).expect("序列化");
        assert!(!json.contains("device_id"), "None 的 device_id 不应序列化");
        let back: DeviceInfo = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, info);
    }

    #[test]
    fn 历史条目可选字段省略() {
        let e = HistoryEntry {
            text: "ls".into(),
            ts: 1,
            cwd: None,
            exit_code: None,
        };
        let json = serde_json::to_string(&e).expect("序列化");
        assert!(!json.contains("cwd"));
        assert!(!json.contains("exit_code"));
    }
}
