//! 共享应用状态（axum `State`）。

use std::sync::Arc;

use deadpool_postgres::Pool;

use crate::config::Config;
use crate::hub::Hub;

/// axum handler 间共享的状态。克隆廉价：`Pool` 内部 `Arc`，`Config`/`Hub` 包 `Arc`。
#[derive(Clone)]
pub struct AppState {
    /// Postgres 连接池（deadpool + tokio-postgres）。
    pub pool: Pool,
    /// 运行配置。
    pub config: Arc<Config>,
    /// M5.3 远程控制中继枢纽（in-memory presence / 配对 / 会话状态机）。
    pub hub: Arc<Hub>,
}
