//! 选区模型：以「绝对行号」定位，滚动/新输出时选区跟随内容不漂移。

/// 选区端点：绝对行号（含全部历史）+ 列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelPoint {
    pub line: u64,
    pub col: usize,
}

/// 一个鼠标选区。`anchor` 是按下处，`head` 随拖动移动，方向不限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: SelPoint,
    pub head: SelPoint,
}

impl Selection {
    /// 返回按文档顺序排列的 (起点, 终点)。
    pub fn normalized(&self) -> (SelPoint, SelPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// 指定单元格是否落在选区内（含两端）。
    pub fn contains(&self, line: u64, col: usize) -> bool {
        let (s, e) = self.normalized();
        let p = SelPoint { line, col };
        s <= p && p <= e
    }

    /// 选区是否退化为单点（视为无内容）。
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}
