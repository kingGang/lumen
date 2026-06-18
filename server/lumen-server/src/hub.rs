//! M5.3 终端远程：WebSocket 中继的**纯内存状态机**（presence + 配对 + 控制
//! 独占 + 会话生命周期 + 数据面盲转）。
//!
//! # 为何单锁
//! [`Hub`] 把 `peers`/`sessions`/`pending` 三张表收进**一把** [`std::sync::Mutex`]
//! （[`HubState`]）。早期设计的三把独立锁需规定全局加锁顺序、稍有不慎即 ABBA
//! 死锁；单锁让每个操作在一次加锁内原子完成「检查 + 改状态 + 投递出站消息」，
//! 从根上杜绝死锁与 TOCTOU 竞态。临界区内**绝不 `.await`**（投递走
//! [`tokio::sync::mpsc::UnboundedSender::send`]，同步非阻塞），故同步锁安全。
//!
//! # 为何零 DB 依赖
//! 设备名、`last_seen` 等 DB 交互全在 `ws.rs`（连接握手阶段，锁外 `await`）完成；
//! [`Hub`] 只吃已备好的内存参数（[`Hub::register`] 收 `name`/`user_id`）。如此
//! 状态机可脱离 Postgres / axum 完整单测（见本文件 `tests`）。
//!
//! # 连接代次（conn_id）
//! 同一 `device_id` 重复连接（两个客户端实例）时，新连接**驱逐**旧连接：旧会话
//! 拆除、旧 `pending` 取消、旧连接收 [`ToClient::Close`]。每条连接持唯一递增
//! `conn_id`，[`Hub::handle`] / [`Hub::disconnect`] 以它为守卫——旧连接的迟到
//! 消息与断开清理都不会误伤已就位的新连接。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use lumen_protocol::remote::{DenyReason, EndReason, PairingFailReason, RemoteC2S, RemoteS2C, Role};
use lumen_protocol::{MIN_SUPPORTED_VERSION, PROTOCOL_VERSION};
use tokio::sync::mpsc::UnboundedSender;

use crate::auth;

/// 配对码有效期（秒）：被控端展示后超时未完成即作废。
pub const PAIRING_TTL_SECS: i64 = 120;
/// 配对码最大尝试次数：连错此数后该配对作废（防暴力）。
pub const PAIRING_MAX_ATTEMPTS: u32 = 5;

/// 服务端 → 单条客户端连接的内部投递信号（经各连接的 mpsc 通道）。
#[derive(Debug)]
pub enum ToClient {
    /// 一条要写给客户端的协议消息。
    Msg(Box<RemoteS2C>),
    /// 要求该连接优雅关闭（被新连接驱逐时下发）。
    Close,
}

/// 在线连接句柄（驻留 [`HubState::peers`]）。
struct PeerHandle {
    /// 账户 id（跨用户隔离校验用）。
    user_id: String,
    /// 设备显示名（握手时由 DB 取，缓存于此）。
    name: String,
    /// 连接代次（驱逐 / 迟到消息守卫）。
    conn_id: u64,
    /// 出站通道发送端（接收端在该连接的 socket 写循环里）。
    tx: UnboundedSender<ToClient>,
}

/// 活跃会话条目（双端各插一条，互指对端）。
///
/// part1 中继逻辑与独占判定只需「对端是谁」（不区分角色——角色已随
/// [`RemoteS2C::SessionStarted`] 告知客户端）。part2/3 若需服务端感知角色
/// （如输入仲裁），在此追加 `role` 字段即可。
struct SessionEntry {
    /// 对端设备 id。
    peer: String,
}

/// 未决配对（key = 被控端 device_id）。
struct Pending {
    /// 发起请求的控制端 device_id（[`RemoteC2S::SubmitPairing`] 须由它提交）。
    controller_device_id: String,
    /// 服务端生成、仅下发给被控端展示的 9 位配对码。
    code: String,
    /// 剩余尝试次数。
    attempts_left: u32,
    /// 创建时刻（Unix 秒），用于过期判定。
    created_at: i64,
}

/// 单锁保护的全部中继状态。
#[derive(Default)]
struct HubState {
    /// 在线连接：device_id → 句柄。
    peers: HashMap<String, PeerHandle>,
    /// 活跃会话：device_id → 条目（控制端与被控端各一条）。
    sessions: HashMap<String, SessionEntry>,
    /// 未决配对：被控端 device_id → 配对态。
    pending: HashMap<String, Pending>,
}

/// 远程控制中继枢纽（`AppState` 持 `Arc<Hub>`）。
#[derive(Default)]
pub struct Hub {
    state: Mutex<HubState>,
    /// 连接代次发号器（单调递增，唯一）。
    conn_seq: AtomicU64,
}

impl Hub {
    /// 新建空枢纽。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取一次性递增的连接代次。
    fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// 加锁（poison 时取回内部值而非 panic / `unwrap`——临界区内无 panic 代码）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 登记一条新连接，返回其连接代次。
    ///
    /// 若同 `device_id` 已有旧连接，先**驱逐**：拆其会话、取消其 pending、给旧连接
    /// 发 [`ToClient::Close`]，再登记新连接。调用方在拿到 `conn_id` 后应立即给本
    /// 连接发 [`RemoteS2C::Welcome`]（见 [`Self::welcome`]）。
    pub fn register(
        &self,
        device_id: &str,
        user_id: String,
        name: String,
        tx: UnboundedSender<ToClient>,
    ) -> u64 {
        let conn_id = self.next_conn_id();
        let mut st = self.lock();
        if let Some(old) = st.peers.remove(device_id) {
            teardown_session(&mut st, device_id, EndReason::Replaced);
            cancel_pending_as_controller(&mut st, device_id);
            cancel_pending_as_target(&mut st, device_id, DenyReason::Offline);
            let _ = old.tx.send(ToClient::Close);
        }
        st.peers.insert(
            device_id.to_string(),
            PeerHandle {
                user_id,
                name,
                conn_id,
                tx,
            },
        );
        conn_id
    }

    /// 构造本连接的 [`RemoteS2C::Welcome`]（协议版本协商 + 确认 device_id）。
    #[must_use]
    pub fn welcome(device_id: &str) -> RemoteS2C {
        RemoteS2C::Welcome {
            protocol_version: PROTOCOL_VERSION,
            min_supported_version: MIN_SUPPORTED_VERSION,
            device_id: device_id.to_string(),
        }
    }

    /// 连接断开：以 `conn_id` 守卫后注销 peer、拆会话、取消相关 pending、通知对端。
    pub fn disconnect(&self, device_id: &str, conn_id: u64) {
        let mut st = self.lock();
        if !is_current(&st, device_id, conn_id) {
            return; // 已被新连接取代：本次断开不触动新连接状态。
        }
        st.peers.remove(device_id);
        teardown_session(&mut st, device_id, EndReason::PeerDisconnected);
        cancel_pending_as_controller(&mut st, device_id);
        cancel_pending_as_target(&mut st, device_id, DenyReason::Offline);
    }

    /// 处理一条客户端消息（[`RemoteC2S::Ping`] 由 `ws.rs` 处理以同时刷 `last_seen`）。
    ///
    /// `conn_id` 守卫：被驱逐的旧连接迟到消息一律忽略。
    pub fn handle(&self, device_id: &str, conn_id: u64, msg: RemoteC2S) {
        let mut st = self.lock();
        if !is_current(&st, device_id, conn_id) {
            return;
        }
        match msg {
            RemoteC2S::RequestControl { target } => request_control(&mut st, device_id, &target),
            RemoteC2S::SubmitPairing { target, code } => {
                submit_pairing(&mut st, device_id, &target, &code);
            }
            RemoteC2S::DeclineControl => decline_control(&mut st, device_id),
            RemoteC2S::EndSession => {
                teardown_session(&mut st, device_id, EndReason::PeerLeft);
            }
            RemoteC2S::Relay(value) => relay(&st, device_id, value),
            RemoteC2S::Ping => send_msg(&st.peers, device_id, RemoteS2C::Pong),
        }
    }

    /// 后台定时清理：移除过期 pending 并通知双方（控制端 `ControlDenied{Expired}`、
    /// 被控端 `PairingCancelled{Expired}`）。由 `main.rs` 周期调用。
    pub fn gc(&self) {
        let mut st = self.lock();
        purge_expired(&mut st);
    }
}

/// `device_id` 当前在线连接的代次是否等于 `conn_id`。
fn is_current(st: &HubState, device_id: &str, conn_id: u64) -> bool {
    st.peers.get(device_id).is_some_and(|p| p.conn_id == conn_id)
}

/// 向某设备投递一条消息（设备不在线则静默丢弃；通道已关亦忽略）。
fn send_msg(peers: &HashMap<String, PeerHandle>, device_id: &str, msg: RemoteS2C) {
    if let Some(peer) = peers.get(device_id) {
        let _ = peer.tx.send(ToClient::Msg(Box::new(msg)));
    }
}

/// 生成 9 位十进制配对码。
///
/// 取 uuid v4（122 位随机熵）低位对 10^9 取模——`2^122 mod 10^9` 引入的取模偏置
/// 在 `2^-90` 量级，可忽略；配合 [`PAIRING_MAX_ATTEMPTS`] 与 [`PAIRING_TTL_SECS`]
/// 足以抵御暴力猜测。
fn gen_pairing_code() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 1_000_000_000;
    format!("{n:09}")
}

/// 拆除 `device_id` 参与的会话（若有）：移除双端条目并通知对端 [`RemoteS2C::SessionEnded`]。
/// 返回是否确实拆了一个会话。
fn teardown_session(st: &mut HubState, device_id: &str, reason: EndReason) -> bool {
    let Some(entry) = st.sessions.remove(device_id) else {
        return false;
    };
    let peer = entry.peer;
    st.sessions.remove(&peer);
    send_msg(&st.peers, &peer, RemoteS2C::SessionEnded { reason });
    true
}

/// 取消 `device_id` 作为**控制端**发起的全部未决 pending，并通知各被控端
/// [`RemoteS2C::PairingCancelled`]（dismiss 配对码）。
fn cancel_pending_as_controller(st: &mut HubState, device_id: &str) {
    let targets: Vec<String> = st
        .pending
        .iter()
        .filter(|(_, p)| p.controller_device_id == device_id)
        .map(|(t, _)| t.clone())
        .collect();
    for t in targets {
        st.pending.remove(&t);
        send_msg(
            &st.peers,
            &t,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::ControllerLeft,
            },
        );
    }
}

/// 取消 `device_id` 作为**被控端**的未决 pending（若有），通知控制端 [`RemoteS2C::ControlDenied`]。
fn cancel_pending_as_target(st: &mut HubState, device_id: &str, reason: DenyReason) {
    if let Some(p) = st.pending.remove(device_id) {
        send_msg(
            &st.peers,
            &p.controller_device_id,
            RemoteS2C::ControlDenied {
                target_device_id: device_id.to_string(),
                reason,
            },
        );
    }
}

/// 移除全部过期 pending 并通知双方。
fn purge_expired(st: &mut HubState) {
    let now = auth::now_secs();
    let expired: Vec<(String, String)> = st
        .pending
        .iter()
        .filter(|(_, p)| now - p.created_at >= PAIRING_TTL_SECS)
        .map(|(t, p)| (t.clone(), p.controller_device_id.clone()))
        .collect();
    for (target, controller) in expired {
        st.pending.remove(&target);
        send_msg(
            &st.peers,
            &controller,
            RemoteS2C::ControlDenied {
                target_device_id: target.clone(),
                reason: DenyReason::Expired,
            },
        );
        send_msg(
            &st.peers,
            &target,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::Expired,
            },
        );
    }
}

/// 控制端发起控制请求：层层校验（自控 / 在线 / 同账户 / 控制端空闲 / 目标空闲 /
/// 目标无配对中），通过则生成配对码、存 pending、通知双方。
fn request_control(st: &mut HubState, controller_id: &str, target: &str) {
    let deny = |st: &HubState, reason: DenyReason| {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::ControlDenied {
                target_device_id: target.to_string(),
                reason,
            },
        );
    };

    // 自控。
    if controller_id == target {
        deny(st, DenyReason::SelfControl);
        return;
    }
    // 控制端身份（必在 peers——刚通过 handle 守卫）。
    let Some(controller) = st.peers.get(controller_id) else {
        return;
    };
    let controller_user = controller.user_id.clone();
    let controller_name = controller.name.clone();
    // 目标在线。
    let Some(target_peer) = st.peers.get(target) else {
        deny(st, DenyReason::Offline);
        return;
    };
    // 同账户（禁跨用户）。
    if target_peer.user_id != controller_user {
        deny(st, DenyReason::CrossUser);
        return;
    }
    let target_name = target_peer.name.clone();
    // 惰性清理过期 pending（避免 contains_key 命中陈旧项）。
    purge_expired(st);
    // 控制端已在会话中 / 已发起其它配对 → 忙（控制端同一刻只控一台）。
    if st.sessions.contains_key(controller_id)
        || st
            .pending
            .values()
            .any(|p| p.controller_device_id == controller_id)
    {
        deny(st, DenyReason::ControllerBusy);
        return;
    }
    // 目标已被控。
    if st.sessions.contains_key(target) {
        deny(st, DenyReason::AlreadyControlled);
        return;
    }
    // 目标正与他人配对中。
    if st.pending.contains_key(target) {
        deny(st, DenyReason::TargetPairing);
        return;
    }
    // 通过：生成码、登记 pending、双向通知。
    let code = gen_pairing_code();
    let now = auth::now_secs();
    st.pending.insert(
        target.to_string(),
        Pending {
            controller_device_id: controller_id.to_string(),
            code: code.clone(),
            attempts_left: PAIRING_MAX_ATTEMPTS,
            created_at: now,
        },
    );
    let ttl = u32::try_from(PAIRING_TTL_SECS).unwrap_or(120);
    send_msg(
        &st.peers,
        target,
        RemoteS2C::ControlRequested {
            controller_device_id: controller_id.to_string(),
            controller_name,
            pairing_code: code,
            expires_in_secs: ttl,
        },
    );
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::PairingNeeded {
            target_device_id: target.to_string(),
            target_name,
            expires_in_secs: ttl,
        },
    );
}

/// 控制端提交配对码：校验提交者身份 + 有效期 + 码值，成功原子建会话、双向
/// [`RemoteS2C::SessionStarted`]；失败按原因回 [`RemoteS2C::PairingResult`]。
fn submit_pairing(st: &mut HubState, controller_id: &str, target: &str, code: &str) {
    let fail = |st: &HubState, reason: PairingFailReason, attempts_left: u32| {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::PairingResult {
                reason,
                attempts_left,
            },
        );
    };

    // 读出 pending 快照（释放借用后再改状态）。
    let Some((p_code, p_created, p_owner, p_attempts)) = st.pending.get(target).map(|p| {
        (
            p.code.clone(),
            p.created_at,
            p.controller_device_id.clone(),
            p.attempts_left,
        )
    }) else {
        fail(st, PairingFailReason::NoPending, 0);
        return;
    };
    // 提交者必须是当初发起请求的控制端（取自鉴权 device_id，杜绝抢答/跨用户）。
    // 不命中按「无此配对」回复，避免向无关方泄漏配对存在与否。
    if p_owner != controller_id {
        fail(st, PairingFailReason::NoPending, 0);
        return;
    }
    // 过期（>= TTL 即失效：配对码恰好有效 PAIRING_TTL_SECS 秒，elapsed∈[0,TTL)）。
    if auth::now_secs() - p_created >= PAIRING_TTL_SECS {
        st.pending.remove(target);
        fail(st, PairingFailReason::Expired, 0);
        send_msg(
            &st.peers,
            target,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::Expired,
            },
        );
        return;
    }
    // 码错：递减尝试，归零即作废。
    if p_code != code {
        let left = p_attempts.saturating_sub(1);
        if left == 0 {
            st.pending.remove(target);
            fail(st, PairingFailReason::TooManyAttempts, 0);
            send_msg(
                &st.peers,
                target,
                RemoteS2C::PairingCancelled {
                    reason: DenyReason::TooManyAttempts,
                },
            );
        } else {
            if let Some(p) = st.pending.get_mut(target) {
                p.attempts_left = left;
            }
            fail(st, PairingFailReason::InvalidCode, left);
        }
        return;
    }
    // 码对：复检独占（配对期间可能被插队成为别人会话的一端），原子建会话。
    st.pending.remove(target);
    if st.sessions.contains_key(controller_id) {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::ControlDenied {
                target_device_id: target.to_string(),
                reason: DenyReason::ControllerBusy,
            },
        );
        return;
    }
    if st.sessions.contains_key(target) {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::ControlDenied {
                target_device_id: target.to_string(),
                reason: DenyReason::AlreadyControlled,
            },
        );
        return;
    }
    let controller_name = st.peers.get(controller_id).map(|p| p.name.clone());
    let target_name = st.peers.get(target).map(|p| p.name.clone());
    // 目标在 pending 中即说明在线（断线会清其 pending）；控制端在线（handle 守卫）。
    let (Some(controller_name), Some(target_name)) = (controller_name, target_name) else {
        // 极端：目标恰在此刻断线。撤销，回告控制端。
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::ControlDenied {
                target_device_id: target.to_string(),
                reason: DenyReason::Offline,
            },
        );
        return;
    };
    st.sessions.insert(
        controller_id.to_string(),
        SessionEntry {
            peer: target.to_string(),
        },
    );
    st.sessions.insert(
        target.to_string(),
        SessionEntry {
            peer: controller_id.to_string(),
        },
    );
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::SessionStarted {
            peer_device_id: target.to_string(),
            peer_name: target_name,
            role: Role::Controller,
        },
    );
    send_msg(
        &st.peers,
        target,
        RemoteS2C::SessionStarted {
            peer_device_id: controller_id.to_string(),
            peer_name: controller_name,
            role: Role::Controlled,
        },
    );
}

/// 被控端拒绝当前未决配对（key = 被控端自身 device_id）：清 pending、通知控制端。
fn decline_control(st: &mut HubState, controlled_id: &str) {
    if let Some(p) = st.pending.remove(controlled_id) {
        send_msg(
            &st.peers,
            &p.controller_device_id,
            RemoteS2C::ControlDenied {
                target_device_id: controlled_id.to_string(),
                reason: DenyReason::RejectedByUser,
            },
        );
    }
}

/// 数据面盲转：查发送者所在会话的对端，把不透明帧原样转给对端；无会话则丢弃。
fn relay(st: &HubState, from: &str, value: serde_json::Value) {
    if let Some(entry) = st.sessions.get(from) {
        let peer = entry.peer.clone();
        send_msg(&st.peers, &peer, RemoteS2C::Relay(value));
    } else {
        tracing::debug!("relay 无活跃会话，丢弃（from={from}）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_protocol::remote::RemoteFrame;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    /// 注册一台设备，返回 (conn_id, 该连接的接收端)。
    fn join(hub: &Hub, device_id: &str, user_id: &str, name: &str) -> (u64, UnboundedReceiver<ToClient>) {
        let (tx, rx) = unbounded_channel();
        let cid = hub.register(device_id, user_id.to_string(), name.to_string(), tx);
        (cid, rx)
    }

    /// 取下一条协议消息（非 Close）；无则 panic（测试断言）。
    fn next_msg(rx: &mut UnboundedReceiver<ToClient>) -> RemoteS2C {
        match rx.try_recv() {
            Ok(ToClient::Msg(m)) => *m,
            Ok(ToClient::Close) => panic!("收到 Close，期望消息"),
            Err(e) => panic!("无消息: {e:?}"),
        }
    }

    /// 排空并返回全部已到协议消息。
    fn drain(rx: &mut UnboundedReceiver<ToClient>) -> Vec<RemoteS2C> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ToClient::Msg(m) = ev {
                out.push(*m);
            }
        }
        out
    }

    /// 驱动一次完整成功配对，返回 (controller_rx, controlled_rx)。
    fn pair_ok(
        hub: &Hub,
        c_id: &str,
        c_cid: u64,
        t_id: &str,
        c_rx: &mut UnboundedReceiver<ToClient>,
        t_rx: &mut UnboundedReceiver<ToClient>,
    ) {
        hub.handle(c_id, c_cid, RemoteC2S::RequestControl { target: t_id.into() });
        // 被控端收到 ControlRequested（含配对码）。
        let code = match next_msg(t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            other => panic!("期望 ControlRequested，得 {other:?}"),
        };
        // 控制端收到 PairingNeeded。
        assert!(matches!(next_msg(c_rx), RemoteS2C::PairingNeeded { .. }));
        hub.handle(c_id, c_cid, RemoteC2S::SubmitPairing { target: t_id.into(), code });
        // 双方收到 SessionStarted。
        assert!(matches!(
            next_msg(c_rx),
            RemoteS2C::SessionStarted { role: Role::Controller, .. }
        ));
        assert!(matches!(
            next_msg(t_rx),
            RemoteS2C::SessionStarted { role: Role::Controlled, .. }
        ));
    }

    #[test]
    fn 配对成功并建立双向会话() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "user-1", "控制机");
        let (_t_cid, mut t_rx) = join(&hub, "tgt", "user-1", "被控机");
        pair_ok(&hub, "ctrl", c_cid, "tgt", &mut c_rx, &mut t_rx);
        // SessionStarted 对端信息正确。
        // （pair_ok 已断言角色；此处补一条 relay 验证通路。）
        let frame = RemoteFrame::Echo("ping".into()).to_value().expect("to_value");
        hub.handle("ctrl", c_cid, RemoteC2S::Relay(frame.clone()));
        match next_msg(&mut t_rx) {
            RemoteS2C::Relay(v) => assert_eq!(
                RemoteFrame::from_value(&v).expect("还原帧"),
                RemoteFrame::Echo("ping".into())
            ),
            other => panic!("期望 Relay，得 {other:?}"),
        }
    }

    #[test]
    fn 错误配对码递减尝试且不建会话() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        let (_t, mut t_rx) = join(&hub, "tgt", "u", "t");
        hub.handle("ctrl", c_cid, RemoteC2S::RequestControl { target: "tgt".into() });
        let _code = match next_msg(&mut t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = next_msg(&mut c_rx); // PairingNeeded
        hub.handle("ctrl", c_cid, RemoteC2S::SubmitPairing { target: "tgt".into(), code: "000000000".into() });
        match next_msg(&mut c_rx) {
            RemoteS2C::PairingResult { reason: PairingFailReason::InvalidCode, attempts_left } => {
                assert_eq!(attempts_left, PAIRING_MAX_ATTEMPTS - 1);
            }
            o => panic!("期望 InvalidCode，得 {o:?}"),
        }
    }

    #[test]
    fn 连错超限作废配对() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        let (_t, mut t_rx) = join(&hub, "tgt", "u", "t");
        hub.handle("ctrl", c_cid, RemoteC2S::RequestControl { target: "tgt".into() });
        let real = match next_msg(&mut t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = drain(&mut c_rx);
        // 用一个保证错误的码（与真码不同）。
        let wrong = if real == "111111111" { "222222222" } else { "111111111" };
        for _ in 0..PAIRING_MAX_ATTEMPTS {
            hub.handle("ctrl", c_cid, RemoteC2S::SubmitPairing { target: "tgt".into(), code: wrong.into() });
        }
        let msgs = drain(&mut c_rx);
        assert!(msgs.iter().any(|m| matches!(
            m,
            RemoteS2C::PairingResult { reason: PairingFailReason::TooManyAttempts, .. }
        )));
        // 被控端收到取消。
        assert!(drain(&mut t_rx).iter().any(|m| matches!(
            m,
            RemoteS2C::PairingCancelled { reason: DenyReason::TooManyAttempts }
        )));
    }

    #[test]
    fn 跨账户控制被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "user-A", "c");
        let (_t, _t_rx) = join(&hub, "tgt", "user-B", "t");
        hub.handle("ctrl", c_cid, RemoteC2S::RequestControl { target: "tgt".into() });
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::CrossUser, .. }
        ));
    }

    #[test]
    fn 自控被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        hub.handle("ctrl", c_cid, RemoteC2S::RequestControl { target: "ctrl".into() });
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::SelfControl, .. }
        ));
    }

    #[test]
    fn 离线目标被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        hub.handle("ctrl", c_cid, RemoteC2S::RequestControl { target: "ghost".into() });
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
    }

    #[test]
    fn 目标已被控时第三方被拒() {
        let hub = Hub::new();
        let (c1, mut c1rx) = join(&hub, "ctrl1", "u", "c1");
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        pair_ok(&hub, "ctrl1", c1, "tgt", &mut c1rx, &mut trx);
        let (c2, mut c2rx) = join(&hub, "ctrl2", "u", "c2");
        hub.handle("ctrl2", c2, RemoteC2S::RequestControl { target: "tgt".into() });
        assert!(matches!(
            next_msg(&mut c2rx),
            RemoteS2C::ControlDenied { reason: DenyReason::AlreadyControlled, .. }
        ));
    }

    #[test]
    fn 非发起者无法抢答配对() {
        let hub = Hub::new();
        let (c1, mut c1rx) = join(&hub, "ctrl1", "u", "c1");
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        hub.handle("ctrl1", c1, RemoteC2S::RequestControl { target: "tgt".into() });
        let code = match next_msg(&mut trx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = drain(&mut c1rx);
        // ctrl2 不是发起者，即便知道真码也应被拒（NoPending，不泄漏）。
        let (c2, mut c2rx) = join(&hub, "ctrl2", "u", "c2");
        hub.handle("ctrl2", c2, RemoteC2S::SubmitPairing { target: "tgt".into(), code });
        assert!(matches!(
            next_msg(&mut c2rx),
            RemoteS2C::PairingResult { reason: PairingFailReason::NoPending, .. }
        ));
        // 原配对仍在：ctrl1 用真码仍可成功。
        // （此处不再重复，覆盖点是抢答被挡。）
    }

    #[test]
    fn 被控端拒绝控制请求() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (t, mut trx) = join(&hub, "tgt", "u", "t");
        hub.handle("ctrl", c, RemoteC2S::RequestControl { target: "tgt".into() });
        let _ = next_msg(&mut trx); // ControlRequested
        let _ = next_msg(&mut crx); // PairingNeeded
        hub.handle("tgt", t, RemoteC2S::DeclineControl);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::ControlDenied { reason: DenyReason::RejectedByUser, .. }
        ));
    }

    #[test]
    fn 断线拆会话并通知对端() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (t, mut trx) = join(&hub, "tgt", "u", "t");
        pair_ok(&hub, "ctrl", c, "tgt", &mut crx, &mut trx);
        hub.disconnect("tgt", t);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::SessionEnded { reason: EndReason::PeerDisconnected }
        ));
        // 会话已拆：控制端再 relay 应被丢弃（对端收不到）。
        let frame = RemoteFrame::Echo("x".into()).to_value().expect("to_value");
        hub.handle("ctrl", c, RemoteC2S::Relay(frame));
        assert!(drain(&mut trx).is_empty());
    }

    #[test]
    fn 重复连接驱逐旧连接() {
        let hub = Hub::new();
        let (_c1, mut c1rx) = join(&hub, "dev", "u", "d");
        // 同 device 第二次连接：旧连接应收 Close。
        let (_c2, _c2rx) = join(&hub, "dev", "u", "d");
        let got_close = {
            let mut closed = false;
            while let Ok(ev) = c1rx.try_recv() {
                if matches!(ev, ToClient::Close) {
                    closed = true;
                }
            }
            closed
        };
        assert!(got_close, "旧连接应被驱逐收到 Close");
    }

    #[test]
    fn 旧连接迟到消息被守卫忽略() {
        let hub = Hub::new();
        let (c1, _c1rx) = join(&hub, "dev", "u", "d");
        let (_c2, _c2rx) = join(&hub, "dev", "u", "d"); // 驱逐 c1
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        // 用旧 conn_id 发消息：应被 is_current 守卫忽略，目标收不到请求。
        hub.handle("dev", c1, RemoteC2S::RequestControl { target: "tgt".into() });
        assert!(drain(&mut trx).is_empty());
    }

    #[test]
    fn 控制端已忙不能再控第二台() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (_t1, mut t1rx) = join(&hub, "t1", "u", "t1");
        pair_ok(&hub, "ctrl", c, "t1", &mut crx, &mut t1rx);
        let (_t2, _t2rx) = join(&hub, "t2", "u", "t2");
        hub.handle("ctrl", c, RemoteC2S::RequestControl { target: "t2".into() });
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::ControlDenied { reason: DenyReason::ControllerBusy, .. }
        ));
    }
}
