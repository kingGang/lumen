//! Block（命令块）模型：Blocks 特性的数据基础。
//!
//! 通过 shell integration 的 OSC 133 序列采集命令边界：
//! `A` 提示符开始、`B` 命令输入开始、`C` 命令输出开始、`D;<exit>` 命令结束。
//! 行号为「绝对行号」（含已滚出可视区的历史），跨滚动稳定。
//! M1 只采集数据，块折叠/跳转 UI 在 M2 实现。

/// 一个命令块。各阶段行号在对应 OSC 133 标记到达时填充。
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// 稳定标识（自增，跨块丢弃不变）。
    pub id: u64,
    /// 提示符首行（OSC 133;A）。
    pub prompt_line: u64,
    /// 命令输入首行（OSC 133;B）。
    pub cmd_line: Option<u64>,
    /// 输出首行（OSC 133;C）。
    pub output_line: Option<u64>,
    /// 块结束行（OSC 133;D）。
    pub end_line: Option<u64>,
    /// D 标记时刻的光标列：>0 说明最后一行输出无结尾换行（新提示符
    /// 接在它后面），提取输出时该行 [0, end_col) 前缀属于本块。
    pub end_col: usize,
    /// 命令退出码（OSC 133;D;<code>）。
    pub exit_code: Option<i32>,
}

impl Block {
    /// 块是否已完整结束。
    pub fn is_closed(&self) -> bool {
        self.end_line.is_some()
    }
}
