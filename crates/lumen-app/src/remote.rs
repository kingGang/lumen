//! 远程设备状态（M5.2「设备在线」）。
//!
//! 登录后后台线程定期**心跳**（保持本设备在线）+ 拉**设备列表**，结果经
//! channel 回传主线程缓存（[`RemoteState::poll`]）；改名/删除走一次性线程后
//! 请求刷新。登出 [`RemoteState::stop`] 终止线程。与 `login_ui` 同款封装：
//! 网络全在后台线程，UI 帧不阻塞，后台完成时 `ctx.request_repaint()` 唤醒。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_protocol::DeviceRecord;
use winit::event_loop::EventLoopProxy;

use crate::cloud::{server_url, CloudClient};
use crate::PtyWake;

/// 心跳 + 列表轮询周期。10s：上下线在列表里更及时反映（轻量 HTTP，几台设备开销可忽略）；
/// 须 < 服务端在线窗口 `LUMEN_ONLINE_WINDOW_SECS`（默认 45s ≈ 4 次心跳容差），免误判离线。
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// 后台线程 → 主线程的消息。
enum Event {
    /// 设备列表刷新成功。
    Devices(Vec<DeviceRecord>),
    /// 拉取失败（用户可见原因）。
    Error(String),
}

/// 远程设备状态（主线程持有）。
#[derive(Default)]
pub struct RemoteState {
    /// 最近一次拉到的设备列表（服务端已按 `last_seen` 倒序）。
    pub devices: Vec<DeviceRecord>,
    /// 最近一次拉取错误（展示用）。
    pub last_error: Option<String>,
    /// 当前选中的设备 id（高亮用；M5.3 连接用）。
    pub active_device_id: Option<String>,
    /// 当前账户 token（改名/删除一次性请求用）。
    token: Option<String>,
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
        token: String,
        ctx: egui::Context,
        proxy: EventLoopProxy<PtyWake>,
        wake_pending: Arc<AtomicBool>,
    ) {
        self.stop();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(true)); // 启动即刷一次
        self.token = Some(token.clone());
        self.rx = Some(rx);
        self.stop = Some(stop.clone());
        self.refresh = Some(refresh.clone());
        std::thread::spawn(move || worker(&token, &tx, &stop, &refresh, &ctx, &proxy, &wake_pending));
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
        self.active_device_id = None;
    }

    /// 是否已在运行（登录态）。
    pub fn is_running(&self) -> bool {
        self.stop.is_some()
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
                        updated = true;
                    }
                    Event::Error(e) => {
                        self.last_error = Some(e);
                        updated = true;
                    }
                }
            }
        }
        updated
    }

    /// 改名设备（一次性后台请求 + 请求刷新）。
    pub fn rename_device(&self, id: String, name: String) {
        let Some(token) = self.token.clone() else {
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
        let Some(token) = self.token.clone() else {
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
fn worker(
    token: &str,
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
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();
        let due = refresh.swap(false, Ordering::SeqCst) || now.duration_since(last) >= POLL_INTERVAL;
        if due {
            last = now;
            let _ = client.heartbeat(token);
            match client.list_devices(token) {
                Ok(resp) => {
                    if tx.send(Event::Devices(resp.devices)).is_err() {
                        break; // 接收端已丢弃（登出）
                    }
                }
                Err(e) => {
                    let _ = tx.send(Event::Error(e.user_message()));
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
