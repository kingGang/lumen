//! 终端分屏布局引擎（F5 / M3.7 固定均分；F7③ / M3.7c 升级可调比例）。
//!
//! 网格结构规则不变（海风哥拍板）：1=满屏、2=左右、3=左中右、
//! 4=上2下2、5=上3下2、6=上3下3，两排时上排在前（窗格顺序先上排自
//! 左向右、再下排）。比例状态 [`PaneLayout`]：每排高度权重 + 每排内
//! 各列宽度权重，挂在 Tab 上（见 session::Tab::layout）；窗格增删时
//! 由 main 重置均分（简单正确优先），随 sessions.json 持久化。
//!
//! 窗格之间留 [`PANE_GAP`] 间隙；分隔条（P11 并入 F7：可视 1px 细线
//! 与加宽命中区一体）落在间隙上（[`PaneLayout::dividers`]），拖动调
//! 相邻两格（列分隔）或两排（排分隔）的权重——拖动按**指针绝对位置**
//! 定边界（[`PaneLayout::drag_col_to`] / [`PaneLayout::drag_row_to`]，
//! 无逐帧增量的累积漂移），最小尺寸钳制 [`MIN_PANE_WIDTH`] 与
//! [`MIN_PANE_HEIGHT`]；双击恢复该方向均分。

/// 窗格间隙（逻辑像素）：分隔条的可视细线居中其内，命中区由 UI 层
/// 向两侧加宽（见 shell/mod.rs 的分隔条交互）。
pub const PANE_GAP: f32 = 2.0;

/// 拖动钳制的窗格最小宽度（逻辑像素，默认字号下约合 ≥13 列）。仅
/// 约束拖动：窗口本身过小时按权重比例缩小（均分也救不了，不在此兜）。
pub const MIN_PANE_WIDTH: f32 = 120.0;

/// 拖动钳制的窗格最小高度（逻辑像素，扣除 ~24px 窗格标题栏后约合
/// ≥3 行）。
pub const MIN_PANE_HEIGHT: f32 = 80.0;

/// 拖动的最小生效步长（逻辑像素）：低于此值视为未变化，避免亚像素
/// 抖动触发无谓的纹理重建与 resize。绝对定位下不存在累积丢失。
const DRAG_DEADZONE: f32 = 0.25;

/// 每排的列数（网格结构规则，上排在前）。n=0 空；n>6 防御性按 6
/// （调用方维护上限不变量，见 session::MAX_PANES）。
fn grid_rows(n: usize) -> &'static [usize] {
    match n {
        0 => &[],
        1 => &[1],
        2 => &[2],
        3 => &[3],
        4 => &[2, 2],
        5 => &[3, 2],
        _ => &[3, 3],
    }
}

/// 分隔条身份：拖动/双击动作经 ShellOutput 带回 main，再施加到
/// 激活 tab 的 [`PaneLayout`] 上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DividerKind {
    /// 第 idx 与 idx+1 排之间的横向分隔条（拖动调两排高度）。
    Row(usize),
    /// row 排内第 idx 与 idx+1 列之间的纵向分隔条（拖动调两列宽度）。
    Col { row: usize, idx: usize },
}

/// 一条分隔条：身份 + 所占间隙矩形（宽/高为 [`PANE_GAP`]；可视线
/// 居中其内、命中区由 UI 层加宽）。
pub struct Divider {
    pub kind: DividerKind,
    pub rect: egui::Rect,
}

/// 一个 tab 的窗格比例布局：每排高度权重 + 每排内各列宽度权重。
///
/// 不变量（构造与变更路径维护）：形状与窗格数的网格结构一致
/// （[`grid_rows`]），权重有限、为正、每组归一化和为 1（拖动保持
/// 相邻两项之和不变，故总和不漂移）。
#[derive(Debug, Clone, PartialEq)]
pub struct PaneLayout {
    /// 每排高度权重（长度 = 排数）。
    row_weights: Vec<f32>,
    /// 每排内各列宽度权重（外层长度 = 排数，内层 = 该排列数）。
    col_weights: Vec<Vec<f32>>,
}

impl PaneLayout {
    /// n 个窗格的均分布局（新建/增删窗格的重置值）。
    pub fn uniform(n: usize) -> Self {
        let shape = grid_rows(n);
        let nrows = shape.len();
        let row_w = if nrows == 0 { 0.0 } else { 1.0 / nrows as f32 };
        Self {
            row_weights: vec![row_w; nrows],
            col_weights: shape.iter().map(|&c| vec![1.0 / c as f32; c]).collect(),
        }
    }

    /// 从持久化权重还原（F7 持久化）：形状须与 n 个窗格的网格结构
    /// 一致且所有权重有限、为正，否则返回 None（调用方回退均分——
    /// 旧 v2 文件无权重字段、恢复时窗格 spawn 失败导致数量变化、
    /// 手改文件的非法值都走这条降级路）。合法权重按组归一化。
    pub fn from_weights(n: usize, rows: &[f32], cols: &[Vec<f32>]) -> Option<Self> {
        if n == 0 || n > 6 {
            return None;
        }
        let shape = grid_rows(n);
        if rows.len() != shape.len() || cols.len() != shape.len() {
            return None;
        }
        if cols.iter().zip(shape).any(|(c, &expect)| c.len() != expect) {
            return None;
        }
        let valid = |w: &f32| w.is_finite() && *w > 0.0;
        if !rows.iter().all(valid) || !cols.iter().flatten().all(valid) {
            return None;
        }
        let norm = |v: &[f32]| -> Vec<f32> {
            let s: f32 = v.iter().sum();
            v.iter().map(|w| w / s).collect()
        };
        Some(Self {
            row_weights: norm(rows),
            col_weights: cols.iter().map(|c| norm(c)).collect(),
        })
    }

    /// 排高权重（持久化快照取值）。
    pub fn row_weights(&self) -> &[f32] {
        &self.row_weights
    }

    /// 列宽权重（持久化快照取值）。
    pub fn col_weights(&self) -> &[Vec<f32>] {
        &self.col_weights
    }

    /// 本布局对应的窗格数（shell 侧与传入的窗格列表对照防御）。
    pub fn pane_count(&self) -> usize {
        self.col_weights.iter().map(Vec::len).sum()
    }

    /// 计算各窗格在 `area` 内的矩形（egui 逻辑点坐标，未做像素对齐
    /// ——调用方按 DPI round_to_pixels）。顺序：先上排自左向右、再
    /// 下排。按权重切分，窗格间留 [`PANE_GAP`]；边界用前缀和计算且
    /// 末段强制贴齐区域边缘（浮点误差不外漏）。
    pub fn pane_rects(&self, area: egui::Rect) -> Vec<egui::Rect> {
        let rows = split(area.height(), &self.row_weights, area.min.y);
        let mut out = Vec::with_capacity(self.pane_count());
        for ((y, h), ws) in rows.iter().zip(&self.col_weights) {
            for (x, w) in split(area.width(), ws, area.min.x) {
                out.push(egui::Rect::from_min_size(
                    egui::pos2(x, *y),
                    egui::vec2(w, *h),
                ));
            }
        }
        out
    }

    /// 各分隔条的间隙矩形：每排内列与列之间（纵向，高 = 该排高），
    /// 以及排与排之间（横向，横贯整个区域宽）。
    pub fn dividers(&self, area: egui::Rect) -> Vec<Divider> {
        let rows = split(area.height(), &self.row_weights, area.min.y);
        let mut out = Vec::new();
        for (r, ((y, h), ws)) in rows.iter().zip(&self.col_weights).enumerate() {
            let cols = split(area.width(), ws, area.min.x);
            for (c, (x, w)) in cols.iter().take(cols.len().saturating_sub(1)).enumerate() {
                out.push(Divider {
                    kind: DividerKind::Col { row: r, idx: c },
                    rect: egui::Rect::from_min_size(
                        egui::pos2(x + w, *y),
                        egui::vec2(PANE_GAP, *h),
                    ),
                });
            }
            if r + 1 < rows.len() {
                out.push(Divider {
                    kind: DividerKind::Row(r),
                    rect: egui::Rect::from_min_size(
                        egui::pos2(area.min.x, y + h),
                        egui::vec2(area.width(), PANE_GAP),
                    ),
                });
            }
        }
        out
    }

    /// 把 `row` 排内第 idx/idx+1 列之间的分隔条中心拖到绝对横坐标
    /// `x`（逻辑点）：相邻两列此消彼长、其余列不动；两列各不小于
    /// [`MIN_PANE_WIDTH`]（两列合计不足双倍最小宽时冻结不动）。
    /// 返回是否发生了变化（亚像素目标视为未变化）。下标越界返回
    /// false（防御：结构刚变更的过渡帧）。
    pub fn drag_col_to(&mut self, row: usize, idx: usize, x: f32, area: egui::Rect) -> bool {
        let Some(ws) = self.col_weights.get(row) else {
            return false;
        };
        if idx + 1 >= ws.len() {
            return false;
        }
        let segs = split(area.width(), ws, area.min.x);
        let (xi, wi) = segs[idx];
        let pair = wi + segs[idx + 1].1;
        // 分隔条中心 x → 左列新宽（间隙的一半在左列右缘之外）。
        let desired = x - PANE_GAP / 2.0 - xi;
        let Some(new_wi) = clamp_pair(desired, wi, pair, MIN_PANE_WIDTH) else {
            return false;
        };
        let avail = area.width() - PANE_GAP * (ws.len() as f32 - 1.0);
        let total: f32 = ws.iter().sum();
        let ws = &mut self.col_weights[row];
        ws[idx] = new_wi / avail * total;
        ws[idx + 1] = (pair - new_wi) / avail * total;
        true
    }

    /// 把第 idx/idx+1 排之间的分隔条中心拖到绝对纵坐标 `y`：相邻
    /// 两排此消彼长，各不小于 [`MIN_PANE_HEIGHT`]。语义同
    /// [`Self::drag_col_to`]。
    pub fn drag_row_to(&mut self, idx: usize, y: f32, area: egui::Rect) -> bool {
        if idx + 1 >= self.row_weights.len() {
            return false;
        }
        let segs = split(area.height(), &self.row_weights, area.min.y);
        let (yi, hi) = segs[idx];
        let pair = hi + segs[idx + 1].1;
        let desired = y - PANE_GAP / 2.0 - yi;
        let Some(new_hi) = clamp_pair(desired, hi, pair, MIN_PANE_HEIGHT) else {
            return false;
        };
        let avail = area.height() - PANE_GAP * (self.row_weights.len() as f32 - 1.0);
        let total: f32 = self.row_weights.iter().sum();
        self.row_weights[idx] = new_hi / avail * total;
        self.row_weights[idx + 1] = (pair - new_hi) / avail * total;
        true
    }

    /// 排高恢复均分（双击横向分隔条）。返回是否发生了变化。
    pub fn reset_rows(&mut self) -> bool {
        reset_uniform(&mut self.row_weights)
    }

    /// `row` 排的列宽恢复均分（双击该排内的纵向分隔条）。返回是否
    /// 发生了变化；下标越界返回 false。
    pub fn reset_cols(&mut self, row: usize) -> bool {
        match self.col_weights.get_mut(row) {
            Some(ws) => reset_uniform(ws),
            None => false,
        }
    }
}

/// 一维按权重切分：`extent` 总长（含段间 [`PANE_GAP`]）、`origin`
/// 起点，返回每段 (起点, 长度)。边界 = 前缀和占比 × 可用长 + 已经过
/// 的间隙；末段边界强制取可用长，保证末段尾缘精确贴齐
/// `origin + extent`（浮点累计误差不外漏）。
fn split(extent: f32, weights: &[f32], origin: f32) -> Vec<(f32, f32)> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let avail = (extent - PANE_GAP * (n as f32 - 1.0)).max(0.0);
    let total: f32 = weights.iter().sum();
    let mut out = Vec::with_capacity(n);
    let mut acc = 0.0f32;
    let mut start = origin;
    for (i, w) in weights.iter().enumerate() {
        acc += w;
        let end_content = if i + 1 == n {
            avail
        } else {
            avail * acc / total
        };
        let end = origin + end_content + PANE_GAP * i as f32;
        out.push((start, (end - start).max(0.0)));
        start = end + PANE_GAP;
    }
    out
}

/// 相邻两段调整的钳制：目标长 `desired` 夹在 [min, pair-min]，两段
/// 合计 `pair` 不足 2×min 时冻结（None）；与当前值差小于死区也视为
/// 未变化（None）。返回钳制后的新长度。
///
/// 特殊情况（B4/问题4 修复）：窗口最小化时 winit inner_size 缩为
/// 约 160×28 小条（非 0×0），导致 pair < 2*min 但 clamp(min, pair-min)
/// 中 pair-min < min，产生 max < min 的 f32::clamp panic。
/// 此处在 pair <= 2.0 * min 时返回 pair / 2.0（平均分，冻结语义），
/// 绕过 clamp 保证不 panic。
fn clamp_pair(desired: f32, current: f32, pair: f32, min: f32) -> Option<f32> {
    if !desired.is_finite() {
        return None;
    }
    // 两段合计不足双倍最小值时（含最小化小条场景 pair ~= 160 < 2×163）：
    // 不能正常钳制，冻结当前值（返回 None）。
    if pair <= 2.0 * min {
        return None;
    }
    let new = desired.clamp(min, pair - min);
    ((new - current).abs() >= DRAG_DEADZONE).then_some(new)
}

/// 权重组重置均分，返回是否发生了变化。
fn reset_uniform(ws: &mut [f32]) -> bool {
    let n = ws.len();
    if n == 0 {
        return false;
    }
    let u = 1.0 / n as f32;
    if ws.iter().all(|w| (w - u).abs() < 1e-6) {
        return false;
    }
    ws.fill(u);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试区域：x 0..304、y 0..202（宽度对 3 列、高度对 2 排都能
    /// 整除：列宽 (304-4)/3=100、(304-2)/2=151；排高 (202-2)/2=100）。
    fn area() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(304.0, 202.0))
    }

    fn assert_rect(r: egui::Rect, x: f32, y: f32, w: f32, h: f32) {
        let eps = 0.01;
        assert!(
            (r.min.x - x).abs() < eps
                && (r.min.y - y).abs() < eps
                && (r.width() - w).abs() < eps
                && (r.height() - h).abs() < eps,
            "矩形不符：得到 {r:?}，期望 min=({x},{y}) size=({w},{h})"
        );
    }

    #[test]
    fn 零与超限() {
        assert!(PaneLayout::uniform(0).pane_rects(area()).is_empty());
        // n>6 防御性按 6 计算（调用方维护上限）。
        assert_eq!(PaneLayout::uniform(9).pane_rects(area()).len(), 6);
        assert_eq!(PaneLayout::uniform(9).pane_count(), 6);
    }

    #[test]
    fn 均分一格满屏() {
        let r = PaneLayout::uniform(1).pane_rects(area());
        assert_eq!(r.len(), 1);
        assert_rect(r[0], 0.0, 0.0, 304.0, 202.0);
    }

    #[test]
    fn 均分两格左右() {
        let r = PaneLayout::uniform(2).pane_rects(area());
        assert_eq!(r.len(), 2);
        assert_rect(r[0], 0.0, 0.0, 151.0, 202.0);
        assert_rect(r[1], 153.0, 0.0, 151.0, 202.0);
    }

    #[test]
    fn 均分三格左中右() {
        let r = PaneLayout::uniform(3).pane_rects(area());
        assert_eq!(r.len(), 3);
        assert_rect(r[0], 0.0, 0.0, 100.0, 202.0);
        assert_rect(r[1], 102.0, 0.0, 100.0, 202.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 202.0);
    }

    #[test]
    fn 均分四格上2下2() {
        let r = PaneLayout::uniform(4).pane_rects(area());
        assert_eq!(r.len(), 4);
        // 上排在前：0/1 上排，2/3 下排。
        assert_rect(r[0], 0.0, 0.0, 151.0, 100.0);
        assert_rect(r[1], 153.0, 0.0, 151.0, 100.0);
        assert_rect(r[2], 0.0, 102.0, 151.0, 100.0);
        assert_rect(r[3], 153.0, 102.0, 151.0, 100.0);
    }

    #[test]
    fn 均分五格上3下2() {
        let r = PaneLayout::uniform(5).pane_rects(area());
        assert_eq!(r.len(), 5);
        // 上排 3 个窄列。
        assert_rect(r[0], 0.0, 0.0, 100.0, 100.0);
        assert_rect(r[1], 102.0, 0.0, 100.0, 100.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 100.0);
        // 下排 2 个宽列。
        assert_rect(r[3], 0.0, 102.0, 151.0, 100.0);
        assert_rect(r[4], 153.0, 102.0, 151.0, 100.0);
    }

    #[test]
    fn 均分六格上3下3() {
        let r = PaneLayout::uniform(6).pane_rects(area());
        assert_eq!(r.len(), 6);
        assert_rect(r[0], 0.0, 0.0, 100.0, 100.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 100.0);
        assert_rect(r[3], 0.0, 102.0, 100.0, 100.0);
        assert_rect(r[5], 204.0, 102.0, 100.0, 100.0);
    }

    #[test]
    fn 权重切分两列一比三() {
        // 列宽权重 1:3：可用宽 302 → 75.5/226.5。
        let l = PaneLayout {
            row_weights: vec![1.0],
            col_weights: vec![vec![0.25, 0.75]],
        };
        let r = l.pane_rects(area());
        assert_rect(r[0], 0.0, 0.0, 75.5, 202.0);
        assert_rect(r[1], 77.5, 0.0, 226.5, 202.0);
    }

    #[test]
    fn 权重切分排高与末格贴齐() {
        // 排高权重 3:7（可用高 200 → 60/140），列均分；末格右下角
        // 必须精确贴齐区域边缘（前缀和 + 末段强制贴齐）。
        let a = egui::Rect::from_min_size(egui::pos2(180.0, 36.0), egui::vec2(304.0, 202.0));
        let l = PaneLayout {
            row_weights: vec![0.3, 0.7],
            col_weights: vec![vec![0.5, 0.5], vec![0.5, 0.5]],
        };
        let r = l.pane_rects(a);
        assert_eq!(r.len(), 4);
        assert_rect(r[0], 180.0, 36.0, 151.0, 60.0);
        assert_rect(r[2], 180.0, 98.0, 151.0, 140.0);
        for rect in &r {
            assert!(a.contains_rect(*rect), "窗格 {rect:?} 超出区域 {a:?}");
        }
        let last = r[3];
        assert!(
            (last.max.x - a.max.x).abs() < 0.01 && (last.max.y - a.max.y).abs() < 0.01,
            "末格右下角 {last:?} 未贴齐区域 {a:?}"
        );
    }

    #[test]
    fn 区域偏移与边界() {
        // 非零原点（真实终端区在侧栏/顶栏右下方）：矩形跟随原点，
        // 且全部窗格都落在区域内、最后一格右下角贴齐区域边界。
        let a = egui::Rect::from_min_size(egui::pos2(180.0, 36.0), egui::vec2(304.0, 202.0));
        for n in 1..=6 {
            let rects = PaneLayout::uniform(n).pane_rects(a);
            assert_eq!(rects.len(), n);
            for r in &rects {
                assert!(a.contains_rect(*r), "n={n} 窗格 {r:?} 超出区域 {a:?}");
            }
            let last = rects[rects.len() - 1];
            assert!(
                (last.max.x - a.max.x).abs() < 0.01 && (last.max.y - a.max.y).abs() < 0.01,
                "n={n} 末格右下角 {last:?} 未贴齐区域 {a:?}"
            );
        }
    }

    #[test]
    fn from_weights_合法归一化与非法回退() {
        // 旧 v2 无权重字段（空向量）→ None（调用方回退均分）。
        assert!(PaneLayout::from_weights(2, &[], &[]).is_none());
        // 形状不符：排数 / 某排列数与网格规则不一致。
        assert!(PaneLayout::from_weights(4, &[1.0], &[vec![1.0, 1.0]]).is_none());
        assert!(PaneLayout::from_weights(2, &[1.0], &[vec![1.0, 1.0, 1.0]]).is_none());
        // 非法值：NaN / 0 / 负数。
        assert!(PaneLayout::from_weights(2, &[1.0], &[vec![f32::NAN, 1.0]]).is_none());
        assert!(PaneLayout::from_weights(2, &[1.0], &[vec![0.0, 1.0]]).is_none());
        assert!(PaneLayout::from_weights(2, &[-1.0], &[vec![1.0, 1.0]]).is_none());
        // n 越界。
        assert!(PaneLayout::from_weights(0, &[], &[]).is_none());
        assert!(PaneLayout::from_weights(7, &[1.0, 1.0], &[vec![1.0; 3], vec![1.0; 3]]).is_none());
        // 合法：按组归一化（2:6 → 0.25:0.75）。
        let l = PaneLayout::from_weights(2, &[2.0], &[vec![2.0, 6.0]]).expect("合法权重");
        assert!((l.row_weights()[0] - 1.0).abs() < 1e-6);
        assert!((l.col_weights()[0][0] - 0.25).abs() < 1e-6);
        assert!((l.col_weights()[0][1] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn 拖动列分隔并钳制最小宽() {
        let mut l = PaneLayout::uniform(2);
        // 往左拖到 x=100：左列目标宽 99 < 最小 120 → 钳到 120。
        assert!(l.drag_col_to(0, 0, 100.0, area()));
        let r = l.pane_rects(area());
        assert_rect(r[0], 0.0, 0.0, 120.0, 202.0);
        assert_rect(r[1], 122.0, 0.0, 182.0, 202.0);
        // 往右拖到尽头：右列钳到最小 120。
        assert!(l.drag_col_to(0, 0, 300.0, area()));
        let r = l.pane_rects(area());
        assert_rect(r[0], 0.0, 0.0, 182.0, 202.0);
        assert_rect(r[1], 184.0, 0.0, 120.0, 202.0);
        // 拖到当前位置（亚像素差）：视为未变化。
        let center = r[0].max.x + PANE_GAP / 2.0;
        assert!(!l.drag_col_to(0, 0, center, area()));
    }

    #[test]
    fn 拖动排分隔并钳制最小高() {
        let mut l = PaneLayout::uniform(4);
        // 往下拖到 y=190：上排目标高 189 > 可用 200-80 → 钳到 120。
        assert!(l.drag_row_to(0, 190.0, area()));
        let r = l.pane_rects(area());
        assert_rect(r[0], 0.0, 0.0, 151.0, 120.0);
        assert_rect(r[2], 0.0, 122.0, 151.0, 80.0);
        // 列宽不受排分隔影响。
        assert_rect(r[3], 153.0, 122.0, 151.0, 80.0);
    }

    #[test]
    fn 空间不足时拖动冻结() {
        // 区域宽 240：两列可用 238 < 2×120，拖动不生效、权重不变。
        let small = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(240.0, 202.0));
        let mut l = PaneLayout::uniform(2);
        let before = l.clone();
        assert!(!l.drag_col_to(0, 0, 30.0, small));
        assert_eq!(l, before);
    }

    #[test]
    fn 最小化小条场景不panic() {
        // 问题4（B4）：无边框窗口最小化时 winit 给出约 160×28 的小条，
        // 两列 pair ≈ 160 < 2×163 = 326，原 clamp(min, pair-min) 中
        // pair-min=160-163=-3 < min=163，f32::clamp panic。
        // 修复后 clamp_pair 直接返回 None（冻结），不 panic。
        let minimized = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(160.0, 28.0));
        let mut l = PaneLayout::uniform(2);
        let before = l.clone();
        // 拖动不生效，权重不变，且不 panic。
        assert!(!l.drag_col_to(0, 0, 80.0, minimized));
        assert_eq!(l, before);
        // pair 恰好 = 2*min 边界情况：240/2=120=min，pair 正好不足，也冻结。
        let edge = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(242.0, 28.0));
        assert!(!l.drag_col_to(0, 0, 80.0, edge));
        // 正常大小窗口：仍可拖动。
        assert!(l.drag_col_to(0, 0, 100.0, area()));
    }

    #[test]
    fn 双击恢复均分() {
        let mut l = PaneLayout::uniform(4);
        assert!(l.drag_col_to(0, 0, 100.0, area()));
        assert!(l.drag_row_to(0, 190.0, area()));
        // 列向恢复均分只影响该排；排向恢复均分只影响排高。
        assert!(l.reset_cols(0));
        assert!(l.reset_rows());
        assert_eq!(l, PaneLayout::uniform(4));
        // 已均分再双击：无变化。
        assert!(!l.reset_cols(0));
        assert!(!l.reset_rows());
    }

    #[test]
    fn 增删重置均分() {
        // 增删窗格的重置语义 = 换一个 uniform(n)：形状与权重全部归位
        // （main 在 new_pane/close_pane 中执行）。
        let l = PaneLayout::uniform(5);
        assert_eq!(l.row_weights.len(), 2);
        assert_eq!(l.col_weights[0].len(), 3);
        assert_eq!(l.col_weights[1].len(), 2);
        for w in &l.row_weights {
            assert!((w - 0.5).abs() < 1e-6);
        }
        for w in &l.col_weights[1] {
            assert!((w - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn 分隔条几何与数量() {
        // n=5（上3下2）：上排 2 条列分隔 + 下排 1 条 + 1 条排分隔 = 4。
        let divs = PaneLayout::uniform(5).dividers(area());
        assert_eq!(divs.len(), 4);
        // 上排第一条列分隔：x∈[100,102]、高 = 上排高。
        let d0 = &divs[0];
        assert_eq!(d0.kind, DividerKind::Col { row: 0, idx: 0 });
        assert_rect(d0.rect, 100.0, 0.0, PANE_GAP, 100.0);
        // 排分隔横贯整宽：y∈[100,102]。
        let drow = divs
            .iter()
            .find(|d| d.kind == DividerKind::Row(0))
            .expect("应有排分隔");
        assert_rect(drow.rect, 0.0, 100.0, 304.0, PANE_GAP);
        // 下排列分隔在下排坐标系：x∈[151,153]、y 从下排起。
        let d_bottom = divs
            .iter()
            .find(|d| d.kind == DividerKind::Col { row: 1, idx: 0 })
            .expect("应有下排列分隔");
        assert_rect(d_bottom.rect, 151.0, 102.0, PANE_GAP, 100.0);
        // 单格无分隔条。
        assert!(PaneLayout::uniform(1).dividers(area()).is_empty());
    }

    #[test]
    fn 拖动越界下标不崩() {
        let mut l = PaneLayout::uniform(2);
        let before = l.clone();
        assert!(!l.drag_col_to(5, 0, 100.0, area()));
        assert!(!l.drag_col_to(0, 5, 100.0, area()));
        assert!(!l.drag_row_to(0, 100.0, area())); // 单排无排分隔
        assert!(!l.reset_cols(9));
        assert_eq!(l, before);
    }
}
