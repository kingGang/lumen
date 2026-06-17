//! 密码哈希（argon2）、JWT 签发/校验、以及鉴权提取器 [`AuthUser`]。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;

/// 对明文密码做 argon2 哈希。salt 取 uuid v4 字节（122 位随机），
/// 避开 `OsRng` 的特性依赖问题，跨平台稳定。
pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt_bytes = uuid::Uuid::new_v4().into_bytes();
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AppError::Internal(format!("salt 编码失败: {e}")))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("argon2 哈希失败: {e}")))?;
    Ok(hash.to_string())
}

/// 校验明文密码是否匹配存储的 argon2 哈希（哈希损坏视为不匹配，不 panic）。
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// JWT 载荷。
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// 账户 id。
    pub sub: String,
    /// 设备 id。
    pub did: String,
    /// 过期 Unix 秒。
    pub exp: usize,
}

/// 签发 JWT，返回 `(token, 过期 Unix 秒)`。
pub fn issue_token(
    secret: &str,
    user_id: &str,
    device_id: &str,
    ttl_secs: i64,
) -> Result<(String, i64), AppError> {
    let exp_secs = now_secs().saturating_add(ttl_secs.max(60));
    let claims = Claims {
        sub: user_id.to_string(),
        did: device_id.to_string(),
        // 不用 `as` 强转（项目 clippy deny cast_*）：try_from 兜底。
        exp: usize::try_from(exp_secs).unwrap_or(usize::MAX),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("JWT 签发失败: {e}")))?;
    Ok((token, exp_secs))
}

/// 校验 JWT，返回 claims（失败一律映射为 [`AppError::Unauthorized`]）。
pub fn verify_token(secret: &str, token: &str) -> Result<Claims, AppError> {
    let mut validation = Validation::default();
    // 本服务的 token 不含 audience：关闭 aud 校验。
    validation.validate_aud = false;
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| AppError::Unauthorized)?;
    Ok(data.claims)
}

/// 当前 Unix 秒（系统时钟早于 1970 的极端情形记 0）。
pub fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// 鉴权提取器：从 `Authorization: Bearer <jwt>` 解出当前用户/设备。
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// 账户 id。
    pub user_id: String,
    /// 设备 id。
    pub device_id: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or(AppError::Unauthorized)?;
        let claims = verify_token(&state.config.jwt_secret, token.trim())?;
        Ok(AuthUser {
            user_id: claims.sub,
            device_id: claims.did,
        })
    }
}
