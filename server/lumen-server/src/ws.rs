//! M5.3 终端远程：WebSocket 传输层（axum 升级 + 单连接 socket 循环）。
//!
//! 职责边界——本模块只管**传输**：JWT 鉴权（复用 [`AuthUser`] 提取器，走
//! `Authorization` 头而非 query，避免反代日志泄漏 token）、DB 握手（取设备名、
//! 刷 `last_seen`）、WebSocket 升级、单连接 `tokio::select!` 读写循环、JSON
//! 帧编解码；所有**状态机逻辑**（配对 / 独占 / 会话）下沉 [`crate::hub::Hub`]。
//!
//! 每条连接一个 task：`select!` 在「socket 收到客户端消息」与「Hub 经 mpsc 投递
//! 出站消息」两路间多路复用，单 task 独占 socket，无需 split，临界区零 `.await`。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use lumen_protocol::remote::{RemoteC2S, RemoteS2C};
use tokio::sync::mpsc::unbounded_channel;

use crate::auth::{self, AuthUser};
use crate::hub::ToClient;
use crate::state::AppState;

/// 单条 WS 消息上限（4 MiB）：part1 控制消息极小，留余量给 part2/3 的状态快照。
const MAX_WS_MESSAGE: usize = 4 * 1024 * 1024;
/// 单帧上限（4 MiB）：限制单个 WebSocket 帧，收口内存 DoS。
const MAX_WS_FRAME: usize = 4 * 1024 * 1024;
/// WS 连接内 `last_seen` 刷新节流（秒）：避免每个 Ping 都打库。
const LAST_SEEN_THROTTLE_SECS: i64 = 25;

/// `GET /api/v1/ws`：远程控制 WebSocket 升级入口。
///
/// [`AuthUser`] 提取器先行完成 JWT 鉴权（失败即 401，不升级）；通过后升级并交
/// [`handle_socket`] 跑连接循环。
pub async fn ws_handler(
    State(state): State<AppState>,
    user: AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_FRAME)
        .on_upgrade(move |socket| handle_socket(socket, state, user.user_id, user.device_id))
}

/// 单连接生命周期：DB 握手 → 登记 Hub → 发 `Welcome` → 读写循环 → 断开清理。
async fn handle_socket(mut socket: WebSocket, state: AppState, user_id: String, device_id: String) {
    // —— DB 握手：取设备名 + 刷 last_seen ——
    let name = match lookup_device_name(&state, &device_id, &user_id).await {
        Some(n) => n,
        None => {
            // token 指向的设备已不存在（被删等）：不建立 presence，关闭连接。
            tracing::warn!("WS 拒绝：设备 {device_id} 不存在或不属于该账户");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    touch_last_seen(&state, &device_id, &user_id).await;

    // —— 登记 Hub（同 device 旧连接会被驱逐）——
    let (tx, mut rx) = unbounded_channel::<ToClient>();
    let conn_id = state
        .hub
        .register(&device_id, user_id.clone(), name, tx.clone());
    // 立即下发 Welcome（经 mpsc 由本循环写出）。
    let _ = tx.send(ToClient::Msg(Box::new(crate::hub::Hub::welcome(&device_id))));

    let mut last_seen_at = auth::now_secs();

    // —— 读写循环 ——
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<RemoteC2S>(text.as_str()) {
                            Ok(RemoteC2S::Ping) => {
                                // Ping 同时承担保活：节流刷 last_seen，回 Pong。
                                let now = auth::now_secs();
                                if now - last_seen_at >= LAST_SEEN_THROTTLE_SECS {
                                    last_seen_at = now;
                                    touch_last_seen(&state, &device_id, &user_id).await;
                                }
                                let _ = tx.send(ToClient::Msg(Box::new(RemoteS2C::Pong)));
                            }
                            Ok(msg) => state.hub.handle(&device_id, conn_id, msg),
                            Err(e) => {
                                // 非法 JSON：不外泄消息体内容，仅记错误类型后继续读。
                                tracing::debug!("WS 消息解析失败（device={device_id}）: {e}");
                            }
                        }
                    }
                    // 二进制 / 控制帧（含底层 Ping/Pong）：part1 不使用，忽略。
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::debug!("WS 读出错（device={device_id}）: {e}");
                        break;
                    }
                }
            }
            outbound = rx.recv() => {
                match outbound {
                    Some(ToClient::Msg(msg)) => {
                        let Ok(text) = serde_json::to_string(&*msg) else {
                            tracing::error!("WS 出站消息序列化失败");
                            continue;
                        };
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break; // 对端已断。
                        }
                    }
                    // 被新连接驱逐：礼貌关闭后退出。
                    Some(ToClient::Close) | None => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }

    // —— 断开清理（conn_id 守卫：被驱逐的旧连接不会误删新连接状态）——
    state.hub.disconnect(&device_id, conn_id);
}

/// 取设备显示名（限定本账户）；不存在返回 `None`。
async fn lookup_device_name(state: &AppState, device_id: &str, user_id: &str) -> Option<String> {
    let client = state.pool.get().await.ok()?;
    let row = client
        .query_opt(
            "SELECT name FROM devices WHERE id = $1 AND user_id = $2",
            &[&device_id, &user_id],
        )
        .await
        .ok()??;
    Some(row.get(0))
}

/// 刷新本设备 `last_seen`（与 M5.2 REST 心跳同一字段，保持 online 判定一致）。
/// 失败仅记日志、不致命——presence 仍由 Hub 内存态兜底。
async fn touch_last_seen(state: &AppState, device_id: &str, user_id: &str) {
    let now = auth::now_secs();
    let Ok(client) = state.pool.get().await else {
        return;
    };
    if let Err(e) = client
        .execute(
            "UPDATE devices SET last_seen=$1 WHERE id=$2 AND user_id=$3",
            &[&now, &device_id, &user_id],
        )
        .await
    {
        tracing::debug!("WS 刷新 last_seen 失败（device={device_id}）: {e}");
    }
}
