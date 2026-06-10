//! Lumen 的终端状态机：VT 解析、Grid、scrollback、Block 边界采集。
//!
//! 本 crate 是纯数据模型，**不依赖任何图形库**，可独立单测。
//! 数据流：PTY 字节 → [`Terminal::advance`] → [`Grid`] 更新 → 渲染层读取。

mod block;
mod cell;
mod grid;
mod selection;
mod term;

pub use block::Block;
pub use cell::{Cell, CellFlags, Color};
pub use grid::{Cursor, Grid, Row};
pub use selection::{SelPoint, Selection};
pub use term::Terminal;
