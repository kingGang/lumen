//! 客户端与 `lumen-server` 的 REST 通道（M5.1）。
//!
//! 用 `ureq` 同步阻塞请求，调用方在后台线程里使用（见 `shell/login_ui.rs`），
//! 不阻塞 UI 帧——与 F3 热更的后台线程模式一致，客户端不引入 tokio。
//!
//! 设备 id 持久化在应用数据目录、**登出后保留**，使同一物理机跨登录复用
//! 同一设备记录（避免在服务端重复登记设备）。

use std::path::PathBuf;
use std::time::Duration;

use lumen_protocol::{
    routes, ApiError, AuthResponse, DeviceListResponse, HistoryEntry, HistoryPullResponse,
    HistoryPushRequest, LoginRequest, RegisterRequest, RenameDeviceRequest, SettingsSync, UserInfo,
};

/// 本地开发默认服务端地址（可由环境变量 `LUMEN_SERVER_URL` 覆盖）。
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8787";

/// 取服务端基址（去掉尾部 `/`）。
pub fn server_url() -> String {
    let raw = std::env::var("LUMEN_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
    raw.trim_end_matches('/').to_string()
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
    let p = device_id_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim_start_matches('\u{feff}').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 保存设备 id（登录成功后调用；**登出不删**，保持设备稳定）。
pub fn save_device_id(id: &str) {
    let Some(p) = device_id_path() else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&p, id) {
        log::warn!("写设备 id 失败: {e}");
    }
}

/// 本机设备显示名（Windows 取 `COMPUTERNAME`，兜底 `Lumen-PC`）。
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Lumen-PC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn server_url_去尾斜杠() {
        // 默认值不带尾斜杠。
        assert!(!server_url().ends_with('/'));
    }
}
