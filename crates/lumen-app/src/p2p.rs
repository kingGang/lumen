//! M6 P2P 直连：QUIC 打洞 + mTLS 握手 + 数据面直连（含中继回退）。
//!
//! 设计见 `docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md`。本模块是「QUIC 打洞 + 中继回退」的
//! **客户端传输引擎**，与主线程的同步 tungstenite（`remote_ws.rs`）范式**隔离**：一条 P2P 后台
//! 线程内起 **current-thread tokio runtime** 驱动 quinn / STUN（tokio 关在线程内，主线程零感知）。
//!
//! # 线程模型（与 `remote_ws.rs` 对称：后台线程 + channel）
//! - 主线程 → P2P 线程：`P2pCmd`（对端信令）/ `data_tx`（出站数据帧），均 tokio unbounded、主线程同步调。
//! - P2P 线程 → 主线程：`P2pEvent`（std mpsc，每帧 [`P2pEngine::poll`] 排空）；收到帧/事件后 [`Waker`]
//!   nudge 唤醒主线程重绘。
//! - `data_ready: Arc<AtomicBool>`：数据面就绪标志（流建立时置位，主线程 `send_frame` 选路读）。
//!
//! # 流程（Phase 0–3 已落地）
//! ① STUN 端点发现（RFC 5389）+ 本地 LAN 候选；② 经信令通道交换候选 + 自签证书（`SignalPayload`）；
//! ③ role 定向打洞（Controller connect / Controlled accept，收敛同一连接）+ mTLS pinned 双向认证；
//! ④ 单条双向 QUIC stream 承载**输出方向**数据面（length-prefix JSON，复用 `RemoteFrame::to_value`），
//! 链路断自动回退中继。输入方向恒走中继（防切换乱序，见 `remote_ws::send_frame`）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumen_protocol::remote::{P2pSignalKind, RemoteFrame, Role};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use winit::event_loop::EventLoopProxy;

use crate::PtyWake;

/// 唤醒主线程重绘的句柄（与 remote_ws 的 WS 线程同款 nudge）。**P2P 数据帧/事件投递后必须 nudge**：
/// Lumen UI 按需重绘，漏唤醒则 `DataFrame` 躺在 channel 里、要等下次偶发重绘才被 `poll` 出来处理，
/// 表现为回显/目录树/快照延迟数秒~十几秒（Phase 3 实测踩坑）。
#[derive(Clone)]
struct Waker {
    ctx: egui::Context,
    proxy: EventLoopProxy<PtyWake>,
    pending: Arc<AtomicBool>,
}

impl Waker {
    fn nudge(&self) {
        self.ctx.request_repaint();
        // SeqCst 与主线程清标志配对（同 remote_ws::nudge），避免丢唤醒。
        if !self.pending.swap(true, Ordering::SeqCst) {
            let _ = self.proxy.send_event(PtyWake);
        }
    }
}

/// 发一条事件给主线程并 nudge 唤醒（漏 nudge 会让按需重绘的 UI 延迟处理，见 [`Waker`]）。
fn emit(evt_tx: &Sender<P2pEvent>, waker: &Option<Waker>, ev: P2pEvent) {
    if evt_tx.send(ev).is_ok() {
        if let Some(w) = waker {
            w.nudge();
        }
    }
}

/// QUIC ALPN 协议标识（两端必须一致；隔离非 lumen 的 QUIC 流量）。
const ALPN: &[u8] = b"lumen-p2p";
/// connect 时的 server name（pinned 验证器忽略此名、只认证书，故用固定占位 DNS 名）。
const SERVER_NAME: &str = "lumen-p2p";
/// 单次打洞握手总超时（所有候选 + accept 竞速；超时即视作打洞失败、回退中继）。
const PUNCH_TIMEOUT: Duration = Duration::from_secs(5);
/// 数据面单帧字节上限（length-prefix 读取前的缓冲上限，防损坏/恶意长度导致 OOM）。**与中继 WS 的
/// 4 MiB 帧上限对齐**——同一帧两条路（QUIC 直连 / 中继回退）大小判定一致，避免回退瞬间超限帧击穿 WS。
/// 常规数据面帧远不及此（OutputWithId ≤8 KiB、FileChunk 256 KiB、递归列目录 <4 MiB 有单测保证）。
const MAX_DATA_FRAME: usize = 4 * 1024 * 1024;
/// 直连 QUIC keep-alive 间隔（< idle 超时；防终端会话空闲时被 idle 静默关连误触发回退）。
const P2P_KEEPALIVE: Duration = Duration::from_secs(4);
/// 直连 QUIC 最大空闲超时（超过即判链路丢失，触发回退中继）。
const P2P_MAX_IDLE: Duration = Duration::from_secs(30);

/// STUN 单次探测超时（无应答即视作该服务器不可达，回退/换源）。
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

/// RFC 5389 magic cookie（固定常量，区分 STUN 与其他 UDP 流量、参与 XOR 编码）。
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// 主线程 → P2P 线程的命令。
#[derive(Debug)]
enum P2pCmd {
    /// 对端经信令通道（[`super::RemoteFrame::P2pSignal`]）发来的信令，喂给打洞状态机。
    PeerSignal {
        /// 信令阶段。
        kind: P2pSignalKind,
        /// 阶段负载（候选 + 证书 + nonce）。
        payload: SignalPayload,
    },
    /// 优雅停机（亦可经 drop `cmd_tx` 触发 `recv()==None`）。
    Stop,
}

/// P2P 线程 → 主线程的事件（主线程 [`P2pEngine::poll`] 排空）。
#[derive(Debug, Clone)]
pub enum P2pEvent {
    /// 引擎请求经信令通道把一条 P2pSignal 发给对端（main 转 `send_frame(P2pSignal)`）。
    SendSignal {
        /// 信令阶段。
        kind: P2pSignalKind,
        /// 阶段负载。
        payload: SignalPayload,
    },
    /// QUIC 连接已建立（信息性日志；数据面是否就绪另看 [`Self::DataPlaneUp`]）。
    Connected,
    /// **数据面就绪**（收到对端 Ready 信令、双向切换点对齐）：主线程此后出站走 QUIC，并补发一次
    /// 订阅触发整屏快照重建（消除切换瞬间的 VT 错位/丢失）。
    DataPlaneUp,
    /// **数据面失效**（QUIC 链路断 / idle 超时 / 流关）：主线程回退中继 + 补发订阅重建 + UI 标识。
    DataPlaneDown,
    /// 一条经 QUIC 直连收到的数据面帧（主线程喂回 `apply_relay`，与中继帧汇入同一状态机）。
    DataFrame(serde_json::Value),
}

/// P2P 直连引擎句柄（主线程持有；与 `RemoteWs` 对称的启停 + poll 生命周期）。
pub struct P2pEngine {
    /// 主线程 → P2P 线程命令端。
    cmd_tx: UnboundedSender<P2pCmd>,
    /// 主线程 → P2P 线程**数据面**出站端（`send_frame` 选路到 QUIC 时投这里，写泵经 stream 发出）。
    data_tx: UnboundedSender<serde_json::Value>,
    /// P2P 线程 → 主线程事件端。
    evt_rx: Receiver<P2pEvent>,
    /// **数据面就绪标志**（数据面流真正建立置 true / 链路断置 false）。主线程 `send_frame` 据此选路：
    /// true 走 QUIC、false 走中继（两端对齐的切换屏障）。
    data_ready: Arc<AtomicBool>,
    /// 停机标志（与 `Stop` 命令双保险）。
    stop: Arc<AtomicBool>,
    /// 后台线程句柄（stop / drop 时 join）。
    handle: Option<JoinHandle<()>>,
}

impl P2pEngine {
    /// 启动 P2P 打洞引擎（线程内建 current-thread tokio runtime）。`role` 决定 Offer/Answer 流向
    /// （[`Role::Controller`] 主动发 Offer）；`stun_host` 为端点发现用的 STUN 服务器（host:port，
    /// 空串 = 不探 STUN、仅用 LAN 候选）。
    /// `ctx`/`proxy`/`pending` 为主线程唤醒句柄（[`RemoteWs`] 持有的同一套）：P2P 收到数据帧/事件后
    /// 用它 nudge 主线程重绘——**漏传则回显/目录树延迟数秒**（按需重绘 UI 不会及时 poll，见 [`Waker`]）。
    #[must_use]
    pub fn start(
        role: Role,
        stun_host: String,
        ctx: Option<egui::Context>,
        proxy: Option<EventLoopProxy<PtyWake>>,
        pending: Option<Arc<AtomicBool>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (data_tx, data_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let data_ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let data_thread = Arc::clone(&data_ready);
        let waker = match (ctx, proxy, pending) {
            (Some(ctx), Some(proxy), Some(pending)) => Some(Waker {
                ctx,
                proxy,
                pending,
            }),
            _ => None,
        };
        let handle = thread::Builder::new()
            .name("lumen-p2p".into())
            .spawn(move || {
                run(
                    role,
                    stun_host,
                    cmd_rx,
                    data_rx,
                    &evt_tx,
                    &stop_thread,
                    &data_thread,
                    waker,
                )
            })
            .ok();
        Self {
            cmd_tx,
            data_tx,
            evt_rx,
            data_ready,
            stop,
            handle,
        }
    }

    /// 把对端经信令通道发来的 P2pSignal 投给打洞状态机。
    pub fn peer_signal(&self, kind: P2pSignalKind, payload: SignalPayload) {
        let _ = self.cmd_tx.send(P2pCmd::PeerSignal { kind, payload });
    }

    /// 数据面是否就绪（主线程 `send_frame` 选路读此；true 走 QUIC、false 走中继）。
    pub fn is_data_ready(&self) -> bool {
        self.data_ready.load(Ordering::Acquire)
    }

    /// 把一条数据面帧（`frame.to_value()` 的 Value）投到 QUIC 出站写泵。返回 `false` = 通道已断
    /// （调用方应回退中继）。
    pub fn try_send_frame(&self, value: serde_json::Value) -> bool {
        self.data_tx.send(value).is_ok()
    }

    /// 非阻塞排空 P2P 线程事件（主线程每帧调用）。
    pub fn poll(&self) -> Vec<P2pEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.evt_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// 置停机标志 + 投 `Stop`（唤醒阻塞在 `recv` 的线程）。
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(P2pCmd::Stop);
    }
}

impl Drop for P2pEngine {
    fn drop(&mut self) {
        // 非阻塞停机：置停机标志 + 投 Stop，后台线程在当前 await 点（STUN / 握手）结束后自行退出。
        // **故意不 join**——避免主线程在 STUN/打洞窗口被 join 阻塞造成 UI 卡顿；线程持有的 quinn
        // endpoint 绑临时端口，自行收尾不影响新会话。`stop()` 提供需要时的阻塞式停机。
        self.signal_stop();
        let _ = self.handle.take();
    }
}

/// 生成一次打洞会话的 nonce（时间 + 计数器混合，区分并发打洞，非密码学随机）。
fn gen_nonce() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (TXN_COUNTER.fetch_add(1, Ordering::Relaxed) << 32)
}

/// 对一条对端信令负载发起打洞（spawn 异步 punch；`punching` 守卫避免重复打洞）。成功的连接经
/// `res_tx` 回主循环保活；失败/超时则经 `evt_tx` 主动通告对端 `Fallback`（`fallback`
/// 为本端 `SignalPayload`，仅用 nonce 关联本次会话），让对端不必干等满 5s 超时即知回退中继。
#[allow(clippy::too_many_arguments)] // 与主循环共享的一组句柄/通道，拆结构体反更晦涩。
fn spawn_punch(
    de: &DirectEndpoint,
    peer: SignalPayload,
    res_tx: &UnboundedSender<quinn::Connection>,
    punching: &mut bool,
    role: Role,
    evt_tx: &Sender<P2pEvent>,
    waker: &Option<Waker>,
    fallback: SignalPayload,
) {
    if *punching {
        return;
    }
    *punching = true;
    let ep = de.endpoint.clone();
    let cert = de.cert.clone();
    let peer_cert = CertificateDer::from(peer.cert_der);
    let cands = peer.candidates;
    let tx = res_tx.clone();
    let evt = evt_tx.clone();
    let wk = waker.clone();
    tokio::spawn(async move {
        match punch(&ep, &cert, &peer_cert, &cands, PUNCH_TIMEOUT, role).await {
            Some(conn) => {
                let _ = tx.send(conn);
            }
            None => {
                log::info!("P2P 打洞失败/超时，主动通告对端回退中继");
                emit(
                    &evt,
                    &wk,
                    P2pEvent::SendSignal {
                        kind: P2pSignalKind::Fallback,
                        payload: fallback,
                    },
                );
            }
        }
    });
}

/// P2P 后台线程主体：建 current-thread runtime，构建直连端点 + 跑打洞信令状态机 + 数据面收发。
#[allow(clippy::too_many_arguments)] // 与主线程共享的句柄/通道一组，拆结构体反更晦涩。
fn run(
    role: Role,
    stun_host: String,
    mut cmd_rx: UnboundedReceiver<P2pCmd>,
    data_rx: UnboundedReceiver<serde_json::Value>,
    evt_tx: &Sender<P2pEvent>,
    stop: &AtomicBool,
    data_ready: &Arc<AtomicBool>,
    waker: Option<Waker>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("P2P tokio runtime 创建失败: {e}");
            return;
        }
    };
    rt.block_on(async move {
        let stun = if stun_host.is_empty() {
            None
        } else {
            Some(stun_host.as_str())
        };
        let de = match build_endpoint(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), stun).await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("P2P endpoint 构建失败，放弃直连（中继不受影响）: {e}");
                while let Some(cmd) = cmd_rx.recv().await {
                    if matches!(cmd, P2pCmd::Stop) {
                        break;
                    }
                }
                return;
            }
        };
        log::debug!("P2P 本端候选端点: {:?}", de.candidates);
        let my_payload = SignalPayload {
            candidates: de.candidates.clone(),
            cert_der: de.cert.cert_der.clone(),
            nonce: gen_nonce(),
        };
        // 控制端主动发 Offer；被控端收到 Offer 后回 Answer（见 PeerSignal 处理）。
        if role == Role::Controller {
            emit(
                evt_tx,
                &waker,
                P2pEvent::SendSignal {
                    kind: P2pSignalKind::Offer,
                    payload: my_payload.clone(),
                },
            );
        }
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel::<quinn::Connection>();
        let mut conn_keep: Option<quinn::Connection> = None;
        let mut data_rx_opt = Some(data_rx);
        let mut punching = false;
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(P2pCmd::PeerSignal { kind, payload }) => match kind {
                        P2pSignalKind::Offer => {
                            // 被控端：回 Answer 并开始打洞。
                            emit(
                                evt_tx,
                                &waker,
                                P2pEvent::SendSignal {
                                    kind: P2pSignalKind::Answer,
                                    payload: my_payload.clone(),
                                },
                            );
                            spawn_punch(
                                &de,
                                payload,
                                &res_tx,
                                &mut punching,
                                role,
                                evt_tx,
                                &waker,
                                my_payload.clone(),
                            );
                        }
                        P2pSignalKind::Answer => {
                            spawn_punch(
                                &de,
                                payload,
                                &res_tx,
                                &mut punching,
                                role,
                                evt_tx,
                                &waker,
                                my_payload.clone(),
                            );
                        }
                        // data_ready 由 run_data_plane 在**本端数据面流真正建立**时置位（而非据对端 Ready），
                        // 避免流失败/未建时误判已直连（审查 #4）。Ready 信令仅作诊断。
                        P2pSignalKind::Ready => log::debug!("P2P 对端报告直连就绪"),
                        // 收对端打洞失败的主动 Fallback 通告（双端另各有 5s 超时兜底）。本端为中继优先、
                        // P2P 仅叠加优化，此处无需改路由，记一条诊断即可。
                        P2pSignalKind::Fallback => log::info!("P2P 对端宣告回退中继"),
                    },
                    Some(P2pCmd::Stop) | None => break,
                },
                Some(conn) = res_rx.recv() => {
                    log::info!("P2P 直连已通 → {}", conn.remote_address());
                    emit(evt_tx, &waker, P2pEvent::Connected);
                    // 通告对端「我直连已通」（仅诊断；data_ready 由本端 run_data_plane 在流建立时置位）。
                    emit(
                        evt_tx,
                        &waker,
                        P2pEvent::SendSignal {
                            kind: P2pSignalKind::Ready,
                            payload: my_payload.clone(),
                        },
                    );
                    // 在此 conn 上建立数据面双向流（仅一次）。
                    if let Some(drx) = data_rx_opt.take() {
                        tokio::spawn(run_data_plane(
                            conn.clone(),
                            role,
                            drx,
                            evt_tx.clone(),
                            Arc::clone(data_ready),
                            waker.clone(),
                        ));
                    }
                    conn_keep = Some(conn); // 保活直连（其 drop 关连接）。
                }
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
        }
        drop(conn_keep);
    });
}

/// 置数据面失效：清 `data_ready` + 通知主线程回退中继（带 nudge）。可被多个泵/监视重复调用（幂等）。
fn fail_data_plane(data_ready: &AtomicBool, evt_tx: &Sender<P2pEvent>, waker: &Option<Waker>) {
    data_ready.store(false, Ordering::Release);
    emit(evt_tx, waker, P2pEvent::DataPlaneDown);
}

/// 在已建立的 QUIC 连接上建数据面双向流并跑读/写泵 + 连接关闭监视。Controller `open_bi` 后**立即写
/// 首帧 Hello**（解除对端 `accept_bi` 阻塞，quinn open_bi 惰性）；Controlled `accept_bi`。任一环节
/// 出错即 `fail_data_plane`（主线程回退中继）。
async fn run_data_plane(
    conn: quinn::Connection,
    role: Role,
    mut data_rx: UnboundedReceiver<serde_json::Value>,
    evt_tx: Sender<P2pEvent>,
    data_ready: Arc<AtomicBool>,
    waker: Option<Waker>,
) {
    let (mut send, recv) = match role {
        Role::Controller => match conn.open_bi().await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("P2P open_bi 失败: {e}");
                fail_data_plane(&data_ready, &evt_tx, &waker);
                return;
            }
        },
        Role::Controlled => match conn.accept_bi().await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("P2P accept_bi 失败: {e}");
                fail_data_plane(&data_ready, &evt_tx, &waker);
                return;
            }
        },
    };
    // Controller 必须先写一帧解除对端 accept_bi 阻塞（quinn open_bi 惰性，不写首字节对端永久等待）。
    if matches!(role, Role::Controller) {
        match RemoteFrame::P2pStreamHello.to_value() {
            Ok(v) if write_value(&mut send, &v).await => {}
            _ => {
                fail_data_plane(&data_ready, &evt_tx, &waker);
                return;
            }
        }
    }
    // 数据面流真正建立 → 置 data_ready + 通知主线程（被控端据此把输出走 QUIC；控制端据此补发订阅
    // 重建镜像）。在此置位（而非据对端 Ready）确保流失败时不会误判已直连（审查 #4）。
    data_ready.store(true, Ordering::Release);
    emit(&evt_tx, &waker, P2pEvent::DataPlaneUp);
    // 读泵（独立任务）。
    {
        let evt = evt_tx.clone();
        let dr = Arc::clone(&data_ready);
        let wk = waker.clone();
        tokio::spawn(async move { read_pump(recv, &evt, &dr, &wk).await });
    }
    // 连接关闭监视（idle 超时 / 对端关 / 链路丢 → 回退）。
    {
        let conn2 = conn.clone();
        let evt = evt_tx.clone();
        let dr = Arc::clone(&data_ready);
        let wk = waker.clone();
        tokio::spawn(async move {
            let reason = conn2.closed().await;
            log::info!("P2P 连接关闭: {reason}");
            fail_data_plane(&dr, &evt, &wk);
        });
    }
    // 写泵（本任务）：排空主线程投来的数据帧，length-prefix 写出 QUIC 流。
    while let Some(v) = data_rx.recv().await {
        if !write_value(&mut send, &v).await {
            break;
        }
    }
    fail_data_plane(&data_ready, &evt_tx, &waker);
}

/// 数据面读泵：循环 read_exact(4 字节长度) → 校验上限 → read_exact(帧体) → 解析 Value → 投主线程。
/// 链路断 / 干净 FIN / 长度非法即退出并 `fail_data_plane`。解析失败仅丢该帧、不断流。
async fn read_pump(
    mut recv: quinn::RecvStream,
    evt_tx: &Sender<P2pEvent>,
    data_ready: &AtomicBool,
    waker: &Option<Waker>,
) {
    loop {
        let mut len_buf = [0u8; 4];
        if recv.read_exact(&mut len_buf).await.is_err() {
            break; // 链路断 / 干净 FIN
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > MAX_DATA_FRAME {
            log::warn!("P2P 数据帧长度非法({len})，断流回退");
            break;
        }
        let mut buf = vec![0u8; len];
        if recv.read_exact(&mut buf).await.is_err() {
            break;
        }
        match serde_json::from_slice::<serde_json::Value>(&buf) {
            Ok(v) => {
                if evt_tx.send(P2pEvent::DataFrame(v)).is_err() {
                    break; // 主线程已收尾
                }
                // **关键**：唤醒主线程及时 poll 出该帧（漏 nudge → 回显延迟数秒，见 Waker）。
                if let Some(w) = waker {
                    w.nudge();
                }
            }
            Err(e) => log::debug!("P2P 数据帧解析失败，丢弃: {e}"),
        }
    }
    fail_data_plane(data_ready, evt_tx, waker);
}

/// length-prefix 写一条 Value 到 QUIC 发送流：u32 大端长度 + JSON 字节。序列化失败/超限仅跳过该帧
/// （返回 `true` 不杀流）；写入失败（链路断）返回 `false`。
async fn write_value(send: &mut quinn::SendStream, value: &serde_json::Value) -> bool {
    let Ok(json) = serde_json::to_vec(value) else {
        log::error!("P2P 数据帧序列化失败，跳过");
        return true;
    };
    if json.len() > MAX_DATA_FRAME {
        log::error!("P2P 数据帧过大({})，跳过", json.len());
        return true;
    }
    let len = (json.len() as u32).to_be_bytes();
    if send.write_all(&len).await.is_err() {
        return false;
    }
    send.write_all(&json).await.is_ok()
}

/// 取本机出口网卡的 LAN 地址（connect-trick：UDP `connect` 到外部地址**不实际发包**，仅令内核选
/// 路由、`local_addr` 即出口网卡 IP）。零依赖、规避枚举全部网卡。失败返回 `None`。
fn local_lan_addr() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// STUN transaction id 发号器（进程级单调，混入时间——非密码学随机，端点发现足够区分应答）。
static TXN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成一个 96-bit STUN transaction id（时间纳秒低 64 位 + 进程内计数器 32 位）。
fn new_txn_id() -> [u8; 12] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let ctr = TXN_COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
    let mut id = [0u8; 12];
    id[0..8].copy_from_slice(&nanos.to_le_bytes());
    id[8..12].copy_from_slice(&ctr.to_le_bytes());
    id
}

/// 构造 RFC 5389 Binding Request（20 字节定长头、无属性）：type(0x0001) + length(0) + magic
/// cookie + 96-bit transaction id。
fn build_binding_request(txn: &[u8; 12]) -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
    req[2..4].copy_from_slice(&0u16.to_be_bytes()); // 无属性
    req[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    req[8..20].copy_from_slice(txn);
    req
}

/// 解析 STUN Binding Success Response（0x0101），提取 XOR-MAPPED-ADDRESS（0x0020）的 IPv4 端点。
/// 校验消息类型 / magic cookie / transaction id；遍历属性（4 字节对齐）；越界 / 非预期一律返回 `None`。
fn parse_xor_mapped_addr(resp: &[u8], txn: &[u8; 12]) -> Option<SocketAddr> {
    if resp.len() < 20 {
        return None;
    }
    if u16::from_be_bytes([resp[0], resp[1]]) != 0x0101 {
        return None; // 仅认 Binding Success Response
    }
    if u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]) != MAGIC_COOKIE {
        return None;
    }
    if &resp[8..20] != txn {
        return None; // 应答与请求不匹配（防串扰 / 陈旧）
    }
    let msg_len = usize::from(u16::from_be_bytes([resp[2], resp[3]]));
    let attrs = resp.get(20..20 + msg_len)?;
    let mut i = 0usize;
    while i + 4 <= attrs.len() {
        let atype = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let alen = usize::from(u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]));
        let val = attrs.get(i + 4..i + 4 + alen)?;
        if atype == 0x0020 {
            return decode_xor_mapped(val);
        }
        // 属性值按 4 字节对齐填充。
        i += 4 + alen + (4 - alen % 4) % 4;
    }
    None
}

/// 解码 XOR-MAPPED-ADDRESS 属性值（reserved(1) + family(1) + x-port(2) + x-address(4=IPv4)）。
/// IPv6 留待后续阶段。
fn decode_xor_mapped(val: &[u8]) -> Option<SocketAddr> {
    if val.len() < 8 || val[1] != 0x01 {
        return None; // 仅 IPv4
    }
    let x_port = u16::from_be_bytes([val[2], val[3]]);
    let port = x_port ^ (MAGIC_COOKIE >> 16) as u16;
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let ip = Ipv4Addr::new(
        val[4] ^ cookie[0],
        val[5] ^ cookie[1],
        val[6] ^ cookie[2],
        val[7] ^ cookie[3],
    );
    Some(SocketAddr::from((ip, port)))
}

/// 自签证书（P2P 直连握手用）：DER 编码的证书 + PKCS#8 私钥。证书 DER 本身经信令通道交换
/// 校验作信任锚（防 MITM，见设计 §6）——比较完整证书 DER 等价于最强指纹。
#[derive(Clone)]
pub struct SelfSignedCert {
    /// 证书 DER。
    pub cert_der: Vec<u8>,
    /// PKCS#8 私钥 DER。
    pub key_der: Vec<u8>,
}

/// 生成一张临时自签证书（rcgen，ring 后端）。Phase 2 用于 quinn server 侧 + 指纹信任锚。
///
/// # Errors
/// rcgen 生成失败（密钥生成 / 序列化错误）时返回。
fn generate_self_signed() -> anyhow::Result<SelfSignedCert> {
    let ck = rcgen::generate_simple_self_signed(vec!["lumen-p2p".to_string()])?;
    Ok(SelfSignedCert {
        cert_der: ck.cert.der().to_vec(),
        key_der: ck.key_pair.serialize_der(),
    })
}

// ── Phase 2：QUIC 打洞 + mTLS 握手（指纹信任锚）──────────────────────────────────
//    会话建立后双方经信令通道（[`super::RemoteFrame::P2pSignal`]）交换 [`SignalPayload`]
//    （候选端点 + 自签证书），各自把 quinn Endpoint 既作 client（connect 对端候选）又作 server
//    （accept 对端连入）打洞；**mTLS 双向认证** + pinned 验证器只认信令交换的对端证书（防 MITM）。
//    Phase 2 仅建立连接 + 日志「直连已通」，**不切数据面**（数据面切换 + 中继回退是 Phase 3）。

/// P2P 信令负载（[`super::RemoteFrame::P2pSignal`] 的 `payload` JSON 内层结构）。Offer / Answer
/// 都携本端候选端点 + 自签证书 DER + nonce；Ready / Fallback 可仅用 nonce 关联本次打洞会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalPayload {
    /// 本端候选端点（LAN + STUN 公网映射，**端口与下方 QUIC endpoint 同源**）。
    pub candidates: Vec<SocketAddr>,
    /// 本端自签证书 DER（对端 pinned 验证器据此认证，信任锚见设计 §6）。
    pub cert_der: Vec<u8>,
    /// 本次打洞会话随机数（防重放 / 区分并发打洞）。
    pub nonce: u64,
}

/// 一个就绪的 QUIC 直连端点（绑定持久 UDP socket、自签证书、候选端点）。
pub struct DirectEndpoint {
    /// quinn 端点（既 connect 又 accept，打洞用）。
    pub endpoint: quinn::Endpoint,
    /// 本端自签证书。
    pub cert: SelfSignedCert,
    /// 本端候选端点（交给对端打洞）。
    pub candidates: Vec<SocketAddr>,
}

/// 构建 QUIC 直连端点：绑持久 UDP socket → （可选）经**同一 socket** STUN 探公网映射端点（保证
/// 公网端口 == QUIC 端口）→ 收集候选 → 用该 socket 建 quinn Endpoint。`bind` 形如 `0.0.0.0:0`
/// （生产）或 `127.0.0.1:0`（loopback 测试）；`stun_host` 为 `None` 时仅用本地候选。
///
/// # Errors
/// 绑定 / socket 转换 / endpoint 创建失败时返回。
pub async fn build_endpoint(bind: SocketAddr, stun_host: Option<&str>) -> anyhow::Result<DirectEndpoint> {
    let std_sock = std::net::UdpSocket::bind(bind)?;
    std_sock.set_nonblocking(true)?;
    let local_addr = std_sock.local_addr()?;
    // 经同一 socket 探 STUN（async）；探完转回 std socket 交给 quinn（保 NAT 映射端口一致）。
    let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
    let mut candidates = vec![local_addr];
    if let Some(ip) = local_lan_addr() {
        let lan = SocketAddr::new(ip, local_addr.port());
        if lan != local_addr {
            candidates.push(lan);
        }
    }
    if let Some(host) = stun_host {
        if let Some(public) = stun_query(&tokio_sock, host, STUN_TIMEOUT).await {
            candidates.push(public);
        }
    }
    let std_back = tokio_sock.into_std()?;
    let runtime = quinn::default_runtime().ok_or_else(|| anyhow::anyhow!("无 tokio runtime"))?;
    // server_config 此时未知对端证书，置 None；打洞时 set_server_config 注入 mTLS pinned 配置。
    let endpoint = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, std_back, runtime)?;
    let cert = generate_self_signed()?;
    Ok(DirectEndpoint {
        endpoint,
        cert,
        candidates,
    })
}

/// 经**已有** socket 发 STUN Binding（不 `connect`，留 socket 给 quinn 复用）：`send_to` 请求 →
/// `recv_from` 应答 → 解析 XOR-MAPPED-ADDRESS。超时 / 失败返回 `None`。
async fn stun_query(
    sock: &tokio::net::UdpSocket,
    stun_host: &str,
    timeout: Duration,
) -> Option<SocketAddr> {
    let target = tokio::net::lookup_host(stun_host)
        .await
        .ok()?
        .find(SocketAddr::is_ipv4)?;
    let txn = new_txn_id();
    let req = build_binding_request(&txn);
    sock.send_to(&req, target).await.ok()?;
    let mut buf = [0u8; 512];
    let (n, _src) = tokio::time::timeout(timeout, sock.recv_from(&mut buf))
        .await
        .ok()?
        .ok()?;
    parse_xor_mapped_addr(&buf[..n], &txn)
}

/// 打洞 + mTLS 握手：给定对端候选 + 对端证书，本端 endpoint 同时 connect 全部候选 + accept 连入，
/// 取**首个成功建立**的 QUIC 连接（双向认证：connect 侧认证对端 server 证书、accept 侧认证对端
/// client 证书，均 pinned 到信令交换的对端证书 → 防 MITM）。`PUNCH_TIMEOUT` 内无连接即 `None`
/// （打洞失败，调用方回退中继）。
pub async fn punch(
    endpoint: &quinn::Endpoint,
    own: &SelfSignedCert,
    peer_cert: &CertificateDer<'static>,
    peer_candidates: &[SocketAddr],
    timeout: Duration,
    role: Role,
) -> Option<quinn::Connection> {
    // accept 侧（对端 connect 到我）：mTLS server 配置，client 证书 pinned 到对端。
    match make_server_config(own, peer_cert) {
        Ok(scfg) => endpoint.set_server_config(Some(scfg)),
        Err(e) => {
            log::warn!("P2P server 配置构建失败: {e}");
            return None;
        }
    }
    let client_cfg = match make_client_config(own, peer_cert) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("P2P client 配置构建失败: {e}");
            return None;
        }
    };

    // 双向打洞开两端 NAT，但**按角色选定同一条连接**作数据面承载（否则两端各留各的连接、
    // open_bi/accept_bi 不在同一连接上）：Controller 取自己 connect 出去的（A→B），Controlled 取自己
    // accept 进来的（同为 A→B）。另一方向（B→A）仅用于开 NAT，连上即弃。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<quinn::Connection>(4);
    // accept 任务：Controlled 取首个 accept 作结果；Controller 的 accept 是冗余方向，连上即弃。
    {
        let ep = endpoint.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                match incoming.accept() {
                    Ok(connecting) => match connecting.await {
                        Ok(conn) => {
                            if matches!(role, Role::Controlled) {
                                let _ = tx.send(conn).await;
                                break;
                            }
                            // Controller：accept 到的是 B→A 冗余方向，弃（drop conn 即关）。
                        }
                        Err(e) => log::debug!("P2P accept 握手失败: {e}"),
                    },
                    Err(e) => log::debug!("P2P accept 拒绝连入: {e}"),
                }
            }
        });
    }
    // connect 任务：Controller 取首个 connect 作结果；Controlled 的 connect 仅开自身 NAT，连上即弃。
    for cand in peer_candidates {
        match endpoint.connect_with(client_cfg.clone(), *cand, SERVER_NAME) {
            Ok(connecting) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = connecting.await {
                        if matches!(role, Role::Controller) {
                            let _ = tx.send(conn).await;
                        }
                        // Controlled：connect 仅为开 NAT，弃。
                    }
                });
            }
            Err(e) => log::debug!("P2P connect {cand} 发起失败: {e}"),
        }
    }
    drop(tx); // 所有发送端 drop 后 recv 返回 None（全失败时不空等到超时）。
    tokio::time::timeout(timeout, rx.recv()).await.ok().flatten()
}

/// ring CryptoProvider（与 quinn/rustls 后端一致）。
fn ring_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 构建 quinn client 配置：pinned server 验证器（只认 `peer_cert`）+ 本端 client 证书（mTLS）。
fn make_client_config(
    own: &SelfSignedCert,
    peer_cert: &CertificateDer<'static>,
) -> anyhow::Result<quinn::ClientConfig> {
    let provider = ring_provider();
    let verifier = Arc::new(PinnedServerVerifier {
        expected: peer_cert.clone(),
        provider: provider.clone(),
    });
    let chain = vec![CertificateDer::from(own.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(own.key_der.clone()));
    let mut rcfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain, key)?;
    rcfg.alpn_protocols = vec![ALPN.to_vec()];
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(rcfg)?;
    let mut cc = quinn::ClientConfig::new(Arc::new(qc));
    cc.transport_config(transport_config());
    Ok(cc)
}

/// 构建 quinn server 配置：pinned client 验证器（mTLS，只认 `peer_cert`）+ 本端 server 证书。
fn make_server_config(
    own: &SelfSignedCert,
    peer_cert: &CertificateDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    let provider = ring_provider();
    let verifier = Arc::new(PinnedClientVerifier {
        expected: peer_cert.clone(),
        provider: provider.clone(),
        roots: Vec::new(),
    });
    let chain = vec![CertificateDer::from(own.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(own.key_der.clone()));
    let mut rcfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    rcfg.alpn_protocols = vec![ALPN.to_vec()];
    let qs = quinn::crypto::rustls::QuicServerConfig::try_from(rcfg)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qs));
    sc.transport_config(transport_config());
    Ok(sc)
}

/// QUIC 传输参数：keep-alive + 最大空闲超时（防终端会话空闲被静默关连；两端各自设，取较小生效）。
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut tc = quinn::TransportConfig::default();
    tc.keep_alive_interval(Some(P2P_KEEPALIVE));
    // P2P_MAX_IDLE（30s）远在 IdleTimeout 合法区间内，转换不会失败。
    if let Ok(idle) = quinn::IdleTimeout::try_from(P2P_MAX_IDLE) {
        tc.max_idle_timeout(Some(idle));
    }
    Arc::new(tc)
}

/// 委托 ring provider 做 TLS1.2 证书签名校验（pinned 验证器只额外比对证书本体）。
fn delegate_tls12(
    provider: &rustls::crypto::CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls12_signature(message, cert, dss, &provider.signature_verification_algorithms)
}

/// 委托 ring provider 做 TLS1.3 证书签名校验。
fn delegate_tls13(
    provider: &rustls::crypto::CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(message, cert, dss, &provider.signature_verification_algorithms)
}

/// client 侧验证对端 **server** 证书：只接受 == 信令交换的对端证书（防 MITM）。
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: CertificateDer<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_ref() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("P2P 对端证书不匹配（防 MITM）".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        delegate_tls12(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        delegate_tls13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// server 侧验证对端 **client** 证书（mTLS）：只接受 == 信令交换的对端证书。
#[derive(Debug)]
struct PinnedClientVerifier {
    expected: CertificateDer<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// 空根提示集（自签 + pinned，不需 CA 根提示）。
    roots: Vec<rustls::DistinguishedName>,
}

impl rustls::server::danger::ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &self.roots
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_ref() {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General("P2P 对端 client 证书不匹配（防 MITM）".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        delegate_tls12(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        delegate_tls13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_binding_request_格式正确() {
        let txn = [7u8; 12];
        let req = build_binding_request(&txn);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), 0x0001); // Binding Request
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0); // 无属性
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&req[8..20], &txn);
    }

    /// 手工构造一个带 XOR-MAPPED-ADDRESS 的 Binding Success Response，断言解出原始公网端点。
    #[test]
    fn stun_响应解析_xor_mapped_ipv4() {
        let txn = [1u8; 12];
        // 期望解出 203.0.113.5:51234。
        // x-port = 51234 ^ (cookie>>16=0x2112) = 0xC822 ^ 0x2112 = 0xE930。
        // x-addr = [203,0,113,5] ^ [0x21,0x12,0xA4,0x42] = [0xEA,0x12,0xD5,0x47]。
        let mut msg = Vec::new();
        msg.extend_from_slice(&0x0101u16.to_be_bytes()); // Success Response
        msg.extend_from_slice(&12u16.to_be_bytes()); // 属性总长 = 4 头 + 8 值
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txn);
        msg.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0x00); // reserved
        msg.push(0x01); // family IPv4
        msg.extend_from_slice(&[0xE9, 0x30]); // x-port
        msg.extend_from_slice(&[0xEA, 0x12, 0xD5, 0x47]); // x-address
        let got = parse_xor_mapped_addr(&msg, &txn).expect("应解出端点");
        assert_eq!(got, "203.0.113.5:51234".parse().expect("地址"));
    }

    #[test]
    fn stun_响应_错误类型或cookie或txn一律拒绝() {
        let txn = [2u8; 12];
        let mut ok = Vec::new();
        ok.extend_from_slice(&0x0101u16.to_be_bytes());
        ok.extend_from_slice(&0u16.to_be_bytes());
        ok.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        ok.extend_from_slice(&txn);
        // 无 XOR-MAPPED-ADDRESS 属性 → None（但不 panic）。
        assert!(parse_xor_mapped_addr(&ok, &txn).is_none());
        // 错误消息类型。
        let mut bad_type = ok.clone();
        bad_type[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        assert!(parse_xor_mapped_addr(&bad_type, &txn).is_none());
        // 错误 magic cookie。
        let mut bad_cookie = ok.clone();
        bad_cookie[4] ^= 0xFF;
        assert!(parse_xor_mapped_addr(&bad_cookie, &txn).is_none());
        // transaction id 不匹配。
        assert!(parse_xor_mapped_addr(&ok, &[9u8; 12]).is_none());
        // 过短缓冲。
        assert!(parse_xor_mapped_addr(&[0u8; 8], &txn).is_none());
    }

    #[test]
    fn txn_id_单调不重复() {
        let a = new_txn_id();
        let b = new_txn_id();
        assert_ne!(a, b, "连续两次 transaction id 应不同");
    }

    #[test]
    fn 本地_lan_地址_不panic() {
        // 仅验证不 panic（CI 无网卡时可能 None）。
        let _ = local_lan_addr();
    }

    #[test]
    fn 自签证书生成_smoke() {
        let cert = generate_self_signed().expect("生成自签证书");
        assert!(!cert.cert_der.is_empty(), "证书 DER 非空");
        assert!(!cert.key_der.is_empty(), "私钥 DER 非空");
    }

    #[test]
    fn 引擎启停_不panic() {
        // 空 STUN → 不联网；仅验证起线程 + 停机不 panic（无对端信令故不会握手）。无唤醒句柄（测试态）。
        let eng = P2pEngine::start(Role::Controller, String::new(), None, None, None);
        assert!(!eng.is_data_ready(), "未握手前数据面不应就绪");
        eng.peer_signal(
            P2pSignalKind::Fallback,
            SignalPayload {
                candidates: vec![],
                cert_der: vec![],
                nonce: 1,
            },
        );
        drop(eng); // Drop 触发非阻塞停机（signal_stop + 弃 handle）。
    }

    /// 测试用 mock STUN 反射：构造 Binding Success Response，XOR-MAPPED-ADDRESS = `src`。
    /// 与 `server/lumen-server/src/stun.rs` 生产逻辑对称（协议常量一致）。
    fn mock_binding_response(req: &[u8], src: SocketAddr) -> Option<Vec<u8>> {
        if req.len() < 20 || u16::from_be_bytes([req[0], req[1]]) != 0x0001 {
            return None;
        }
        let SocketAddr::V4(v4) = src else { return None };
        let ip = v4.ip().octets();
        let cookie = MAGIC_COOKIE.to_be_bytes();
        let x_port = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
        let mut resp = Vec::with_capacity(32);
        resp.extend_from_slice(&0x0101u16.to_be_bytes());
        resp.extend_from_slice(&12u16.to_be_bytes());
        resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        resp.extend_from_slice(&req[8..20]); // 原样回带 transaction id
        resp.extend_from_slice(&0x0020u16.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0x00);
        resp.push(0x01);
        resp.extend_from_slice(&x_port.to_be_bytes());
        resp.extend_from_slice(&[
            ip[0] ^ cookie[0],
            ip[1] ^ cookie[1],
            ip[2] ^ cookie[2],
            ip[3] ^ cookie[3],
        ]);
        Some(resp)
    }

    /// 端到端：生产用的 `stun_query` 打本地 mock 反射，走完「构造请求→发→收→XOR 解析」全链路，
    /// 探到本机回环源端点（Phase 1 验收点「探公网端点」的离线可重复验证；直接覆盖生产 STUN 路径）。
    #[tokio::test]
    async fn stun_query_对本地mock反射_探到端点() {
        let server = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind mock 反射");
        let server_addr = server.local_addr().expect("反射地址");
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            if let Ok((n, src)) = server.recv_from(&mut buf).await {
                if let Some(resp) = mock_binding_response(&buf[..n], src) {
                    let _ = server.send_to(&resp, src).await;
                }
            }
        });
        let client = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind client");
        let got = stun_query(&client, &server_addr.to_string(), STUN_TIMEOUT).await;
        let _ = task.await;
        let addr = got.expect("应探到端点");
        assert!(addr.ip().is_loopback(), "源地址应为本机回环");
        assert_ne!(addr.port(), 0, "应得到具体端口");
    }

    #[test]
    fn signal_payload_序列化往返() {
        let p = SignalPayload {
            candidates: vec!["192.168.1.5:50000".parse().expect("候选")],
            cert_der: vec![1, 2, 3, 4],
            nonce: 42,
        };
        let s = serde_json::to_string(&p).expect("序列化");
        let back: SignalPayload = serde_json::from_str(&s).expect("反序列化");
        assert_eq!(back.candidates, p.candidates);
        assert_eq!(back.cert_der, p.cert_der);
        assert_eq!(back.nonce, p.nonce);
    }

    /// loopback 双端：A↔B 各自打洞 + mTLS 双向认证，两端都应建立直连（QUIC 握手 + 证书 pinned 通过）。
    #[tokio::test]
    async fn quic_直连_双端互认握手() {
        let loop_bind: SocketAddr = "127.0.0.1:0".parse().expect("bind");
        let a = build_endpoint(loop_bind, None).await.expect("A endpoint");
        let b = build_endpoint(loop_bind, None).await.expect("B endpoint");
        let a_addr = a.endpoint.local_addr().expect("A 地址");
        let b_addr = b.endpoint.local_addr().expect("B 地址");
        let a_cert = CertificateDer::from(a.cert.cert_der.clone());
        let b_cert = CertificateDer::from(b.cert.cert_der.clone());

        let a_ep = a.endpoint.clone();
        let a_self = a.cert.clone();
        let b_ep = b.endpoint.clone();
        let b_self = b.cert.clone();
        let ja = tokio::spawn(async move {
            punch(&a_ep, &a_self, &b_cert, &[b_addr], Duration::from_secs(5), Role::Controller)
                .await
                .is_some()
        });
        let jb = tokio::spawn(async move {
            punch(&b_ep, &b_self, &a_cert, &[a_addr], Duration::from_secs(5), Role::Controlled)
                .await
                .is_some()
        });
        let ra = ja.await.expect("A 任务");
        let rb = jb.await.expect("B 任务");
        assert!(ra && rb, "双端应各自建立直连（A={ra} B={rb}）");
    }

    /// 伪造证书冒充对端：A 用一张与真 B 无关的证书作信任锚连真 B → mTLS 校验失败、握手不成（防 MITM）。
    #[tokio::test]
    async fn quic_直连_伪造证书被拒() {
        let loop_bind: SocketAddr = "127.0.0.1:0".parse().expect("bind");
        let a = build_endpoint(loop_bind, None).await.expect("A endpoint");
        let b = build_endpoint(loop_bind, None).await.expect("B endpoint");
        let b_addr = b.endpoint.local_addr().expect("B 地址");
        let a_cert = CertificateDer::from(a.cert.cert_der.clone());
        // 与真 B 无关的伪造证书。
        let fake = generate_self_signed().expect("伪造证书");
        let fake_cert = CertificateDer::from(fake.cert_der);
        // 真 B 仅起 accept（认 A），让 A 的连接被真实握手。
        let b_ep = b.endpoint.clone();
        let b_self = b.cert.clone();
        let _jb = tokio::spawn(async move {
            let _ = punch(&b_ep, &b_self, &a_cert, &[], Duration::from_secs(2), Role::Controlled).await;
        });
        let got =
            punch(&a.endpoint, &a.cert, &fake_cert, &[b_addr], Duration::from_secs(2), Role::Controller).await;
        assert!(got.is_none(), "对端证书不匹配应握手失败");
    }

    /// loopback 数据面：建立直连后 Controller open_bi + 写 Hello 首帧 + 一帧 Output，Controlled
    /// accept_bi + read_pump 解出两帧（验证 open_bi 惰性解除、length-prefix 分帧、读泵全链路）。
    #[tokio::test]
    async fn quic_数据面_单流_帧往返() {
        let loop_bind: SocketAddr = "127.0.0.1:0".parse().expect("bind");
        let a = build_endpoint(loop_bind, None).await.expect("A endpoint");
        let b = build_endpoint(loop_bind, None).await.expect("B endpoint");
        let a_addr = a.endpoint.local_addr().expect("A 地址");
        let b_addr = b.endpoint.local_addr().expect("B 地址");
        let a_cert = CertificateDer::from(a.cert.cert_der.clone());
        let b_cert = CertificateDer::from(b.cert.cert_der.clone());
        let a_ep = a.endpoint.clone();
        let a_self = a.cert.clone();
        let b_ep = b.endpoint.clone();
        let b_self = b.cert.clone();
        let ja = tokio::spawn(async move {
            punch(&a_ep, &a_self, &b_cert, &[b_addr], Duration::from_secs(5), Role::Controller).await
        });
        let jb = tokio::spawn(async move {
            punch(&b_ep, &b_self, &a_cert, &[a_addr], Duration::from_secs(5), Role::Controlled).await
        });
        let conn_a = ja.await.expect("A 任务").expect("A 连接");
        let conn_b = jb.await.expect("B 任务").expect("B 连接");

        // Controller 侧：open_bi + 写 Hello 首帧（解除对端 accept_bi）+ 一帧 Output，然后 finish。
        let writer = tokio::spawn(async move {
            let (mut send, _r) = conn_a.open_bi().await.expect("open_bi");
            let hello = RemoteFrame::P2pStreamHello.to_value().expect("hello value");
            assert!(write_value(&mut send, &hello).await, "写 Hello");
            let out = RemoteFrame::Output(b"hello-p2p".to_vec())
                .to_value()
                .expect("out value");
            assert!(write_value(&mut send, &out).await, "写 Output");
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(300)).await; // 保 conn_a 存活到对端读完
            drop(conn_a);
        });

        // Controlled 侧：accept_bi + read_pump 读到流结束。
        let (_sb, recv_b) = conn_b.accept_bi().await.expect("accept_bi");
        let (tx, rx) = std::sync::mpsc::channel();
        let dr = Arc::new(AtomicBool::new(true));
        read_pump(recv_b, &tx, &dr, &None).await;
        writer.await.expect("writer");

        let frames: Vec<serde_json::Value> = rx
            .try_iter()
            .filter_map(|e| match e {
                P2pEvent::DataFrame(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 2, "应收到 Hello + Output 两帧");
        assert_eq!(
            RemoteFrame::from_value(&frames[0]).expect("decode 0"),
            RemoteFrame::P2pStreamHello
        );
        assert_eq!(
            RemoteFrame::from_value(&frames[1]).expect("decode 1"),
            RemoteFrame::Output(b"hello-p2p".to_vec())
        );
        assert!(!dr.load(Ordering::Acquire), "流结束后 data_ready 应被清");
    }
}
