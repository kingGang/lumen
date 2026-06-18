//! M5.3 终端远程 · part2a：客户端 WebSocket 控制面引擎。
//!
//! 与 `remote.rs`（M5.2 HTTP 心跳/设备列表）同款**后台线程 + channel**范式，不引
//! tokio：一条后台线程用**同步** `tungstenite` 维持到 `lumen-server` 的 WS 长连接，
//! 收发 [`lumen_protocol::remote`] 控制面消息；UI 帧不阻塞，后台有事件时
//! `ctx.request_repaint()` 唤醒。
//!
//! # 线程模型（读超时单线程）
//! `tungstenite` 是阻塞同步 socket，读写都需 `&mut`。后台线程把底层 `TcpStream`
//! 设 [`READ_TIMEOUT`] 读超时，单循环内交替：①排空 UI 投来的出站命令队列并写出
//! ②周期 [`Ping`](lumen_protocol::remote::RemoteC2S::Ping) 保活 ③带超时读一条消息
//! （超时即「暂无消息」，非错误）。连接断开则退避 [`RECONNECT_DELAY`] 重连。出站
//! 命令最坏延迟一个读超时（控制面人工节奏足够；part3 数据面再调小）。
//!
//! # 生命周期（与 `remote.rs` 对称，挂同样的主循环钩子）
//! 登录后 [`RemoteWs::start`]（须已有 token），每帧 [`RemoteWs::poll`] 收取后台
//! 事件并推进 UI 态，登出 [`RemoteWs::stop`]。本模块（part2a）只做**引擎 + 状态
//! 机**；配对弹窗 / 被控横幅 / 设备「连接」入口等 UI 在 part2b 消费这里暴露的态。
//!
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lumen_protocol::remote::{
    DenyReason, EndReason, PairingFailReason, RemoteC2S, RemoteFrame, RemoteS2C, Role,
};
use lumen_term::Terminal;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{ClientRequestBuilder, Message, WebSocket};

use crate::cloud::server_url;

/// 控制端镜像 Terminal 的 scrollback 上限（行）。被控端转发的实时输出滚出可见区后
/// 进镜像的历史，控制端可上滚回看。
const MIRROR_SCROLLBACK: usize = 5000;
/// 镜像 Terminal 创建时的占位尺寸（首个 `Resize` 帧到达前；被控端会立即发实际尺寸）。
const MIRROR_INIT_ROWS: usize = 24;
/// 同上：占位列数。
const MIRROR_INIT_COLS: usize = 80;

/// 读超时：无消息时 `read` 返回，循环转去处理出站/保活/停止（兼顾响应与不空转）。
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// 应用层 Ping 周期（保活 + 刷新服务端 `last_seen`）。
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// 断线后重连退避。
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// 控制端待配对态：已发起请求、正等用户输入被控端展示的配对码。
#[derive(Debug, Clone)]
pub struct PairingPrompt {
    /// 目标（被控端）设备 id（提交 [`RemoteC2S::SubmitPairing`] 时回填）。
    pub target_device_id: String,
    /// 目标设备显示名。
    pub target_name: String,
    /// 上次配对码校验失败原因（首次为 `None`）。
    pub last_error: Option<PairingFailReason>,
    /// 剩余尝试次数（仅在收到 [`RemoteS2C::PairingResult`] 后有意义）。
    pub attempts_left: Option<u32>,
}

/// 被控端来件态：有控制端请求控制本机，醒目展示配对码 + 可拒绝。
#[derive(Debug, Clone)]
pub struct IncomingControl {
    /// 控制端设备显示名。
    pub controller_name: String,
    /// 本机展示给对方转述的 9 位配对码。
    pub pairing_code: String,
}

/// 活跃会话态（控制中 / 被控中）。
#[derive(Debug, Clone)]
pub struct ActiveSession {
    /// 对端设备显示名。
    pub peer_name: String,
    /// 本端角色（[`Role::Controller`] = 控制中；[`Role::Controlled`] = 被控中）。
    pub role: Role,
}

/// 一次性通知（main 循环 [`RemoteWs::take_notices`] 取走 → 弹 toast）。
#[derive(Debug, Clone)]
pub enum Notice {
    /// 控制请求被拒（离线 / 已被控 / 被拒 / 跨用户 / 自控等）。
    ControlDenied(DenyReason),
    /// 被控端的未决配对被取消（控制端撤销 / 超时）。
    PairingCancelled(DenyReason),
    /// 配对码连错作废 / 过期。
    PairingFailed(PairingFailReason),
    /// 会话已建立（本端角色 + 对端设备名）。
    SessionStarted {
        /// 本端角色。
        role: Role,
        /// 对端设备显示名。
        peer: String,
    },
    /// 会话结束（对端离开 / 断线 / 被取代）。
    SessionEnded(EndReason),
}

/// 后台线程 → 主线程事件。
enum WsEvent {
    /// 连接已建立。
    Connected,
    /// 连接断开（含主动停止前的退场）。
    Disconnected,
    /// 收到一条服务端消息。
    Server(Box<RemoteS2C>),
}

/// 客户端远程控制 WS 引擎（主线程持有）。
#[derive(Default)]
pub struct RemoteWs {
    /// 控制端：待输入配对码态（part2b 渲染弹窗）。
    pub pairing: Option<PairingPrompt>,
    /// 被控端：来件配对态（part2b 渲染横幅 + 配对码）。
    pub incoming: Option<IncomingControl>,
    /// 活跃会话态（part2b 渲染「控制中 / 正在被远程控制」横幅）。
    pub session: Option<ActiveSession>,
    /// M5.3 part3a 控制端镜像：被控端整屏状态在本地用无 PTY 的 `Terminal` 复现
    /// （`advance` 喂入被控端转发的 PTY 字节）。仅 `role==Controller` 会话期间存在。
    mirror: Option<Terminal>,
    /// 待消费的一次性通知（main 取走弹 toast）。
    notices: Vec<Notice>,
    /// UI → 后台 出站命令发送端。
    cmd_tx: Option<Sender<RemoteC2S>>,
    /// 后台 → UI 事件接收端。
    evt_rx: Option<Receiver<WsEvent>>,
    /// 停止标志（登出 / Drop 时置位）。
    stop: Option<Arc<AtomicBool>>,
}

impl RemoteWs {
    /// 登录后启动后台 WS 线程（已在跑则先停旧的）。`token` 为账户 JWT。
    pub fn start(&mut self, token: String, ctx: egui::Context) {
        self.stop();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.cmd_tx = Some(cmd_tx);
        self.evt_rx = Some(evt_rx);
        self.stop = Some(stop.clone());
        if let Err(e) = thread::Builder::new()
            .name("lumen-remote-ws".into())
            .spawn(move || worker(&token, &cmd_rx, &evt_tx, &stop, &ctx))
        {
            log::error!("启动远程 WS 线程失败: {e}");
        }
    }

    /// 登出 / 停止：终止后台线程并清空所有远程态。
    pub fn stop(&mut self) {
        if let Some(s) = &self.stop {
            s.store(true, Ordering::SeqCst);
        }
        self.cmd_tx = None;
        self.evt_rx = None;
        self.stop = None;
        self.pairing = None;
        self.incoming = None;
        self.session = None;
        self.mirror = None;
        self.notices.clear();
    }

    /// 是否已登录并在维持连接（后台线程在跑）。
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stop.is_some()
    }

    /// 每帧调用：收取后台事件、推进 UI 态。返回是否有更新（请求重绘用）。
    pub fn poll(&mut self) -> bool {
        let mut events = Vec::new();
        if let Some(rx) = &self.evt_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        let changed = !events.is_empty();
        for ev in events {
            self.apply(ev);
        }
        changed
    }

    /// 取走待消费通知（main 弹 toast 用）。
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// 控制端：发起控制 `target` 设备。
    pub fn request_control(&self, target: String) {
        self.send(RemoteC2S::RequestControl { target });
    }

    /// 控制端：提交当前待配对的配对码。
    pub fn submit_pairing(&self, code: String) {
        if let Some(p) = &self.pairing {
            self.send(RemoteC2S::SubmitPairing {
                target: p.target_device_id.clone(),
                code,
            });
        }
    }

    /// 控制端：取消当前待配对（仅清本地态；服务端 120s 后自动 GC）。
    pub fn cancel_pairing(&mut self) {
        self.pairing = None;
    }

    /// 被控端：拒绝来件控制请求。
    pub fn decline(&mut self) {
        self.send(RemoteC2S::DeclineControl);
        self.incoming = None;
    }

    /// 任一端：结束当前活跃会话。
    pub fn end_session(&mut self) {
        self.send(RemoteC2S::EndSession);
        self.session = None;
        self.mirror = None;
    }

    /// 被控端：转发焦点窗格 PTY 输出字节给控制端（含会话起始的整屏快照重放）。
    pub fn send_output(&self, bytes: &[u8]) {
        self.send_frame(&RemoteFrame::Output(bytes.to_vec()));
    }

    /// 被控端：告知控制端镜像终端尺寸（会话起始 + 窗格 resize 时发；须在对应
    /// 尺寸的 `Output` 之前）。
    pub fn send_resize(&self, rows: u16, cols: u16) {
        self.send_frame(&RemoteFrame::Resize { rows, cols });
    }

    /// 控制端：当前镜像 Terminal（`role==Controller` 会话期间 Some），供渲染读取。
    #[must_use]
    pub fn mirror(&self) -> Option<&Terminal> {
        self.mirror.as_ref()
    }

    /// 把数据面帧序列化为不透明 `Relay` 投递（序列化失败仅记日志、不断连）。
    fn send_frame(&self, frame: &RemoteFrame) {
        match frame.to_value() {
            Ok(v) => self.send(RemoteC2S::Relay(v)),
            Err(e) => log::error!("远程数据面帧序列化失败: {e}"),
        }
    }

    /// 投递一条出站命令（未连接则静默丢弃）。
    fn send(&self, msg: RemoteC2S) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(msg);
        }
    }

    /// 控制端：把收到的数据面帧作用到镜像 Terminal（仅控制会话期间）。
    fn apply_mirror(&mut self, value: &serde_json::Value) {
        let Some(mirror) = self.mirror.as_mut() else {
            return; // 非控制端会话：无镜像，丢弃（part3 只单向被控端→控制端）。
        };
        let Ok(frame) = RemoteFrame::from_value(value) else {
            log::debug!("镜像帧解析失败（可能是更高版本对端的未知变体），丢弃");
            return;
        };
        match frame {
            RemoteFrame::Resize { rows, cols } => {
                mirror.resize(usize::from(rows).max(1), usize::from(cols).max(1));
            }
            RemoteFrame::Output(bytes) => {
                mirror.advance(&bytes);
                // 镜像无 PTY，不回写应答（DSR/DA 等）；排空避免无界增长。
                let _ = mirror.take_responses();
            }
            RemoteFrame::Echo(_) => {}
        }
    }

    /// 处理一条后台事件。
    fn apply(&mut self, ev: WsEvent) {
        match ev {
            // 连接建立：presence 上线（后台线程已处理重连，主线程无需记状态）。
            WsEvent::Connected => {}
            WsEvent::Disconnected => {
                // 断线即丢弃进行中的配对/会话态（服务端侧亦已拆除）。
                self.pairing = None;
                self.incoming = None;
                self.mirror = None;
                if self.session.take().is_some() {
                    self.notices.push(Notice::SessionEnded(EndReason::PeerDisconnected));
                }
            }
            WsEvent::Server(msg) => self.apply_server(*msg),
        }
    }

    /// 处理一条服务端协议消息，推进配对/会话状态机。
    fn apply_server(&mut self, msg: RemoteS2C) {
        match msg {
            RemoteS2C::Welcome { .. } | RemoteS2C::Pong => {}
            RemoteS2C::ControlRequested {
                controller_name,
                pairing_code,
                ..
            } => {
                self.incoming = Some(IncomingControl {
                    controller_name,
                    pairing_code,
                });
            }
            RemoteS2C::PairingNeeded {
                target_device_id,
                target_name,
                ..
            } => {
                self.pairing = Some(PairingPrompt {
                    target_device_id,
                    target_name,
                    last_error: None,
                    attempts_left: None,
                });
            }
            RemoteS2C::PairingResult {
                reason,
                attempts_left,
            } => {
                if attempts_left == 0 {
                    self.pairing = None;
                    self.notices.push(Notice::PairingFailed(reason));
                } else if let Some(p) = &mut self.pairing {
                    p.last_error = Some(reason);
                    p.attempts_left = Some(attempts_left);
                }
            }
            RemoteS2C::ControlDenied { reason, .. } => {
                self.pairing = None;
                self.notices.push(Notice::ControlDenied(reason));
            }
            RemoteS2C::PairingCancelled { reason } => {
                self.incoming = None;
                self.notices.push(Notice::PairingCancelled(reason));
            }
            RemoteS2C::SessionStarted {
                peer_name, role, ..
            } => {
                self.pairing = None;
                self.incoming = None;
                let peer = peer_name.clone();
                // 控制端：起一个无 PTY 的镜像 Terminal（被控端会随即发 Resize+快照）。
                self.mirror = (role == Role::Controller)
                    .then(|| Terminal::new(MIRROR_INIT_ROWS, MIRROR_INIT_COLS, MIRROR_SCROLLBACK));
                self.session = Some(ActiveSession { peer_name, role });
                self.notices.push(Notice::SessionStarted { role, peer });
            }
            RemoteS2C::SessionEnded { reason } => {
                self.session = None;
                self.mirror = None;
                self.notices.push(Notice::SessionEnded(reason));
            }
            // 数据面（part3a：被控端整屏状态字节流）→ 作用到镜像 Terminal。
            RemoteS2C::Relay(value) => self.apply_mirror(&value),
        }
    }
}

/// 后台线程主体：连接 → 跑读写循环 → 断线退避重连，直到 `stop`。
fn worker(
    token: &str,
    cmd_rx: &Receiver<RemoteC2S>,
    evt_tx: &Sender<WsEvent>,
    stop: &Arc<AtomicBool>,
    ctx: &egui::Context,
) {
    while !stop.load(Ordering::SeqCst) {
        match connect_ws(token) {
            Ok(mut socket) => {
                let _ = evt_tx.send(WsEvent::Connected);
                ctx.request_repaint();
                run_connection(&mut socket, cmd_rx, evt_tx, stop, ctx);
                let _ = evt_tx.send(WsEvent::Disconnected);
                ctx.request_repaint();
            }
            Err(e) => log::warn!("远程 WS 连接失败: {e}"),
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        sleep_with_stop(RECONNECT_DELAY, stop);
    }
}

/// 单条连接的读写循环：排空出站命令 + 周期 Ping + 带超时读消息。返回即断开。
fn run_connection(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    cmd_rx: &Receiver<RemoteC2S>,
    evt_tx: &Sender<WsEvent>,
    stop: &Arc<AtomicBool>,
    ctx: &egui::Context,
) {
    let mut last_ping = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            return;
        }
        // 1. 排空 UI 出站命令。
        loop {
            match cmd_rx.try_recv() {
                Ok(msg) => {
                    if !write_msg(socket, &msg) {
                        return; // 写失败=断开
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return, // 主线程已 stop
            }
        }
        // 2. 周期保活。
        if last_ping.elapsed() >= PING_INTERVAL {
            if !write_msg(socket, &RemoteC2S::Ping) {
                return;
            }
            last_ping = Instant::now();
        }
        // 3. 带超时读一条消息。
        match socket.read() {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<RemoteS2C>(text.as_str()) {
                    Ok(msg) => {
                        let _ = evt_tx.send(WsEvent::Server(Box::new(msg)));
                        ctx.request_repaint();
                    }
                    Err(e) => log::debug!("远程 WS 消息解析失败: {e}"),
                }
            }
            Ok(Message::Close(_)) => return,
            // 二进制 / Ping / Pong / 原始帧：part1 不使用，忽略。
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // 读超时：本轮无消息，继续循环（处理出站/保活/停止）。
            }
            Err(e) => {
                log::debug!("远程 WS 读断开: {e}");
                return;
            }
        }
    }
}

/// 序列化并发送一条出站消息；成功返回 `true`，写失败（断开）返回 `false`。
fn write_msg(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, msg: &RemoteC2S) -> bool {
    let Ok(text) = serde_json::to_string(msg) else {
        log::error!("远程 WS 出站消息序列化失败");
        return true; // 序列化错误不该断连接，丢弃该条
    };
    socket.send(Message::Text(text.into())).is_ok()
}

/// 建立到 `lumen-server` 的 WS 连接（带 `Authorization: Bearer` 头），并对底层
/// `TcpStream` 设读超时。
fn connect_ws(token: &str) -> anyhow::Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let url = ws_url(&server_url());
    let uri: tungstenite::http::Uri = url.parse()?;
    let req = ClientRequestBuilder::new(uri).with_header("Authorization", format!("Bearer {token}"));
    let (mut socket, _resp) = tungstenite::connect(req)?;
    set_read_timeout(socket.get_mut(), Some(READ_TIMEOUT));
    Ok(socket)
}

/// 把 HTTP(S) 基址转成 WS(S) URL 并拼上远程控制路径。
fn ws_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let scheme_swapped = if let Some(rest) = b.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = b.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{b}")
    };
    format!("{scheme_swapped}{}", lumen_protocol::routes::WS)
}

/// 给底层 `TcpStream` 设读超时（明文 / rustls 两种流均覆盖）。
fn set_read_timeout(stream: &mut MaybeTlsStream<TcpStream>, dur: Option<Duration>) {
    match stream {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(dur);
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(dur);
        }
        // MaybeTlsStream 是 #[non_exhaustive]：未启用的 TLS 后端等忽略。
        _ => {}
    }
}

/// 可被 `stop` 提前打断的睡眠（重连退避用，避免登出后还干等）。
fn sleep_with_stop(total: Duration, stop: &Arc<AtomicBool>) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_转换() {
        assert_eq!(ws_url("http://127.0.0.1:8787"), "ws://127.0.0.1:8787/api/v1/ws");
        assert_eq!(ws_url("https://lumen.example.com"), "wss://lumen.example.com/api/v1/ws");
        // 缺协议默认 ws://；去尾斜杠。
        assert_eq!(ws_url("192.168.1.85:8787/"), "ws://192.168.1.85:8787/api/v1/ws");
    }

    #[test]
    fn 默认态与停止() {
        let mut ws = RemoteWs::default();
        assert!(!ws.is_running());
        assert!(ws.pairing.is_none() && ws.incoming.is_none() && ws.session.is_none());
        // stop 在未启动时应安全（幂等）。
        ws.stop();
        assert!(!ws.is_running());
    }

    #[test]
    fn 配对结果推进状态机() {
        let mut ws = RemoteWs::default();
        // 模拟收到 PairingNeeded → 进入待配对态。
        ws.apply_server(RemoteS2C::PairingNeeded {
            target_device_id: "t".into(),
            target_name: "被控机".into(),
            expires_in_secs: 120,
        });
        assert!(ws.pairing.is_some());
        // 错码：剩余次数下降、记录错误，仍保留待配对。
        ws.apply_server(RemoteS2C::PairingResult {
            reason: PairingFailReason::InvalidCode,
            attempts_left: 4,
        });
        let p = ws.pairing.as_ref().expect("仍待配对");
        assert_eq!(p.attempts_left, Some(4));
        assert!(matches!(p.last_error, Some(PairingFailReason::InvalidCode)));
        // 归零：配对作废 + 通知。
        ws.apply_server(RemoteS2C::PairingResult {
            reason: PairingFailReason::TooManyAttempts,
            attempts_left: 0,
        });
        assert!(ws.pairing.is_none());
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::PairingFailed(PairingFailReason::TooManyAttempts)]
        ));
    }

    #[test]
    fn 会话建立与结束() {
        let mut ws = RemoteWs::default();
        ws.apply_server(RemoteS2C::SessionStarted {
            peer_device_id: "p".into(),
            peer_name: "对端".into(),
            role: Role::Controller,
        });
        let s = ws.session.as_ref().expect("会话已建立");
        assert_eq!(s.role, Role::Controller);
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::SessionStarted { role: Role::Controller, .. }]
        ));
        ws.apply_server(RemoteS2C::SessionEnded {
            reason: EndReason::PeerLeft,
        });
        assert!(ws.session.is_none());
        assert!(matches!(
            ws.take_notices().as_slice(),
            [Notice::SessionEnded(EndReason::PeerLeft)]
        ));
    }

    #[test]
    fn 来件控制与断线清理() {
        let mut ws = RemoteWs::default();
        ws.apply_server(RemoteS2C::ControlRequested {
            controller_device_id: "c".into(),
            controller_name: "控制机".into(),
            pairing_code: "123456789".into(),
            expires_in_secs: 120,
        });
        assert_eq!(
            ws.incoming.as_ref().map(|i| i.pairing_code.clone()),
            Some("123456789".to_string())
        );
        // 断线：来件/会话态清掉。
        ws.apply(WsEvent::Disconnected);
        assert!(ws.incoming.is_none());
    }
}
