//! 数据库：deadpool 连接池 + 启动时幂等建表（`CREATE TABLE IF NOT EXISTS`）。
//!
//! 设计取舍：id 用 `TEXT` 存 uuid 字符串，时间戳用 `BIGINT` 存 Unix 秒/毫秒，
//! 列类型与 tokio-postgres 的 `ToSql`/`FromSql` 基础映射一一对应，零额外特性。
//! 不用迁移工具，免 sqlx-cli / 编译期连库。

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::NoTls;

/// 由连接串建立 deadpool 连接池。
///
/// 连接串走标准 libpq URL 形式（如
/// `postgres://user:pass@host:5432/db?sslmode=disable`）。本地 docker 不强制
/// SSL，故用 `NoTls`；生产 server↔Postgres 走同主机/内网。
pub fn create_pool(database_url: &str) -> anyhow::Result<Pool> {
    let pg_config: tokio_postgres::Config = database_url.parse()?;
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let mgr = Manager::from_config(pg_config, NoTls, mgr_config);
    let pool = Pool::builder(mgr).max_size(10).build()?;
    Ok(pool)
}

/// 启动时建表（幂等）。多条 DDL 用 `batch_execute` 一次性执行。
pub async fn init_schema(pool: &Pool) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .batch_execute(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id            TEXT PRIMARY KEY,
                email         TEXT NOT NULL UNIQUE,
                display_name  TEXT NOT NULL,
                password_hash TEXT NOT NULL,
                created_at    BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS devices (
                id          TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                name        TEXT NOT NULL,
                os          TEXT NOT NULL,
                app_version TEXT NOT NULL,
                last_seen   BIGINT NOT NULL,
                created_at  BIGINT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS devices_user_idx ON devices(user_id);
            CREATE TABLE IF NOT EXISTS settings_sync (
                user_id    TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
                version    BIGINT NOT NULL,
                data       TEXT NOT NULL,
                updated_at BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS history_entries (
                user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                text       TEXT NOT NULL,
                ts         BIGINT NOT NULL,
                cwd        TEXT,
                exit_code  INTEGER,
                PRIMARY KEY (user_id, text, ts)
            );
            CREATE INDEX IF NOT EXISTS history_user_ts_idx ON history_entries(user_id, ts);
            "#,
        )
        .await?;
    Ok(())
}
