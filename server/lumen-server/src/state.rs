//! 共享应用状态（axum `State`）。

use std::sync::Arc;

use deadpool_postgres::Pool;

use crate::config::Config;

/// axum handler 间共享的状态。克隆廉价：`Pool` 内部 `Arc`，`Config` 包 `Arc`。
#[derive(Clone)]
pub struct AppState {
    /// Postgres 连接池（deadpool + tokio-postgres）。
    pub pool: Pool,
    /// 运行配置。
    pub config: Arc<Config>,
}
