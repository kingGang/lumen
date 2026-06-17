//! 运行配置：全部从环境变量读取，带合理默认（本地 docker Postgres 开箱即用）。

use std::env;

/// 默认（不安全）JWT 密钥——仅供本地开发；生产必须经 `LUMEN_JWT_SECRET` 覆盖。
pub const DEFAULT_JWT_SECRET: &str = "dev-insecure-secret-change-me";

/// 服务端运行配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres 连接串。
    pub database_url: String,
    /// 监听地址（如 `0.0.0.0:8787`）。
    pub bind_addr: String,
    /// JWT 签名密钥（生产务必经 `LUMEN_JWT_SECRET` 覆盖）。
    pub jwt_secret: String,
    /// token 有效期（秒）。
    pub token_ttl_secs: i64,
    /// 设备在线判定阈值（秒）：`last_seen` 在此窗口内视为在线（M5.1 近似，M5.2 换心跳）。
    pub online_window_secs: i64,
}

impl Config {
    /// 从环境变量加载，缺失用默认值（默认对接本地 docker Postgres）。
    pub fn from_env() -> Self {
        Self {
            database_url: env::var("LUMEN_DATABASE_URL").unwrap_or_else(|_| {
                // 本地开发：专用 docker 容器 lumen-postgres（host 5544 -> 容器 5432）。
                // 用 127.0.0.1 强制 IPv4，避开本机原生 PostgreSQL 占用的 5432。
                // 详见 server/lumen-server/README.md。
                "postgres://lumen_user:lumen_password@127.0.0.1:5544/lumen?sslmode=disable"
                    .to_string()
            }),
            bind_addr: env::var("LUMEN_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
            jwt_secret: env::var("LUMEN_JWT_SECRET")
                .unwrap_or_else(|_| DEFAULT_JWT_SECRET.to_string()),
            token_ttl_secs: env::var("LUMEN_TOKEN_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7 * 24 * 3600),
            online_window_secs: env::var("LUMEN_ONLINE_WINDOW_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        }
    }

    /// 是否仍在用默认（不安全）JWT 密钥。
    pub fn uses_default_jwt_secret(&self) -> bool {
        self.jwt_secret == DEFAULT_JWT_SECRET
    }

    /// 监听地址是否为本机回环（默认密钥仅在回环时可容忍）。
    pub fn is_loopback_bind(&self) -> bool {
        let a = self.bind_addr.trim();
        a.starts_with("127.") || a.starts_with("localhost") || a.starts_with("[::1]")
    }
}
