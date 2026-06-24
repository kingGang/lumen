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
    /// M6 P2P STUN 反射端 UDP 监听地址（如 `0.0.0.0:8788`）。客户端经此探公网映射端点做 QUIC
    /// 打洞（自建反射替代被墙的公共 STUN，国内可达 + 自主可控，见 docs/M6 设计 §7）。与中继 WS
    /// （TCP `bind_addr`）解耦的独立端点。
    pub stun_bind_addr: String,
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
            bind_addr: env::var("LUMEN_BIND_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8787".to_string()),
            jwt_secret: env::var("LUMEN_JWT_SECRET")
                .unwrap_or_else(|_| DEFAULT_JWT_SECRET.to_string()),
            token_ttl_secs: env::var("LUMEN_TOKEN_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7 * 24 * 3600),
            // 在线窗口：last_seen 在此秒内视为在线。45s ≈ 客户端 10s 心跳的 4 次容差——离线后约
            // 45s 即被判离线、从控制端列表移除（120s 太久，海风哥反馈离线迟迟不消失）。
            online_window_secs: env::var("LUMEN_ONLINE_WINDOW_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(45),
            stun_bind_addr: env::var("LUMEN_STUN_BIND_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8788".to_string()),
        }
    }

    /// 是否仍在用默认（不安全）JWT 密钥。
    pub fn uses_default_jwt_secret(&self) -> bool {
        self.jwt_secret == DEFAULT_JWT_SECRET
    }
}
