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
mod hub;
mod state;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, patch, post};
use axum::Router;
use lumen_protocol::routes as r;

use crate::config::Config;
use crate::hub::Hub;
use crate::state::AppState;

/// 后台清理周期：扫一遍未决配对，移除过期项（与配对码有效期对齐）。
const HUB_GC_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::from_env();
    // 安全告警：默认 JWT 密钥不安全（任何人可伪造 token）。局域网测试可容忍，
    // 公网部署务必经 LUMEN_JWT_SECRET 设强随机值。
    if config.uses_default_jwt_secret() {
        tracing::warn!(
            "⚠ 正在使用默认 JWT 密钥（不安全，仅限本地/局域网测试）；监听 {}。公网部署务必设置 LUMEN_JWT_SECRET！",
            config.bind_addr
        );
    }
    tracing::info!("连接数据库 …");
    let pool = db::create_pool(&config.database_url)?;
    db::init_schema(&pool).await?;
    tracing::info!("数据库就绪，建表完成");

    let bind_addr = config.bind_addr.clone();
    let hub = Arc::new(Hub::new());
    let state = AppState {
        pool,
        config: Arc::new(config),
        hub: hub.clone(),
    };
    // 后台 GC：周期清理过期未决配对（防内存泄漏 + 释放被占目标）。
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HUB_GC_INTERVAL);
        loop {
            ticker.tick().await;
            hub.gc();
        }
    });
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
        .route(r::HEARTBEAT, post(handlers::heartbeat))
        // M5.3 远程控制 WebSocket 中继（升级请求无 body，下方 DefaultBodyLimit 不影响它；
        // WS 帧大小另由 ws_handler 的 max_frame_size/max_message_size 收口）。
        .route(r::WS, get(ws::ws_handler))
        .with_state(state)
        // 全局请求体上限 1 MiB，防超大 payload（DoS 面收口）。仅作用于 REST 请求体。
        .layer(DefaultBodyLimit::max(1_048_576))
}

/// 初始化日志（`LUMEN_LOG` 控制级别，默认 info）。
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("LUMEN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
