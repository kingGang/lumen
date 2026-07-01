//! 远程设备状态（M5.2「设备在线」）。
//!
//! 登录后后台线程定期**心跳**（保持本设备在线）+ 拉**设备列表**，结果经
//! channel 回传主线程缓存（[`RemoteState::poll`]）；改名/删除走一次性线程后
//! 请求刷新。登出 [`RemoteState::stop`] 终止线程。与 `login_ui` 同款封装：
//! 网络全在后台线程，UI 帧不阻塞，后台完成时 `ctx.request_repaint()` 唤醒。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lumen_protocol::DeviceRecord;
use winit::event_loop::EventLoopProxy;

use crate::cloud::{server_url, CloudClient, CloudError};
use crate::PtyWake;

/// 心跳 + 列表轮询周期。10s：上下线在列表里更及时反映（轻量 HTTP，几台设备开销可忽略）；
/// 须 < 服务端在线窗口 `LUMEN_ONLINE_WINDOW_SECS`（默认 45s ≈ 4 次心跳容差），免误判离线。
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// token 续期提前量：剩余有效期 < 此值即自动续新（治本：免 7 天到期后全面 401 掉线）。2 天 ≈ 只要
/// 客户端每 5 天内用过一次就永不过期；过期后无法续（需重登），由登录 UI 提示兜底。
const TOKEN_REFRESH_AHEAD_SECS: i64 = 2 * 24 * 3600;
/// 续期尝试最小间隔：失败时不每 10s 高频重试，按此节流（窗口内通常一次成功）。
const TOKEN_REFRESH_RETRY: Duration = Duration::from_secs(60);

/// 当前 Unix 秒（系统时钟早于 1970 记 0；与服务端 `expires_at` 同基准比较）。
fn now_secs() -> i64 {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    i64::try_from(s).unwrap_or(i64::MAX)
}

/// 读取共享 token（poison 兜底空串——只损当次请求、不 panic）。
fn read_token(token: &Arc<RwLock<String>>) -> String {
    token.read().map(|g| g.clone()).unwrap_or_default()
}

/// 是否处于 token 续期窗口：有到期时间(`>0`)、**未过期**(`now < expires_at`)、且剩余 < 提前量。
/// 已过期不续（换不了，需重登）；无到期(0)不续。
fn in_refresh_window(now_secs: i64, expires_at: i64) -> bool {
    expires_at > 0 && now_secs < expires_at && now_secs + TOKEN_REFRESH_AHEAD_SECS >= expires_at
}

/// 与云服务器的连接态（底部状态栏「已连接 / 未连接 / 连接错误」指示用）。
/// 仅在登录后（心跳 worker 运行时）有意义；未登录场景由 [`RemoteState::is_running`]
/// 判定（那时统一显示「未连接」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerConn {
    /// 未确认连上真服务器：心跳线程刚起未拿到首次结果（连接中），或服务器有响应但非正常
    /// list_devices（401/5xx/非 Lumen 响应）——UI 统一显示黄「未连接服务器」。
    #[default]
    Connecting,
    /// 已连上真 Lumen 服务器且账户有效（list_devices 成功）——UI 显示绿「已连接服务器」。
    Connected,
    /// 最近一次请求网络层失败（连不上服务器）。
    Error,
}

/// 后台线程 → 主线程的消息。
enum Event {
    /// 设备列表刷新成功。
    Devices(Vec<DeviceRecord>),
    /// 拉取失败。`network=true` 表示网络层连不上（红「连接错误」）；`false` 表示
    /// 服务器有响应但业务错误（如 401，服务器仍可达，不算连接错误）。
    Error { message: String, network: bool },
    /// token 已自动续期：主线程据此落 profile（持久化新 token + 到期时间）。
    TokenRefreshed { token: String, expires_at: i64 },
}

/// 远程设备状态（主线程持有）。
#[derive(Default)]
pub struct RemoteState {
    /// 最近一次拉到的设备列表（服务端已按 `last_seen` 倒序）。
    pub devices: Vec<DeviceRecord>,
    /// 最近一次拉取错误（展示用）。
    pub last_error: Option<String>,
    /// 与服务器的连接态（状态栏指示；仅登录后有意义，由心跳结果驱动）。
    conn: ServerConn,
    /// 当前选中的设备 id（高亮用；M5.3 连接用）。
    pub active_device_id: Option<String>,
    /// 当前账户 token（**共享可变**：心跳 worker 自动续期时写回，改名/删除一次性请求 + WS 重连
    /// 共读同一份，确保续期后处处用新 token）。
    token: Option<Arc<RwLock<String>>>,
    /// 自动续期得到的新 token（main 每帧 [`Self::take_refreshed_token`] 取走落 profile）。
    pending_token: Option<(String, i64)>,
    rx: Option<Receiver<Event>>,
    stop: Option<Arc<AtomicBool>>,
    refresh: Option<Arc<AtomicBool>>,
}

impl RemoteState {
    /// 登录后启动后台心跳 + 轮询线程（已在跑则先停旧的）。`proxy`/`wake_pending` 用于**唤醒空闲的
    /// winit 事件循环**（`ControlFlow::Wait` 下 `ctx.request_repaint()` 单独叫不醒空闲循环——必须经
    /// `PtyWake`，否则停在远程设备视图时 30s 轮询拉到新列表也不重绘、在线状态不刷新，海风哥实测踩坑）。
    pub fn start(
        &mut self,
        token: Arc<RwLock<String>>,
        token_expires_at: i64,
        ctx: egui::Context,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    ) {
        self.stop();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(true)); // 启动即刷一次
        self.token = Some(Arc::clone(&token));
        self.rx = Some(rx);
        self.stop = Some(stop.clone());
        self.refresh = Some(refresh.clone());
        std::thread::spawn(move || {
            worker(
                &token,
                token_expires_at,
                &tx,
                &stop,
                &refresh,
                &ctx,
                &proxy,
                &wake_pending,
            );
        });
    }

    /// 登出：停止后台线程、清空缓存。
    pub fn stop(&mut self) {
        if let Some(s) = &self.stop {
            s.store(true, Ordering::SeqCst);
        }
        self.token = None;
        self.rx = None;
        self.stop = None;
        self.refresh = None;
        self.devices.clear();
        self.last_error = None;
        self.conn = ServerConn::Connecting;
        self.active_device_id = None;
    }

    /// 是否已在运行（登录态）。
    pub fn is_running(&self) -> bool {
        self.stop.is_some()
    }

    /// 与服务器的连接态（状态栏指示用；仅在 [`Self::is_running`] 为真时有意义）。
    pub fn server_conn(&self) -> ServerConn {
        self.conn
    }

    /// 请求后台线程尽快刷新一次（进入远程 tab / 改名删除后调用）。
    pub fn request_refresh(&self) {
        if let Some(r) = &self.refresh {
            r.store(true, Ordering::SeqCst);
        }
    }

    /// 主循环每帧调用：收取后台事件、更新缓存。返回是否有更新（用于请求重绘）。
    pub fn poll(&mut self) -> bool {
        let mut updated = false;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    Event::Devices(d) => {
                        self.devices = d;
                        self.last_error = None;
                        self.conn = ServerConn::Connected;
                        updated = true;
                    }
                    Event::Error { message, network } => {
                        self.last_error = Some(message);
                        // 网络层失败=连不上→红。其余（HTTP 有响应但非正常 list_devices：
                        // 401 token 失效/被吊销、5xx、captive portal / 非 Lumen 服务器致解析
                        // 失败）→未确认连上真服务器，退回 Connecting（黄「未连接」），绝不误报绿。
                        // 只有 list_devices 成功（Devices 分支）才置 Connected（绿）——那是「连到
                        // 真 Lumen 且账户有效」的唯一可信证据。
                        self.conn = if network {
                            ServerConn::Error
                        } else {
                            ServerConn::Connecting
                        };
                        updated = true;
                    }
                    Event::TokenRefreshed { token, expires_at } => {
                        // 共享句柄已被 worker 写新值（WS/REST 即刻用新 token）；此处仅记下供 main 落 profile。
                        self.pending_token = Some((token, expires_at));
                        updated = true;
                    }
                }
            }
        }
        updated
    }

    /// main 每帧取走自动续期得到的新 token（落 profile 持久化）；无续期返回 `None`。
    pub fn take_refreshed_token(&mut self) -> Option<(String, i64)> {
        self.pending_token.take()
    }

    /// 改名设备（一次性后台请求 + 请求刷新）。
    pub fn rename_device(&self, id: String, name: String) {
        let Some(token) = self.token.as_ref().map(read_token) else {
            return;
        };
        std::thread::spawn(move || {
            let client = CloudClient::new(server_url());
            if let Err(e) = client.rename_device(&token, &id, &name) {
                log::warn!("远程设备改名失败: {}", e.user_message());
            }
        });
        self.request_refresh();
    }

    /// 删除设备（一次性后台请求 + 请求刷新）。
    pub fn delete_device(&mut self, id: String) {
        let Some(token) = self.token.as_ref().map(read_token) else {
            return;
        };
        if self.active_device_id.as_deref() == Some(id.as_str()) {
            self.active_device_id = None;
        }
        std::thread::spawn(move || {
            let client = CloudClient::new(server_url());
            if let Err(e) = client.delete_device(&token, &id) {
                log::warn!("远程设备删除失败: {}", e.user_message());
            }
        });
        self.request_refresh();
    }
}

/// 后台线程主体：到点（或被请求）就心跳 + 拉列表，回传并唤醒 UI。
#[allow(clippy::too_many_arguments)] // 与主线程共享的一组句柄/通道，拆结构体反更晦涩。
fn worker(
    token: &Arc<RwLock<String>>,
    mut expires_at: i64,
    tx: &Sender<Event>,
    stop: &Arc<AtomicBool>,
    refresh: &Arc<AtomicBool>,
    ctx: &egui::Context,
    proxy: &EventLoopProxy<PtyWake>,
    wake_pending: &Arc<AtomicBool>,
) {
    let client = CloudClient::new(server_url());
    let mut last = Instant::now()
        .checked_sub(POLL_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_refresh_try = Instant::now()
        .checked_sub(TOKEN_REFRESH_RETRY)
        .unwrap_or_else(Instant::now);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();

        // 自动续期（治本）：token **未过期**但已进入「剩余 < 提前量」窗口、且距上次尝试 ≥ 节流间隔时，
        // 用现有 token 换发新 token。成功即写回共享句柄（WS 重连 / REST 随后即用新 token）+ 报 main 落
        // profile 持久化。已过期则换不了（服务端 401），保持原样、待用户重登（登录 UI 提示兜底）。
        let nows = now_secs();
        if in_refresh_window(nows, expires_at)
            && now.duration_since(last_refresh_try) >= TOKEN_REFRESH_RETRY
        {
            last_refresh_try = now;
            let cur = read_token(token);
            if let Ok(resp) = client.refresh_token(&cur) {
                if let Ok(mut g) = token.write() {
                    *g = resp.token.clone();
                }
                expires_at = resp.expires_at;
                let _ = tx.send(Event::TokenRefreshed {
                    token: resp.token,
                    expires_at: resp.expires_at,
                });
                nudge(ctx, proxy, wake_pending);
            }
        }

        let due = refresh.swap(false, Ordering::SeqCst) || now.duration_since(last) >= POLL_INTERVAL;
        if due {
            last = now;
            let tok = read_token(token); // 共享句柄当前值（含已续期的新 token）。
            let _ = client.heartbeat(&tok);
            match client.list_devices(&tok) {
                Ok(resp) => {
                    if tx.send(Event::Devices(resp.devices)).is_err() {
                        break; // 接收端已丢弃（登出）
                    }
                }
                Err(e) => {
                    let network = matches!(e, CloudError::Network(_));
                    let _ = tx.send(Event::Error {
                        message: e.user_message(),
                        network,
                    });
                }
            }
            nudge(ctx, proxy, wake_pending);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 唤醒主线程重绘 + **唤醒空闲的 winit 事件循环**（`request_repaint` 在 `ControlFlow::Wait` 下不叫醒
/// 空闲循环，必须发 `PtyWake`）。`wake_pending` 与主线程清标志配对去重（同 remote_ws / p2p 的 nudge）。
fn nudge(ctx: &egui::Context, proxy: &EventLoopProxy<PtyWake>, wake_pending: &Arc<AtomicBool>) {
    ctx.request_repaint();
    if !wake_pending.swap(true, Ordering::SeqCst) {
        let _ = proxy.send_event(PtyWake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 续期窗口判定() {
        let exp = 1_000_000_i64;
        // 无到期时间(0)：不续。
        assert!(!in_refresh_window(999_999, 0));
        // 远未到期（剩余 > 提前量）：不续。
        assert!(!in_refresh_window(exp - TOKEN_REFRESH_AHEAD_SECS - 1, exp));
        // 进入窗口（剩余恰 = 提前量）：续。
        assert!(in_refresh_window(exp - TOKEN_REFRESH_AHEAD_SECS, exp));
        // 窗口内（剩余 < 提前量、未过期）：续。
        assert!(in_refresh_window(exp - 10, exp));
        // 恰到期(now == expires_at)：不续。
        assert!(!in_refresh_window(exp, exp));
        // 已过期：不续（换不了，需重登）。
        assert!(!in_refresh_window(exp + 100, exp));
    }

    #[test]
    fn 读共享token() {
        let t = Arc::new(RwLock::new("abc".to_string()));
        assert_eq!(read_token(&t), "abc");
        if let Ok(mut g) = t.write() {
            *g = "xyz".into();
        }
        assert_eq!(read_token(&t), "xyz");
    }
}
