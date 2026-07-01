//! HTTP handler：账户、设备、设置/历史同步。

use axum::extract::{Path, Query, State};
use axum::Json;
use deadpool_postgres::Client;
use lumen_protocol::{
    AuthResponse, DeviceInfo, DeviceListResponse, DeviceRecord, HistoryEntry, HistoryPullResponse,
    HistoryPushRequest, LoginRequest, RefreshResponse, RegisterRequest, RenameDeviceRequest,
    SettingsSync, UserInfo, PROTOCOL_VERSION,
};
use serde::Deserialize;

use crate::auth::{self, AuthUser};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 单次历史推送的最大条目数（超出拒绝，防批量灌库）。
const MAX_HISTORY_BATCH: usize = 5000;
/// 单次历史拉取返回的最大条目数（超出截断并置 `has_more`，客户端续拉）。
const HISTORY_PULL_LIMIT: i64 = 5000;
/// 偏好 blob 最大字节数（512 KiB）。
const MAX_SETTINGS_BYTES: usize = 512 * 1024;
/// 设备名最大长度（字节）。
const MAX_NAME_LEN: usize = 200;

/// `GET /health`：存活探针 + 协议版本。
pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "protocol_version": PROTOCOL_VERSION }))
}

/// `POST /heartbeat`：保持本设备在线（刷新 `last_seen`）。M5.2 在线状态机制。
pub async fn heartbeat(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    let now = auth::now_secs();
    let client = state.pool.get().await?;
    client
        .execute(
            "UPDATE devices SET last_seen=$1 WHERE id=$2 AND user_id=$3",
            &[&now, &user.device_id, &user.user_id],
        )
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 邮箱 `@` 前段作为展示名。
fn display_name_of(email: &str) -> String {
    email.split('@').next().unwrap_or("").to_string()
}

/// 生成新的 uuid 字符串 id。
fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `POST /auth/register`：创建账户（argon2 哈希密码），返回账户信息。
pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> AppResult<Json<UserInfo>> {
    let email = req.email.trim().to_lowercase();
    if !email.contains('@') || req.password.is_empty() {
        return Err(AppError::BadRequest("邮箱或密码格式不正确".into()));
    }
    let client = state.pool.get().await?;
    let existing = client
        .query_opt("SELECT id FROM users WHERE email = $1", &[&email])
        .await?;
    if existing.is_some() {
        return Err(AppError::EmailTaken);
    }
    let id = new_id();
    let display_name = display_name_of(&email);
    let hash = auth::hash_password(&req.password)?;
    let created = auth::now_secs();
    client
        .execute(
            "INSERT INTO users (id, email, display_name, password_hash, created_at) VALUES ($1,$2,$3,$4,$5)",
            &[&id, &email, &display_name, &hash, &created],
        )
        .await?;
    Ok(Json(UserInfo {
        id,
        email,
        display_name,
    }))
}

/// `POST /auth/login`：校验密码、登记/更新本设备、签发 JWT。
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> AppResult<Json<AuthResponse>> {
    let email = req.email.trim().to_lowercase();
    let client = state.pool.get().await?;
    let row = client
        .query_opt(
            "SELECT id, display_name, password_hash FROM users WHERE email = $1",
            &[&email],
        )
        .await?
        .ok_or(AppError::UserNotFound)?;
    let user_id: String = row.get(0);
    let display_name: String = row.get(1);
    let password_hash: String = row.get(2);
    if !auth::verify_password(&req.password, &password_hash) {
        return Err(AppError::InvalidCredentials);
    }
    let device_id = upsert_device(&client, &user_id, &req.device).await?;
    let (token, expires_at) = auth::issue_token(
        &state.config.jwt_secret,
        &user_id,
        &device_id,
        state.config.token_ttl_secs,
    )?;
    Ok(Json(AuthResponse {
        protocol_version: PROTOCOL_VERSION,
        token,
        expires_at,
        user: UserInfo {
            id: user_id,
            email,
            display_name,
        },
        device_id,
    }))
}

/// `POST /auth/refresh`：用现有**有效** token（经 [`AuthUser`] 提取器校验通过）换发新 token，
/// 客户端在快到期时调用，避免 7 天到期后对 WS / `/devices` / `/heartbeat` 全面 401 掉线。
/// 无需查库 / 密码——`AuthUser` 已从旧 token 解出 `user_id`/`device_id`；旧 token 已过期则提取器
/// 直接 401（续期失败，需重新登录）。在线状态由心跳维持，此处不重复刷 `last_seen`。
pub async fn refresh(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<RefreshResponse>> {
    let (token, expires_at) = auth::issue_token(
        &state.config.jwt_secret,
        &user.user_id,
        &user.device_id,
        state.config.token_ttl_secs,
    )?;
    Ok(Json(RefreshResponse { token, expires_at }))
}

/// 登记/更新设备，返回该设备在服务端的稳定 id。
///
/// **幂等认领（修「幽灵设备」核心）**：优先按客户端上报的稳定硬件标识
/// `(user_id, hw_id)` 认领同一物理机的唯一行——无论这次带的 `device_id` 是旧的、空的
/// 还是丢了，只要 `hw_id` 命中就复用同一行、**绝不新建**，从根上杜绝「同机分裂出幽灵行」。
/// 无 `hw_id`（老客户端 / 取不到）时退化回原「按 `device_id` 更新，否则新建」的行为，兼容老端。
async fn upsert_device(
    client: &Client,
    user_id: &str,
    device: &DeviceInfo,
) -> AppResult<String> {
    let now = auth::now_secs();
    let hw_id = device.hw_id.as_deref().filter(|s| !s.is_empty());
    let did = device.device_id.as_deref().filter(|s| !s.is_empty());

    // —— 1) 有稳定硬件标识：按 (user_id, hw_id) 幂等认领 ——
    if let Some(hw) = hw_id {
        // 1a) 已登记过这台机器 → 复用其行（顺带刷新 os/版本/last_seen）。
        if let Some(row) = client
            .query_opt(
                "SELECT id FROM devices WHERE user_id=$1 AND hw_id=$2 ORDER BY created_at ASC LIMIT 1",
                &[&user_id, &hw],
            )
            .await?
        {
            let id: String = row.get(0);
            client
                .execute(
                    "UPDATE devices SET last_seen=$1, os=$2, app_version=$3 WHERE id=$4",
                    &[&now, &device.os, &device.app_version, &id],
                )
                .await?;
            return Ok(id);
        }
        // 1b) 尚无本机 hw 行，但客户端带着一条「属于本账户、hw_id 仍为 NULL」的旧行
        //     （滚动升级：老客户端建的行）→ 采纳并回填 hw_id，复用老行、id 不变，
        //     滚动升级期零新增幽灵。先 1a 再 1b 的顺序可避开与既有 hw 行的唯一冲突。
        if let Some(d) = did {
            let affected = client
                .execute(
                    "UPDATE devices SET hw_id=$1, last_seen=$2, os=$3, app_version=$4 \
                     WHERE id=$5 AND user_id=$6 AND hw_id IS NULL",
                    &[&hw, &now, &device.os, &device.app_version, &d, &user_id],
                )
                .await?;
            if affected == 1 {
                return Ok(d.to_string());
            }
        }
        // 1c) 全新机器 → 插入带 hw_id 的新行；并发登录以部分唯一索引兜底
        //     （撞索引则改为认领已存在的那行），最终该机恰好一行。
        let id = new_id();
        let row = client
            .query_opt(
                "INSERT INTO devices (id,user_id,name,os,app_version,last_seen,created_at,hw_id) \
                 VALUES ($1,$2,$3,$4,$5,$6,$6,$7) \
                 ON CONFLICT (user_id, hw_id) WHERE hw_id IS NOT NULL \
                 DO UPDATE SET last_seen=EXCLUDED.last_seen, os=EXCLUDED.os, app_version=EXCLUDED.app_version \
                 RETURNING id",
                &[&id, &user_id, &device.name, &device.os, &device.app_version, &now, &hw],
            )
            .await?;
        return Ok(row.map_or(id, |r| r.get(0)));
    }

    // —— 2) 无 hw_id（老客户端 / 取不到）：保持原行为——按 device_id 更新，否则新建 ——
    if let Some(d) = did {
        let affected = client
            .execute(
                "UPDATE devices SET last_seen=$1, os=$2, app_version=$3 WHERE id=$4 AND user_id=$5",
                &[&now, &device.os, &device.app_version, &d, &user_id],
            )
            .await?;
        if affected == 1 {
            return Ok(d.to_string());
        }
    }
    let id = new_id();
    client
        .execute(
            "INSERT INTO devices (id,user_id,name,os,app_version,last_seen,created_at) VALUES ($1,$2,$3,$4,$5,$6,$6)",
            &[&id, &user_id, &device.name, &device.os, &device.app_version, &now],
        )
        .await?;
    Ok(id)
}

/// `GET /devices`：本账户全部设备（含在线/本机标记）。
///
/// 按注册时间 `created_at` 升序（`id` 兜底确保完全确定性）——**稳定排序**：老实现按
/// `last_seen DESC`，而 `last_seen` 每次心跳都变，导致列表每刷新一次就重排、设备跳位
/// （海风哥反馈）。改按注册先后固定顺序，新设备排末尾、既有设备不再移动。
pub async fn list_devices(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<DeviceListResponse>> {
    let client = state.pool.get().await?;
    let rows = client
        .query(
            "SELECT id, name, os, app_version, last_seen FROM devices WHERE user_id = $1 ORDER BY created_at ASC, id ASC",
            &[&user.user_id],
        )
        .await?;
    let now = auth::now_secs();
    let window = state.config.online_window_secs;
    let devices = rows
        .iter()
        .map(|row| {
            let id: String = row.get(0);
            let last_seen: i64 = row.get(4);
            DeviceRecord {
                online: now - last_seen <= window,
                is_self: id == user.device_id,
                name: row.get(1),
                os: row.get(2),
                app_version: row.get(3),
                last_seen,
                id,
            }
        })
        .collect();
    Ok(Json(DeviceListResponse { devices }))
}

/// `PATCH /devices/{id}`：重命名本账户下的设备。
pub async fn rename_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<RenameDeviceRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("设备名不能为空".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(AppError::BadRequest("设备名过长".into()));
    }
    let client = state.pool.get().await?;
    let affected = client
        .execute(
            "UPDATE devices SET name=$1 WHERE id=$2 AND user_id=$3",
            &[&name, &id, &user.user_id],
        )
        .await?;
    if affected == 0 {
        return Err(AppError::DeviceNotFound);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `DELETE /devices/{id}`：删除本账户下的设备。
pub async fn delete_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let client = state.pool.get().await?;
    let affected = client
        .execute(
            "DELETE FROM devices WHERE id=$1 AND user_id=$2",
            &[&id, &user.user_id],
        )
        .await?;
    if affected == 0 {
        return Err(AppError::DeviceNotFound);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `GET /sync/settings`：拉取偏好 blob（无则返回 version=0、空 data）。
pub async fn get_settings(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<SettingsSync>> {
    let client = state.pool.get().await?;
    let row = client
        .query_opt(
            "SELECT version, data FROM settings_sync WHERE user_id = $1",
            &[&user.user_id],
        )
        .await?;
    let (version, data) = match row {
        Some(r) => (r.get::<_, i64>(0), r.get::<_, String>(1)),
        None => (0_i64, String::new()),
    };
    Ok(Json(SettingsSync { version, data }))
}

/// `PUT /sync/settings`：推送偏好 blob（仅当 `version` 更大才覆盖，last-write-wins）。
/// 返回服务端当前权威值。
pub async fn put_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<SettingsSync>,
) -> AppResult<Json<SettingsSync>> {
    if req.data.len() > MAX_SETTINGS_BYTES {
        return Err(AppError::BadRequest(format!(
            "偏好数据 {} 字节超过上限 {MAX_SETTINGS_BYTES}",
            req.data.len()
        )));
    }
    let client = state.pool.get().await?;
    let now = auth::now_secs();
    client
        .execute(
            r#"
            INSERT INTO settings_sync (user_id, version, data, updated_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (user_id) DO UPDATE
                SET version = EXCLUDED.version,
                    data = EXCLUDED.data,
                    updated_at = EXCLUDED.updated_at
                WHERE settings_sync.version < EXCLUDED.version
            "#,
            &[&user.user_id, &req.version, &req.data, &now],
        )
        .await?;
    let row = client
        .query_opt(
            "SELECT version, data FROM settings_sync WHERE user_id = $1",
            &[&user.user_id],
        )
        .await?;
    let (version, data) = match row {
        Some(r) => (r.get::<_, i64>(0), r.get::<_, String>(1)),
        None => (req.version, req.data),
    };
    Ok(Json(SettingsSync { version, data }))
}

/// `GET /sync/history` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct HistorySince {
    /// 拉取 `ts` 严格大于此值的条目（毫秒水位线，缺省 0）。
    #[serde(default)]
    pub since: i64,
}

/// `GET /sync/history?since=<ts_ms>`：拉取增量历史（按 `ts` 升序，单批上限 `HISTORY_PULL_LIMIT`）。
/// 多取 1 条精确判定 `has_more`：若查回 limit+1 条，则截回 limit 条并置 `has_more=true`，客户端
/// 据此用新 `since=watermark` 续拉。
///
/// 注：水位线按 `ts`、续拉用严格 `ts > watermark`——理论上同一毫秒内 >limit 条会在批边界丢同 ts
/// 尾条（需 `(ts,id)` 复合游标才完全杜绝），但单毫秒 5000 条历史不现实，故不为此引入 id 游标。
pub async fn pull_history(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<HistorySince>,
) -> AppResult<Json<HistoryPullResponse>> {
    let client = state.pool.get().await?;
    let rows = client
        .query(
            "SELECT text, ts, cwd, exit_code FROM history_entries WHERE user_id=$1 AND ts > $2 ORDER BY ts ASC LIMIT $3",
            &[&user.user_id, &q.since, &(HISTORY_PULL_LIMIT + 1)],
        )
        .await?;
    // 多取的第 limit+1 条仅用于判定后续是否还有，不返回给客户端。
    let has_more = rows.len() as i64 > HISTORY_PULL_LIMIT;
    let take = rows.len().min(HISTORY_PULL_LIMIT as usize);
    let mut watermark = q.since;
    let entries: Vec<HistoryEntry> = rows
        .iter()
        .take(take)
        .map(|row| {
            let ts: i64 = row.get(1);
            if ts > watermark {
                watermark = ts;
            }
            HistoryEntry {
                text: row.get(0),
                ts,
                cwd: row.get(2),
                exit_code: row.get(3),
            }
        })
        .collect();
    Ok(Json(HistoryPullResponse {
        entries,
        watermark,
        has_more,
    }))
}

/// `POST /sync/history`：推送历史，多设备来源按 `(user, text, ts)` 去重合并。
pub async fn push_history(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<HistoryPushRequest>,
) -> AppResult<Json<serde_json::Value>> {
    if req.entries.len() > MAX_HISTORY_BATCH {
        return Err(AppError::BadRequest(format!(
            "单批历史条目数 {} 超过上限 {MAX_HISTORY_BATCH}",
            req.entries.len()
        )));
    }
    let client = state.pool.get().await?;
    let mut inserted: u64 = 0;
    for e in &req.entries {
        let n = client
            .execute(
                r#"
                INSERT INTO history_entries (user_id, text, ts, cwd, exit_code)
                VALUES ($1,$2,$3,$4,$5)
                ON CONFLICT (user_id, text, ts) DO NOTHING
                "#,
                &[&user.user_id, &e.text, &e.ts, &e.cwd, &e.exit_code],
            )
            .await?;
        inserted += n;
    }
    Ok(Json(serde_json::json!({ "inserted": inserted })))
}

#[cfg(test)]
mod tests {
    //! `upsert_device` 幂等认领的 DB 集成测试（修「幽灵设备」核心回归）。
    //!
    //! 需要可达的 Postgres，故默认 `#[ignore]`——CI 无库时不阻塞；本地对接开发库跑：
    //! `cargo test -p lumen-server -- --ignored`（可用 `LUMEN_TEST_DATABASE_URL` 指定库）。
    //! 每个测试用 uuid 临时账户、结束即 `DELETE`（`ON DELETE CASCADE` 顺带清设备），不污染既有数据。

    use super::*;
    use deadpool_postgres::Pool;
    use lumen_protocol::DeviceInfo;

    fn test_db_url() -> String {
        std::env::var("LUMEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://lumen_user:lumen_password@127.0.0.1:5544/lumen?sslmode=disable".to_string()
        })
    }

    /// 进程内只建一次表：并行测试各自并发跑 DDL（`ALTER TABLE`/`CREATE INDEX` 取
    /// AccessExclusiveLock）会互相死锁，故用 `OnceCell` 串行化、只跑一次。
    async fn ensure_schema(pool: &Pool) {
        static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SCHEMA
            .get_or_init(|| async {
                crate::db::init_schema(pool).await.expect("建表");
            })
            .await;
    }

    /// 建池 + 幂等建表（含 hw_id 列/索引）+ 一个临时账户，返回 `(pool, user_id)`。
    async fn setup() -> (Pool, String) {
        let pool = crate::db::create_pool(&test_db_url()).expect("建连接池");
        ensure_schema(&pool).await;
        let client = pool.get().await.expect("取连接");
        let uid = new_id();
        client
            .execute(
                "INSERT INTO users (id,email,display_name,password_hash,created_at) VALUES ($1,$2,$3,$4,$5)",
                &[&uid, &format!("{uid}@test.local"), &"t", &"x", &auth::now_secs()],
            )
            .await
            .expect("建临时用户");
        (pool, uid)
    }

    async fn teardown(pool: &Pool, uid: &str) {
        if let Ok(c) = pool.get().await {
            let _ = c.execute("DELETE FROM users WHERE id=$1", &[&uid]).await;
        }
    }

    fn dev(device_id: Option<&str>, hw_id: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            device_id: device_id.map(str::to_string),
            hw_id: hw_id.map(str::to_string),
            name: "PC".into(),
            os: "windows".into(),
            app_version: "1.0.0".into(),
        }
    }

    async fn device_count(pool: &Pool, uid: &str) -> i64 {
        let c = pool.get().await.expect("取连接");
        c.query_one("SELECT COUNT(*) FROM devices WHERE user_id=$1", &[&uid])
            .await
            .expect("count")
            .get(0)
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn hw幂等_同机重登复用同一行不产幽灵() {
        let (pool, uid) = setup().await;
        let c = pool.get().await.expect("连接");
        // 首次：带 hw、无 device_id → 新建。
        let id1 = upsert_device(&c, &uid, &dev(None, Some("HW-1"))).await.expect("首登");
        // 重登：带 hw、device_id 为空（模拟本地文件丢失）→ 复用同一行，绝不新建。
        let id2 = upsert_device(&c, &uid, &dev(None, Some("HW-1"))).await.expect("重登-空id");
        // 重登：带 hw、带一个陈旧/错误 device_id → 仍复用同一行。
        let id3 = upsert_device(&c, &uid, &dev(Some("STALE-XYZ"), Some("HW-1"))).await.expect("重登-异id");
        assert_eq!(id1, id2, "空 device_id 应按 hw 复用");
        assert_eq!(id1, id3, "异 device_id 应按 hw 复用");
        assert_eq!(device_count(&pool, &uid).await, 1, "同机恰好一行，无幽灵");
        teardown(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn hw采纳老did行_滚动升级零幽灵() {
        let (pool, uid) = setup().await;
        let c = pool.get().await.expect("连接");
        // 老客户端建的行：无 hw（legacy 分支）。
        let old = upsert_device(&c, &uid, &dev(None, None)).await.expect("老登");
        assert_eq!(device_count(&pool, &uid).await, 1);
        // 升级后带 hw + 老 did 回来 → 采纳老行、回填 hw、id 不变，不新建。
        let adopted = upsert_device(&c, &uid, &dev(Some(&old), Some("HW-2"))).await.expect("升级登");
        assert_eq!(adopted, old, "应复用并回填老行");
        assert_eq!(device_count(&pool, &uid).await, 1, "滚动升级不新增行");
        // 之后即便 did 丢了，靠 hw 仍复用同一行。
        let again = upsert_device(&c, &uid, &dev(None, Some("HW-2"))).await.expect("再登");
        assert_eq!(again, old);
        assert_eq!(device_count(&pool, &uid).await, 1);
        teardown(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn 老客户端无hw_保持旧行为() {
        let (pool, uid) = setup().await;
        let c = pool.get().await.expect("连接");
        let id1 = upsert_device(&c, &uid, &dev(None, None)).await.expect("首登");
        // 带回该 did → 复用。
        let id2 = upsert_device(&c, &uid, &dev(Some(&id1), None)).await.expect("重登");
        assert_eq!(id1, id2);
        // 带一个查不到的 did → 无 hw 无从幂等，退化回老行为「新建」。
        let id3 = upsert_device(&c, &uid, &dev(Some("NOPE"), None)).await.expect("异id");
        assert_ne!(id3, id1);
        assert_eq!(device_count(&pool, &uid).await, 2);
        teardown(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn 跨账户同hw互不合并() {
        let (pool, uid_a) = setup().await;
        let (_pool_b, uid_b) = setup().await;
        let c = pool.get().await.expect("连接");
        let a = upsert_device(&c, &uid_a, &dev(None, Some("SHARED-HW"))).await.expect("A 登");
        let b = upsert_device(&c, &uid_b, &dev(None, Some("SHARED-HW"))).await.expect("B 登");
        assert_ne!(a, b, "同 hw 不同账户应各自一行，按 (user_id,hw_id) 隔离");
        assert_eq!(device_count(&pool, &uid_a).await, 1);
        assert_eq!(device_count(&pool, &uid_b).await, 1);
        teardown(&pool, &uid_a).await;
        teardown(&pool, &uid_b).await;
    }
}
