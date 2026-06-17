//! Lumen 云服务端入口（M5.1）：账户、设备、设置/历史同步。
//!
//! 纯跨平台依赖：Windows（本地测试）与 Linux（生产发布）均可 `cargo build`。
//! 配置全走环境变量（见 [`config::Config`]），默认对接本地 docker Postgres。

#![forbid(unsafe_code)]

mod auth;
mod config;
mod db;
mod error;
mod handlers;
mod state;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
use axum::Router;
use lumen_protocol::routes as r;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::from_env();
    // 安全闸门：默认 JWT 密钥仅允许本地回环；非回环地址用默认密钥拒绝启动。
    if config.uses_default_jwt_secret() {
        if config.is_loopback_bind() {
            tracing::warn!(
                "正在使用默认 JWT 密钥（仅限本地开发）；生产部署务必设置 LUMEN_JWT_SECRET 为强随机值！"
            );
        } else {
            anyhow::bail!(
                "拒绝启动：监听非回环地址 {} 却使用默认 JWT 密钥；请设置 LUMEN_JWT_SECRET",
                config.bind_addr
            );
        }
    }
    tracing::info!("连接数据库 …");
    let pool = db::create_pool(&config.database_url)?;
    db::init_schema(&pool).await?;
    tracing::info!("数据库就绪，建表完成");

    let bind_addr = config.bind_addr.clone();
    let state = AppState {
        pool,
        config: Arc::new(config),
    };
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("lumen-server 已就绪 → http://{bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// 组装路由。
fn build_router(state: AppState) -> Router {
    Router::new()
        .route(r::HEALTH, get(handlers::health))
        .route(r::REGISTER, post(handlers::register))
        .route(r::LOGIN, post(handlers::login))
        .route(r::DEVICES, get(handlers::list_devices))
        .route(
            "/api/v1/devices/{id}",
            patch(handlers::rename_device).delete(handlers::delete_device),
        )
        .route(
            r::SYNC_SETTINGS,
            get(handlers::get_settings).put(handlers::put_settings),
        )
        .route(
            r::SYNC_HISTORY,
            get(handlers::pull_history).post(handlers::push_history),
        )
        .with_state(state)
        // 全局请求体上限 1 MiB，防超大 payload（DoS 面收口）。
        .layer(DefaultBodyLimit::max(1_048_576))
}

/// 初始化日志（`LUMEN_LOG` 控制级别，默认 info）。
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("LUMEN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
