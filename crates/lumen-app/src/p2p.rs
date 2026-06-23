//! M6 P2P 直连 · Phase 1：tokio 隔离骨架 + STUN 端点发现 + QUIC/证书就位。
//!
//! 设计见 `docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md`。本模块是「QUIC 打洞 + 中继回退」的
//! **客户端传输地基**，与主线程的同步 tungstenite（`remote_ws.rs`）范式**隔离**：一条 P2P 后台
//! 线程内起 **current-thread tokio runtime** 驱动 quinn / STUN（tokio 关在线程内，主线程零感知）。
//!
//! # 线程模型（与 `remote_ws.rs` 对称：后台线程 + channel）
//! - 主线程 → P2P 线程：`P2pCmd`（tokio unbounded channel，`send` 同步、主线程非 async 可调）。
//! - P2P 线程 → 主线程：`P2pEvent`（std mpsc，主线程每帧 [`P2pEngine::poll`] 非阻塞排空）。
//! - `ready: Arc<AtomicBool>`：直连数据面就绪标志（Phase 3 握手成功置位，主线程 `send_frame` 选路读）。
//!   Phase 1 恒 false。
//!
//! # Phase 1 范围（骨架空跑，**未接** `remote_ws`——Phase 2/3 接）
//! ① tokio 隔离线程 + quinn client `Endpoint` 在 runtime 内创建（验证「tokio 隔离 + quinn 可活」）；
//! ② STUN binding 客户端（RFC 5389）探公网映射端点；③ 本地 LAN 候选枚举；④ rcgen 自签证书生成
//! （Phase 2 握手 + 指纹信任锚就位）。打洞 / 候选交换 / 握手 / 数据面切换是 Phase 2–3。
//!
//! 因 Phase 1 尚未接入 `main`/`remote_ws`，本模块整体 `#![allow(dead_code)]`；Phase 2 接线后逐项移除。
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// QUIC ALPN 协议标识（两端必须一致；隔离非 lumen 的 QUIC 流量）。
const ALPN: &[u8] = b"lumen-p2p";
/// connect 时的 server name（pinned 验证器忽略此名、只认证书，故用固定占位 DNS 名）。
const SERVER_NAME: &str = "lumen-p2p";
/// 单次打洞握手总超时（所有候选 + accept 竞速；超时即视作打洞失败、回退中继）。
const PUNCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 开发期默认 STUN 服务器（公共）。**生产切自建 server UDP 反射**（国内可达性 + 自主可控，
/// 见设计 §7）。host:port 形式，运行期 DNS 解析（`tokio::net::lookup_host`）。
pub const DEFAULT_STUN: &str = "stun.l.google.com:19302";

/// STUN 单次探测超时（无应答即视作该服务器不可达，回退/换源）。
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

/// RFC 5389 magic cookie（固定常量，区分 STUN 与其他 UDP 流量、参与 XOR 编码）。
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// 一个候选端点（打洞时逐个尝试；经信令 `Offer`/`Answer` 与对端交换，见设计 §4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 候选地址（LAN：本机网卡地址；STUN：公网映射地址）。
    pub addr: SocketAddr,
    /// 来源类型。
    pub kind: CandidateKind,
}

/// [`Candidate`] 来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// 本机网卡的 LAN 地址（同子网 / VPN 直连用）。
    Local,
    /// STUN 反射得到的公网映射地址（跨 NAT 打洞用）。
    Stun,
}

/// 主线程 → P2P 线程的命令。
#[derive(Debug)]
enum P2pCmd {
    /// 收集本端候选端点（LAN + STUN 公网映射），结果经 [`P2pEvent::Candidates`] 回主线程。
    Discover,
    /// 优雅停机（亦可经 drop `cmd_tx` 触发 `recv()==None`）。
    Stop,
}

/// P2P 线程 → 主线程的事件（主线程 [`P2pEngine::poll`] 排空）。
#[derive(Debug, Clone)]
pub enum P2pEvent {
    /// 本端候选端点收集完成（可能只含 LAN——STUN 不可达时）。
    Candidates(Vec<Candidate>),
}

/// P2P 直连引擎句柄（主线程持有；与 `RemoteWs` 对称的启停 + poll 生命周期）。
pub struct P2pEngine {
    /// 主线程 → P2P 线程命令端。
    cmd_tx: UnboundedSender<P2pCmd>,
    /// P2P 线程 → 主线程事件端。
    evt_rx: Receiver<P2pEvent>,
    /// 直连数据面就绪标志（Phase 3 置位；主线程 `send_frame` 据此选路 P2P/中继）。
    ready: Arc<AtomicBool>,
    /// 停机标志（与 `Stop` 命令双保险）。
    stop: Arc<AtomicBool>,
    /// 后台线程句柄（stop / drop 时 join）。
    handle: Option<JoinHandle<()>>,
}

impl P2pEngine {
    /// 启动 P2P 后台线程（线程内建 current-thread tokio runtime）。`stun_host` 为端点发现用的
    /// STUN 服务器（host:port；可传 [`DEFAULT_STUN`]）。Phase 1 仅备引擎，**不自动接入数据面**。
    #[must_use]
    pub fn start(stun_host: String) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("lumen-p2p".into())
            .spawn(move || run(cmd_rx, &evt_tx, &stop_thread, &stun_host))
            .ok();
        Self {
            cmd_tx,
            evt_rx,
            ready,
            stop,
            handle,
        }
    }

    /// 请求收集本端候选端点（异步，结果经 [`Self::poll`] 取 [`P2pEvent::Candidates`]）。
    pub fn request_discovery(&self) {
        let _ = self.cmd_tx.send(P2pCmd::Discover);
    }

    /// 非阻塞排空 P2P 线程事件（主线程每帧调用）。
    pub fn poll(&self) -> Vec<P2pEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.evt_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// 直连数据面是否就绪（Phase 3 起有意义；Phase 1 恒 `false`）。
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// 优雅停机并 join 后台线程。
    pub fn stop(&mut self) {
        self.signal_stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// 置停机标志 + 投 `Stop`（唤醒阻塞在 `recv` 的线程）。
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(P2pCmd::Stop);
    }
}

impl Drop for P2pEngine {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// P2P 后台线程主体：建 current-thread runtime，`block_on` 异步主循环。
fn run(
    mut cmd_rx: UnboundedReceiver<P2pCmd>,
    evt_tx: &Sender<P2pEvent>,
    stop: &AtomicBool,
    stun_host: &str,
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
        // quinn client Endpoint 在 runtime 内创建——验证「tokio 隔离 + quinn 可活」（Phase 2 复用
        // 此 endpoint 发起 connect + 加 server 侧自签证书做打洞握手）。失败不致命：STUN 仍可探端点。
        match quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) {
            Ok(ep) => log::debug!("P2P quinn client endpoint 就绪 @ {:?}", ep.local_addr()),
            Err(e) => log::warn!("P2P quinn endpoint 创建失败: {e}"),
        }
        while !stop.load(Ordering::SeqCst) {
            match cmd_rx.recv().await {
                Some(P2pCmd::Discover) => {
                    let cands = collect_candidates(stun_host).await;
                    log::debug!("P2P 候选端点收集完成：{cands:?}");
                    let _ = evt_tx.send(P2pEvent::Candidates(cands));
                }
                Some(P2pCmd::Stop) | None => break,
            }
        }
    });
}

/// 收集本端候选端点：本机 LAN 地址 + STUN 反射的公网映射地址（STUN 不可达时仅返回 LAN）。
async fn collect_candidates(stun_host: &str) -> Vec<Candidate> {
    let mut cands = Vec::new();
    if let Some(ip) = local_lan_addr() {
        cands.push(Candidate {
            addr: SocketAddr::new(ip, 0),
            kind: CandidateKind::Local,
        });
    }
    if let Some(public) = discover_public_addr(stun_host, STUN_TIMEOUT).await {
        cands.push(Candidate {
            addr: public,
            kind: CandidateKind::Stun,
        });
    } else {
        log::info!("P2P STUN 未探到公网端点（{stun_host} 不可达 / 对称 NAT），仅 LAN 候选");
    }
    cands
}

/// 取本机出口网卡的 LAN 地址（connect-trick：UDP `connect` 到外部地址**不实际发包**，仅令内核选
/// 路由、`local_addr` 即出口网卡 IP）。零依赖、规避枚举全部网卡。失败返回 `None`。
fn local_lan_addr() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// 经 STUN 探本端公网映射端点（RFC 5389 Binding）：绑随机端口 UDP → 发 Binding Request →
/// 收 Binding Success Response → 解析 XOR-MAPPED-ADDRESS。超时 / 失败返回 `None`。
async fn discover_public_addr(stun_host: &str, timeout: Duration) -> Option<SocketAddr> {
    // 解析 STUN 服务器（取首个 IPv4）。
    let target = tokio::net::lookup_host(stun_host)
        .await
        .ok()?
        .find(SocketAddr::is_ipv4)?;
    let sock = tokio::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .await
        .ok()?;
    sock.connect(target).await.ok()?;
    let txn = new_txn_id();
    let req = build_binding_request(&txn);
    sock.send(&req).await.ok()?;
    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(timeout, sock.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    parse_xor_mapped_addr(&buf[..n], &txn)
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

    // 竞速：accept 任务 + 每候选一个 connect 任务，首个成功连接经 channel 胜出。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<quinn::Connection>(4);
    {
        let ep = endpoint.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                match incoming.accept() {
                    Ok(connecting) => {
                        if let Ok(conn) = connecting.await {
                            let _ = tx.send(conn).await;
                            break;
                        }
                    }
                    Err(e) => log::debug!("P2P accept 拒绝连入: {e}"),
                }
            }
        });
    }
    for cand in peer_candidates {
        match endpoint.connect_with(client_cfg.clone(), *cand, SERVER_NAME) {
            Ok(connecting) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = connecting.await {
                        let _ = tx.send(conn).await;
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
    Ok(quinn::ClientConfig::new(Arc::new(qc)))
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
    Ok(quinn::ServerConfig::with_crypto(Arc::new(qs)))
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
        let mut eng = P2pEngine::start(DEFAULT_STUN.to_string());
        assert!(!eng.is_ready(), "Phase 1 直连未就绪");
        eng.request_discovery(); // 不等待结果（可能联网慢）；仅验证投递 + 停机不 panic。
        eng.stop();
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

    /// 端到端：客户端 `discover_public_addr` 打本地 mock 反射，走完「构造请求→发→收→XOR 解析」
    /// 全链路，探到本机回环源端点（Phase 1 验收点「探公网端点」的离线可重复验证）。
    #[tokio::test]
    async fn discover_对本地mock反射_探到端点() {
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
        let got = discover_public_addr(&server_addr.to_string(), STUN_TIMEOUT).await;
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
            punch(&a_ep, &a_self, &b_cert, &[b_addr], Duration::from_secs(5))
                .await
                .is_some()
        });
        let jb = tokio::spawn(async move {
            punch(&b_ep, &b_self, &a_cert, &[a_addr], Duration::from_secs(5))
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
            let _ = punch(&b_ep, &b_self, &a_cert, &[], Duration::from_secs(2)).await;
        });
        let got = punch(&a.endpoint, &a.cert, &fake_cert, &[b_addr], Duration::from_secs(2)).await;
        assert!(got.is_none(), "对端证书不匹配应握手失败");
    }
}
