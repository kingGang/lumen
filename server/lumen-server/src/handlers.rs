//! HTTP handler：账户、设备、设置/历史同步。

use axum::extract::{Path, Query, State};
use axum::Json;
use deadpool_postgres::Client;
use lumen_protocol::{
    AuthResponse, DeviceInfo, DeviceListResponse, DeviceRecord, HistoryEntry, HistoryPullResponse,
    HistoryPushRequest, LoginRequest, RegisterRequest, RenameDeviceRequest, SettingsSync, UserInfo,
    PROTOCOL_VERSION,
};
use serde::Deserialize;

use crate::auth::{self, AuthUser};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 单次历史推送的最大条目数（超出拒绝，防批量灌库）。
const MAX_HISTORY_BATCH: usize = 5000;
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

/// 登记/更新设备：带有效 `device_id` 且属于本账户则更新 `last_seen`，
/// 否则新建一台并返回其 id（容错客户端持有陈旧 id 的情况）。
async fn upsert_device(
    client: &Client,
    user_id: &str,
    device: &DeviceInfo,
) -> AppResult<String> {
    let now = auth::now_secs();
    if let Some(did) = device.device_id.as_ref().filter(|s| !s.is_empty()) {
        let affected = client
            .execute(
                "UPDATE devices SET last_seen=$1, os=$2, app_version=$3 WHERE id=$4 AND user_id=$5",
                &[&now, &device.os, &device.app_version, did, &user_id],
            )
            .await?;
        if affected == 1 {
            return Ok(did.clone());
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

/// `GET /devices`：本账户全部设备（按 `last_seen` 倒序，含在线/本机标记）。
pub async fn list_devices(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<DeviceListResponse>> {
    let client = state.pool.get().await?;
    let rows = client
        .query(
            "SELECT id, name, os, app_version, last_seen FROM devices WHERE user_id = $1 ORDER BY last_seen DESC",
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

/// `GET /sync/history?since=<ts_ms>`：拉取增量历史（按 `ts` 升序，单批上限 5000）。
pub async fn pull_history(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<HistorySince>,
) -> AppResult<Json<HistoryPullResponse>> {
    let client = state.pool.get().await?;
    let rows = client
        .query(
            "SELECT text, ts, cwd, exit_code FROM history_entries WHERE user_id=$1 AND ts > $2 ORDER BY ts ASC LIMIT 5000",
            &[&user.user_id, &q.since],
        )
        .await?;
    let mut watermark = q.since;
    let entries: Vec<HistoryEntry> = rows
        .iter()
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
    Ok(Json(HistoryPullResponse { entries, watermark }))
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
