//! M5.3 终端远程：WebSocket 长连接经 `lumen-server` 中继的「控制面」协议。
//!
//! 与 M5.1/M5.2 的 REST DTO 不同，本模块是**双向消息流**：客户端与服务端
//! 各自维护一条 WebSocket（路径 [`crate::routes::WS`]），消息以 JSON 文本帧
//! 收发。两类角色：
//!
//! - **控制端**（controller）：选中在线设备发起 [`RemoteC2S::RequestControl`]，
//!   输入被控端展示的 9 位配对码 [`RemoteC2S::SubmitPairing`]，配对成功后镜像
//!   并操控对端。
//! - **被控端**（controlled）：收到 [`RemoteS2C::ControlRequested`] 后醒目展示
//!   配对码，可 [`RemoteC2S::DeclineControl`] 拒绝；被控期间把状态增量经
//!   [`RemoteC2S::Relay`] 推给控制端，并执行控制端发来的操作指令。
//!
//! # 控制面 vs 数据面（可扩展性铁律）
//! 服务端**只解释控制面消息**（请求 / 配对 / 独占仲裁 / 会话收尾）；数据面
//! [`RemoteC2S::Relay`] / [`RemoteS2C::Relay`] 的载荷是**不透明 [`serde_json::Value`]**，
//! 服务端原样转发给会话对端、绝不反序列化其内部结构。客户端两端共享
//! [`RemoteFrame`] 类型做强类型收发（[`RemoteFrame::to_value`] /
//! [`RemoteFrame::from_value`]）。如此 M5.3 part2/3 给 [`RemoteFrame`] 增加
//! 状态增量（grid delta / block / 布局）与操作指令（Action）变体时，**服务端
//! 中继代码零改动**——这是分期推进不返工的关键。
//!
//! # 安全要点（part1 已落实，详见 `docs/M5远程控制设计.md` §6）
//! - WS 升级走 `Authorization: Bearer` 头鉴权（复用 REST 的 JWT），不走 query
//!   参数，避免反代日志泄漏 token。
//! - 配对码仅下发给**被控端**展示；控制端凭人工转述输入，服务端校验。
//! - [`RemoteC2S::SubmitPairing`] 由服务端校验「提交者 == 发起请求的 device_id」
//!   （取自连接的鉴权身份，非消息自报），杜绝抢答 / 重放 / 跨用户接管。

use serde::{Deserialize, Serialize};

/// 会话中本端扮演的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// 控制端（镜像并操控对端）。
    Controller,
    /// 被控端（被镜像、执行远程指令）。
    Controlled,
}

/// 控制请求被拒 / 取消的机器可读原因（客户端按此本地化提示，不靠字符串匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// 目标设备当前不在线（无活跃 WS 连接）。
    Offline,
    /// 目标设备已被其他控制端独占。
    AlreadyControlled,
    /// 控制端自己已在控制另一台设备（控制端同一时刻只控一台）。
    ControllerBusy,
    /// 目标设备正在与他人配对中（已有未决配对请求）。
    TargetPairing,
    /// 目标设备属于其它账户（禁止跨账户控制）。
    CrossUser,
    /// 不能控制自己。
    SelfControl,
    /// 被控端用户主动拒绝。
    RejectedByUser,
    /// 控制端在配对完成前断线 / 撤销。
    ControllerLeft,
    /// 配对超时（被控端未在有效期内完成）。
    Expired,
    /// 配对码连续输错次数超限。
    TooManyAttempts,
}

/// 配对码校验失败的机器可读原因（[`RemoteS2C::PairingResult`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingFailReason {
    /// 配对码错误（还有剩余尝试次数）。
    InvalidCode,
    /// 配对已超时失效。
    Expired,
    /// 无对应的未决配对（可能已被取消 / 完成 / 从未发起）。
    NoPending,
    /// 错误次数超限，配对作废。
    TooManyAttempts,
}

/// 会话结束的机器可读原因（[`RemoteS2C::SessionEnded`] 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndReason {
    /// 对端主动结束会话（[`RemoteC2S::EndSession`]）。
    PeerLeft,
    /// 对端断线。
    PeerDisconnected,
    /// 本设备在别处重新登录，当前连接被新连接取代。
    Replaced,
}

/// 数据面帧（控制端↔被控端，经服务端**盲转**；服务端不解释内容）。
///
/// part1 仅含 [`RemoteFrame::Echo`] 占位；**part3a 终端镜像**采用「VT 字节流转发」
/// 方案：被控端把焦点窗格 PTY 输出（含初始整屏快照重放）转发给控制端，控制端喂入
/// 一个无 PTY 的镜像 `Terminal::advance` 复现整状态（颜色/光标/标题/cwd/命令块全在
/// 字节流的 SGR/OSC 里，自动还原）。故数据面**只传字节**，无需把结构化 Cell 上线，
/// `lumen-protocol` 保持零依赖。后续 part3c（多窗格/布局）再加变体，**服务端中继零改**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteFrame {
    /// 回环测试帧：原样转发给对端，用于 part1 验证中继通路连通。
    Echo(String),
    /// 被控端 → 控制端：焦点窗格 PTY 输出字节（会话起始的整屏快照重放 + 实时增量，
    /// 同一通道）。控制端逐帧 `mirror.advance(&bytes)`。
    Output(Vec<u8>),
    /// 被控端 → 控制端：镜像终端尺寸（行/列）。会话起始与被控端窗格 resize 时发；
    /// 控制端据此 `mirror.resize(rows, cols)`，**必须在该尺寸的 `Output` 之前到达**。
    Resize {
        /// 行数。
        rows: u16,
        /// 列数。
        cols: u16,
    },
}

impl RemoteFrame {
    /// 序列化为不透明 JSON 值，包进 [`RemoteC2S::Relay`] / [`RemoteS2C::Relay`]。
    ///
    /// # Errors
    /// 序列化失败（理论上不会发生，除非自定义 `Serialize` 出错）时返回 serde 错误。
    pub fn to_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// 从 [`RemoteS2C::Relay`] 收到的不透明 JSON 值还原强类型帧。
    ///
    /// # Errors
    /// 当 JSON 不是本版本认识的 [`RemoteFrame`] 变体时返回 serde 错误——调用方
    /// 应 `warn` 后丢弃该帧（前向兼容：新版本对端可能发来本端未知的变体）。
    pub fn from_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
    }
}

/// 客户端 → 服务端的远程控制消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteC2S {
    /// 控制端发起：请求控制 `target` 设备（同账户、在线、未被占用）。
    RequestControl {
        /// 目标（被控端）设备 id。
        target: String,
    },
    /// 控制端提交被控端展示的配对码。
    SubmitPairing {
        /// 目标设备 id（确定提交针对哪个未决配对）。
        target: String,
        /// 用户输入的 9 位配对码。
        code: String,
    },
    /// 被控端拒绝当前未决的控制请求（dismiss 配对码并通知控制端）。
    DeclineControl,
    /// 任一端主动结束当前活跃会话。
    EndSession,
    /// 数据面：把不透明帧盲转给会话对端（载荷由 [`RemoteFrame`] 序列化而来）。
    Relay(serde_json::Value),
    /// 应用层心跳：保活 + 刷新在线状态，服务端回 [`RemoteS2C::Pong`]。
    Ping,
}

/// 服务端 → 客户端的远程控制消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteS2C {
    /// 连接建立后立即下发：协议版本协商 + 确认本设备 id。
    Welcome {
        /// 服务端当前协议版本。
        protocol_version: u32,
        /// 服务端仍兼容的最低客户端协议版本（低于此应提示用户升级）。
        min_supported_version: u32,
        /// 服务端据 JWT 确认的本设备 id。
        device_id: String,
    },
    /// 发给**被控端**：有控制端请求控制你，展示配对码供其转述输入。
    ControlRequested {
        /// 控制端设备 id。
        controller_device_id: String,
        /// 控制端设备显示名。
        controller_name: String,
        /// 9 位配对码（被控端醒目展示）。
        pairing_code: String,
        /// 配对有效期（秒），UI 可倒计时。
        expires_in_secs: u32,
    },
    /// 发给**控制端**：请求已送达被控端，请输入其展示的配对码。
    PairingNeeded {
        /// 目标（被控端）设备 id。
        target_device_id: String,
        /// 目标设备显示名。
        target_name: String,
        /// 配对有效期（秒）。
        expires_in_secs: u32,
    },
    /// 发给**控制端**：配对码校验结果（仅在失败时下发；成功走 [`Self::SessionStarted`]）。
    PairingResult {
        /// 失败原因。
        reason: PairingFailReason,
        /// 剩余尝试次数（0 表示配对已作废）。
        attempts_left: u32,
    },
    /// 发给**控制端**：控制请求被拒（同步失败 / 被控端拒绝 / 取消）。
    ControlDenied {
        /// 目标设备 id。
        target_device_id: String,
        /// 机器可读原因。
        reason: DenyReason,
    },
    /// 发给**被控端**：未决配对被取消（控制端撤销 / 超时），dismiss 配对码。
    PairingCancelled {
        /// 机器可读原因。
        reason: DenyReason,
    },
    /// 会话已建立（双方各收一份，含对端信息与本端角色）。
    SessionStarted {
        /// 对端设备 id。
        peer_device_id: String,
        /// 对端设备显示名。
        peer_name: String,
        /// 本端角色。
        role: Role,
    },
    /// 会话已结束（双方各收一份）。
    SessionEnded {
        /// 机器可读原因。
        reason: EndReason,
    },
    /// 数据面：来自会话对端的不透明帧（用 [`RemoteFrame::from_value`] 还原）。
    Relay(serde_json::Value),
    /// 心跳应答。
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c2s_往返() {
        let msg = RemoteC2S::SubmitPairing {
            target: "dev-2".into(),
            code: "123456789".into(),
        };
        let json = serde_json::to_string(&msg).expect("序列化");
        let back: RemoteC2S = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, msg);
    }

    #[test]
    fn s2c_往返带枚举原因() {
        let msg = RemoteS2C::ControlDenied {
            target_device_id: "dev-2".into(),
            reason: DenyReason::AlreadyControlled,
        };
        let json = serde_json::to_string(&msg).expect("序列化");
        let back: RemoteS2C = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, msg);
    }

    #[test]
    fn remoteframe_经value往返() {
        // 数据面：RemoteFrame → Value → 包进 Relay → 解包 → 还原 RemoteFrame。
        let frame = RemoteFrame::Echo("hello".into());
        let value = frame.to_value().expect("转 value");
        let c2s = RemoteC2S::Relay(value.clone());
        // 模拟服务端盲转：原样取出 value，无需认识 RemoteFrame。
        let RemoteC2S::Relay(relayed) = c2s else {
            panic!("应为 Relay");
        };
        let back = RemoteFrame::from_value(&relayed).expect("还原");
        assert_eq!(back, frame);
    }

    #[test]
    fn 镜像帧经value往返() {
        for frame in [
            RemoteFrame::Output(vec![0x1b, b'[', b'2', b'J']),
            RemoteFrame::Resize { rows: 40, cols: 120 },
        ] {
            let v = frame.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&v).expect("from_value");
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn 服务端盲转无需识别帧内部() {
        // 关键不变量：构造一个本版本「未知」的帧 JSON（模拟未来 part2/3 的变体），
        // 服务端只需搬运 Value、不反序列化即可转发；客户端旧版本 from_value 才报错。
        let future = serde_json::json!({ "FutureVariant": { "x": 1 } });
        let c2s = RemoteC2S::Relay(future.clone());
        let json = serde_json::to_string(&c2s).expect("序列化");
        // 服务端能完整解析外层 RemoteC2S（不因内层未知而失败）。
        let parsed: RemoteC2S = serde_json::from_str(&json).expect("外层可解析");
        let RemoteC2S::Relay(v) = parsed else {
            panic!("应为 Relay");
        };
        assert_eq!(v, future);
        // 旧版本客户端尝试强类型还原才会失败（应 warn 后丢弃）。
        assert!(RemoteFrame::from_value(&v).is_err());
    }
}
