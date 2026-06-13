//! 命令历史库（M4.1 批D2，feature = "input-editor"）——设计稿 §8。
//!
//! # 存储格式
//! `%LOCALAPPDATA%/Lumen/history.jsonl`：每行一条 JSON 记录，追加写。
//! 条目格式：`{ text, cwd?, exit_code?, duration_ms?, ts }`。
//!
//! # 回填策略（内存回填 + 退出时原子重写）
//! - 提交时立即追加 `{ text, cwd, ts }` 到文件（exit_code 尚未知）；
//! - 块闭合时（OSC 133;D）在内存中回填 exit_code/duration_ms；
//! - 进程退出（CloseRequested）时原子重写（tmp → rename），去重、
//!   保留最近 10000 条、保留已知 exit_code 条目。
//! - 运行期文件只追加，偶有「未回填」条目属于正常情况，加载时同
//!   text+ts 去重取 exit_code 最新非 None 值。
//!
//! # PSReadLine 种子导入
//! 首次启动（history.jsonl 不存在）时一次性导入
//! `%APPDATA%\Microsoft\Windows\PowerShell\PSReadLine\ConsoleHost_history.txt`，
//! 取最近 5000 条，ts 用文件 mtime 兜底，失败静默降级（log info）。
//!
//! # 上限管控
//! 内存与重写时均保留最近 10000 条。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde::{Deserialize, Serialize};

/// 单条历史记录（JSONL 每行的结构体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 命令文本。
    pub text: String,
    /// 提交时的工作目录（OSC 9;9 上报；None 表示未知）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 进程退出码（None = 尚未回填或种子导入条目）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 命令耗时毫秒（None = 尚未回填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 提交时刻（Unix 毫秒）。
    pub ts: u64,
    /// 该命令在历史中出现次数（`deduplicate` 去重时统计；`#[serde(default)]` 兼容旧 jsonl 文件）。
    #[serde(default)]
    pub count: u32,
}

impl HistoryEntry {
    /// 构造一条「刚提交、exit_code 尚未知」的条目。
    pub fn new_submitted(text: String, cwd: Option<String>, ts: u64) -> Self {
        Self {
            text,
            cwd,
            exit_code: None,
            duration_ms: None,
            ts,
            count: 0, // load → deduplicate 时重算
        }
    }
}

/// 命令历史库（进程生命周期内单例）。
///
/// 内存中保留已去重的最近 [`MAX_ENTRIES`] 条；
/// 提交时追加写文件（轻量），退出时原子重写。
pub struct HistoryStore {
    /// 历史文件路径（`%LOCALAPPDATA%/Lumen/history.jsonl`）。
    path: PathBuf,
    /// 内存条目：按提交时间升序，去重键 = `text`（保留 ts 最新）。
    entries: Vec<HistoryEntry>,
    /// 历史游标（上下导航时的当前位置；None = 未进入导航态）。
    /// 值域：0 = 最旧一条，`entries.len()-1` = 最新一条，
    /// 再按"下"回到草稿区。
    cursor: Option<usize>,
    /// 进入历史导航前的草稿暂存（↑ 第一按时保存）。
    draft: Option<String>,
    /// 草稿暂存中的「放弃稿」（Ctrl+C 存的），↑ 优先呈现。
    abandoned: Option<String>,
}

/// 单会话内存条目上限。
const MAX_ENTRIES: usize = 10_000;
/// PSReadLine 种子导入上限（最近条目）。
const SEED_LIMIT: usize = 5_000;

/// 当前 Unix 毫秒时间戳。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

/// 历史库文件路径（`%LOCALAPPDATA%/Lumen/history.jsonl`）。
///
/// # Errors
/// 若 LOCALAPPDATA 环境变量未设置则返回 None（此时历史功能静默降级）。
pub fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut p = PathBuf::from(base);
    p.push("Lumen");
    p.push("history.jsonl");
    Some(p)
}

impl HistoryStore {
    /// 启动时加载历史库（文件不存在时尝试 PSReadLine 种子导入）。
    ///
    /// 失败静默降级：返回空 store（不中断启动流程）。
    pub fn load() -> Self {
        let path = history_path().unwrap_or_else(|| PathBuf::from("history.jsonl"));
        let seed_needed = !path.exists();

        let mut store = Self {
            path: path.clone(),
            entries: Vec::new(),
            cursor: None,
            draft: None,
            abandoned: None,
        };

        if seed_needed {
            // 首次：尝试 PSReadLine 种子导入。
            store.import_psreadline_seed();
        } else {
            // 读取现有文件。
            store.load_from_file();
        }

        info!(
            "历史库加载完成：{} 条（路径：{}）",
            store.entries.len(),
            path.display()
        );
        store
    }

    /// 从 JSONL 文件加载并去重。
    fn load_from_file(&mut self) {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return;
        };
        let reader = std::io::BufReader::new(file);
        let mut raw: Vec<HistoryEntry> = Vec::new();
        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim().to_owned();
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<HistoryEntry>(&line) {
                raw.push(e);
                // 损坏行（Err）静默跳过
            }
        }
        self.entries = deduplicate(raw, MAX_ENTRIES);
    }

    /// PSReadLine 种子导入（首次启动时调用）。
    fn import_psreadline_seed(&mut self) {
        let psrl_path = match std::env::var_os("APPDATA") {
            Some(a) => {
                let mut p = PathBuf::from(a);
                p.push("Microsoft");
                p.push("Windows");
                p.push("PowerShell");
                p.push("PSReadLine");
                p.push("ConsoleHost_history.txt");
                p
            }
            None => {
                info!("历史种子导入跳过：APPDATA 未设置");
                return;
            }
        };

        let Ok(file) = std::fs::File::open(&psrl_path) else {
            info!("历史种子导入跳过：PSReadLine 历史文件不存在（{psrl_path:?}）");
            return;
        };

        // 使用文件 mtime 作为 ts 兜底。
        let mtime_ms = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or_else(now_ms);

        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader
            .lines()
            .map_while(Result::ok)
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();

        // 取最近 SEED_LIMIT 条。
        let start = lines.len().saturating_sub(SEED_LIMIT);
        let seed: Vec<HistoryEntry> = lines[start..]
            .iter()
            .map(|text| HistoryEntry {
                text: text.clone(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: mtime_ms,
                count: 0, // deduplicate 时重算
            })
            .collect();

        let count = seed.len();
        self.entries = deduplicate(seed, MAX_ENTRIES);
        info!(
            "历史种子导入完成：{count} 条（来自 {}）",
            psrl_path.display()
        );
    }

    /// 提交命令时立即追加（不含 exit_code/duration，稍后回填）。
    ///
    /// - 写失败 log warn 不打扰用户。
    /// - 内存中先插入（exit_code=None），等块闭合时 [`Self::backfill`] 回填。
    pub fn append_submitted(&mut self, text: String, cwd: Option<String>) -> usize {
        let ts = now_ms();
        let entry = HistoryEntry::new_submitted(text, cwd, ts);

        // 确保父目录存在。
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        // 追加写文件（行刷新）。
        match serde_json::to_string(&entry) {
            Ok(line) => {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    if let Err(e) = writeln!(f, "{line}") {
                        warn!("历史库追加写失败: {e}");
                    }
                } else {
                    warn!("历史库文件打开失败: {}", self.path.display());
                }
            }
            Err(e) => warn!("历史条目序列化失败: {e}"),
        }

        // 内存插入（返回新条目的下标，供 backfill 用）。
        self.entries.push(entry);
        // 超限截断（保留最新）。
        if self.entries.len() > MAX_ENTRIES {
            let drain_n = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..drain_n);
        }
        // 返回最新条目在 entries 中的下标。
        self.entries.len() - 1
    }

    /// 块闭合时回填 exit_code 和 duration（内存操作，退出时原子重写）。
    ///
    /// `idx` 为 [`Self::append_submitted`] 返回的内存下标，可能因超限截断失效
    /// 此时改为按 text+ts 最近匹配查找并回填（简单正确优先）。
    pub fn backfill(&mut self, idx: usize, text: &str, ts: u64, exit_code: i32, duration_ms: u64) {
        // 优先按 idx 直接访问（大多数情况下有效）。
        if let Some(e) = self.entries.get_mut(idx) {
            if e.text == text && e.ts == ts {
                e.exit_code = Some(exit_code);
                e.duration_ms = Some(duration_ms);
                return;
            }
        }
        // idx 失效（因截断偏移）：按 text+ts 逆序找最近匹配。
        for e in self.entries.iter_mut().rev() {
            if e.text == text && e.ts == ts {
                e.exit_code = Some(exit_code);
                e.duration_ms = Some(duration_ms);
                return;
            }
        }
        // 完全找不到（极端情况）：忽略，不影响功能。
        warn!("历史库 backfill 未找到条目（text={text:?} ts={ts}）");
    }

    /// M4.2 命令文本对账：块闭合时用 shell 上报的**权威命令文本**
    /// （OSC 133;C base64 私参）校正历史记录。
    ///
    /// 编辑器本地记录的提交文本（`submitted`）与 shell 实际执行的文本
    /// （`authoritative`）一致时（绝大多数情况）直接返回；不一致时
    /// （PSReadLine 历史展开 `!!`、用户在直通态手敲等）以 shell 为准
    /// 更新内存条目（退出时 [`Self::flush_on_exit`] 原子重写落盘）。
    /// 条目按 `idx` 优先、失效则 `submitted`+`ts` 逆序匹配（同 backfill）。
    pub fn reconcile_text(&mut self, idx: usize, submitted: &str, ts: u64, authoritative: &str) {
        if submitted == authoritative || authoritative.is_empty() {
            return;
        }
        // idx 命中且 text+ts 吻合：直接校正。
        if self
            .entries
            .get(idx)
            .is_some_and(|e| e.text == submitted && e.ts == ts)
        {
            info!(
                "历史对账：命令文本以 shell 为准更新 {submitted:?} → {authoritative:?}"
            );
            self.entries[idx].text = authoritative.to_owned();
            return;
        }
        // idx 失效：按 text+ts 逆序找最近匹配。
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.text == submitted && e.ts == ts)
        {
            info!(
                "历史对账：命令文本以 shell 为准更新 {submitted:?} → {authoritative:?}"
            );
            e.text = authoritative.to_owned();
        }
    }

    /// 退出时原子重写（tmp → rename）。去重 + 保留最近 MAX_ENTRIES 条。
    ///
    /// 写失败 log warn 不打扰用户。
    pub fn flush_on_exit(&self) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(dir);

        // 先把内存条目去重后写到 tmp 文件。
        let tmp = self.path.with_extension("jsonl.tmp");
        let Ok(mut f) = std::fs::File::create(&tmp) else {
            warn!("历史库 tmp 文件创建失败: {}", tmp.display());
            return;
        };
        for entry in &self.entries {
            match serde_json::to_string(entry) {
                Ok(line) => {
                    if let Err(e) = writeln!(f, "{line}") {
                        warn!("历史库 flush 写失败: {e}");
                        return;
                    }
                }
                Err(e) => warn!("历史条目序列化失败: {e}"),
            }
        }
        drop(f);

        // 原子 rename（同盘移动，Windows 上 rename 是原子的）。
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            warn!("历史库 rename 失败: {e}（tmp 留存：{}）", tmp.display());
        }
    }

    // ── 历史导航 API ─────────────────────────────────────────────────

    /// 设置放弃稿（Ctrl+C 清空时由 editor.abandoned 同步到此处）。
    pub fn set_abandoned(&mut self, abandoned: Option<String>) {
        self.abandoned = abandoned;
    }

    /// 向上导航（↑），返回应填入输入区的文本。
    ///
    /// - 首按时：若有 abandoned 草稿则先呈现它，否则保存当前草稿并跳到最新历史条目。
    /// - 已在导航中：向旧方向移动一步（到达最旧则停在最旧）。
    ///
    /// 返回 `None` 表示没有历史可导航（历史为空）。
    pub fn navigate_up(&mut self, current_text: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }

        // 首按：先检查是否有放弃稿。
        if self.cursor.is_none() {
            // 保存当前草稿（无论是否为空，都保存以便 ↓ 回退）。
            self.draft = Some(current_text.to_owned());

            if let Some(ab) = self.abandoned.take() {
                // 有放弃稿：优先呈现，不移动历史游标（保持 None → 用户再按 ↑ 才进历史）。
                // 但为了下一次 ↑ 能进历史，此时仍要初始化游标为「最新」以便下次移。
                // 拍板：首按呈现 abandoned 后，游标置为 entries.len()（即「比最新还新1位」的虚位），
                // 下次 ↑ 进入 entries.len()-1（最新历史）。
                self.cursor = Some(self.entries.len()); // 虚位，下次 ↑ 减1
                return Some(ab);
            }

            // 没有放弃稿：游标从最新开始。
            let idx = self.entries.len() - 1;
            self.cursor = Some(idx);
            return Some(self.entries[idx].text.clone());
        }

        let cur = self.cursor.unwrap();

        // 已有游标但处于虚位（abandoned 呈现后）：进入最新历史。
        if cur == self.entries.len() {
            let idx = self.entries.len() - 1;
            self.cursor = Some(idx);
            return Some(self.entries[idx].text.clone());
        }

        // 普通导航：向旧方向移。
        if cur == 0 {
            // 已在最旧，停止移动。
            return Some(self.entries[0].text.clone());
        }
        let idx = cur - 1;
        self.cursor = Some(idx);
        Some(self.entries[idx].text.clone())
    }

    /// 向下导航（↓），返回应填入输入区的文本。
    ///
    /// - 到达最新历史之后恢复草稿（返回 None 表示应恢复草稿到编辑区；
    ///   但为了接口统一，恢复草稿也作为 `Some(String)` 返回）。
    /// - 未在导航中时返回 None（无操作）。
    ///
    /// 调用方：返回 `None` 表示无历史导航状态，不处理。
    pub fn navigate_down(&mut self) -> Option<String> {
        let cur = self.cursor?;

        // 在虚位（abandoned 展示后尚未进历史）：直接恢复草稿，退出导航。
        if cur == self.entries.len() {
            self.cursor = None;
            return Some(self.draft.take().unwrap_or_default());
        }

        // 已在最新历史：恢复草稿，退出导航。
        if cur + 1 >= self.entries.len() {
            self.cursor = None;
            return Some(self.draft.take().unwrap_or_default());
        }

        // 向新方向移。
        let idx = cur + 1;
        self.cursor = Some(idx);
        Some(self.entries[idx].text.clone())
    }

    /// 任何编辑动作调用此方法退出导航态（游标清空，草稿不恢复——当前文本即新草稿基线）。
    pub fn exit_navigation(&mut self) {
        self.cursor = None;
        self.draft = None;
        // 注意：不清 abandoned，它已在 navigate_up 里 take() 掉了
    }

    /// 是否处于历史导航态。
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// 只读访问所有内存条目（供 backfill 时按 idx 取 ts 用）。
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// 按前缀查找最佳历史联想（ghost text，M4.1 批3）。
    ///
    /// 规则：
    /// - `prefix` 空串或多行（含换行）时返回 None（ghost text 仅对单行非空前缀有效）。
    /// - 从最新条目向最旧逆序扫描，返回第一个**严格以 `prefix` 开头且不与 `prefix` 完全相等**的
    ///   条目文本的「后缀部分」（即 `text[prefix.len()..]`）。
    /// - 跳过多行条目（含 `\n` 的历史记录），保持 ghost text 单行干净。
    /// - 未找到返回 None。
    ///
    /// # Arguments
    /// * `prefix` - 当前输入框文本（UTF-8，光标在文末时使用）。
    ///
    /// # Returns
    /// 命中时返回补全后缀（不含 prefix 本身），未命中返回 None。
    ///
    /// # Examples
    /// ```
    /// use lumen_app::history::HistoryStore;
    ///
    /// // HistoryStore::load() 用于真实环境；测试中可用 entries 手动注入。
    /// ```
    pub fn find_ghost_prefix(&self, prefix: &str) -> Option<String> {
        // 空前缀 / 多行前缀不联想。
        if prefix.is_empty() || prefix.contains('\n') {
            return None;
        }
        // 逆序（最新 → 最旧）找第一个严格前缀匹配且不等于 prefix 的条目。
        for entry in self.entries.iter().rev() {
            // 跳过多行历史条目（保持 ghost 单行干净）。
            if entry.text.contains('\n') {
                continue;
            }
            // 严格前缀匹配：以 prefix 开头 + 条目内容 != prefix（避免 ghost 为空）。
            if entry.text.starts_with(prefix) && entry.text != prefix {
                let suffix = entry.text[prefix.len()..].to_owned();
                return Some(suffix);
            }
        }
        None
    }

    /// 模糊搜索历史命令（M4.3 Ctrl+R 历史搜索面板）。
    ///
    /// 子序列匹配（大小写不敏感）+ 评分（连续/词首边界 bonus + 短文本加权）。
    /// - **空 query**：按使用频率（`count` 降序）排列，同 count 按近因（entry_idx 大→小）；
    ///   取前 20 条（验收④）。
    /// - **非空 query**：按匹配分降序，同分按近因；取前 20 条。
    /// - **输出去重保险**：同 text 只保留排序最靠前的一条（验收②）。
    /// - 跳过多行条目（面板单行展示）。
    ///
    /// 返回的 [`HistorySearchHit::entry_idx`] 是 [`Self::entries`] 的下标。
    pub fn fuzzy_search(&self, query: &str) -> Vec<HistorySearchHit> {
        // query 去空白 + 小写化为字符序列。
        let q: Vec<char> = query
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();

        let mut hits: Vec<HistorySearchHit> = if q.is_empty() {
            // 空 query：全部非多行条目，按频率（count 降序）+ 近因排列。
            let mut v: Vec<HistorySearchHit> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.text.contains('\n'))
                .map(|(entry_idx, _)| HistorySearchHit {
                    entry_idx,
                    score: 0,
                    match_spans: Vec::new(),
                })
                .collect();
            v.sort_by(|a, b| {
                let ca = self.entries[a.entry_idx].count;
                let cb = self.entries[b.entry_idx].count;
                cb.cmp(&ca).then(b.entry_idx.cmp(&a.entry_idx))
            });
            v
        } else {
            // 非空 query：子序列匹配 + 评分。
            let mut v: Vec<HistorySearchHit> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.text.contains('\n'))
                .filter_map(|(entry_idx, e)| {
                    fuzzy_match_score(&e.text, &q).map(|(score, match_spans)| HistorySearchHit {
                        entry_idx,
                        score,
                        match_spans,
                    })
                })
                .collect();
            // 分降序；同分按近因（entry_idx 大 = 新），再按 count 做三级排序。
            v.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then(b.entry_idx.cmp(&a.entry_idx))
                    .then(
                        self.entries[b.entry_idx]
                            .count
                            .cmp(&self.entries[a.entry_idx].count),
                    )
            });
            v
        };

        // 输出层去重保险（验收②）：同 text 只保留排序最靠前的一条。
        {
            use std::collections::HashSet;
            let mut seen: HashSet<&str> = HashSet::new();
            hits.retain(|h| seen.insert(self.entries[h.entry_idx].text.as_str()));
        }

        // 截断：最多返回 20 条（验收④）。
        hits.truncate(20);
        hits
    }
}

/// 历史模糊搜索命中（M4.3 Ctrl+R 面板）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySearchHit {
    /// 命中条目在 [`HistoryStore::entries`] 中的下标。
    pub entry_idx: usize,
    /// 匹配分（降序排列，越大越相关）。
    pub score: i64,
    /// 命中字符在条目 `text` 中的字节区间 `[start, end)`（已合并连续，供高亮）。
    pub match_spans: Vec<(usize, usize)>,
}

/// 子序列模糊匹配 + 评分：`query_lower`（已小写、去空白）全部字符按序出现在
/// `text` 中则命中。返回 `(score, 命中字节区间)`，不命中返回 `None`。
///
/// 评分：每命中 +1；与上一命中字符相邻（连续）额外 +5；命中处于词首/边界
/// （前一字符非字母数字，或行首）额外 +3；命中后按文本越短越相关追加小幅加权。
fn fuzzy_match_score(text: &str, query_lower: &[char]) -> Option<(i64, Vec<(usize, usize)>)> {
    let mut qi = 0usize;
    let mut score: i64 = 0;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut prev_match_end: Option<usize> = None;
    let mut prev_char: Option<char> = None;
    for (byte, ch) in text.char_indices() {
        if qi >= query_lower.len() {
            break;
        }
        let ch_lower = ch.to_lowercase().next().unwrap_or(ch);
        if ch_lower == query_lower[qi] {
            let end = byte + ch.len_utf8();
            if prev_match_end == Some(byte) {
                // 与上一命中相邻：连续 bonus + 合并到上一区间。
                score += 5;
                if let Some(last) = spans.last_mut() {
                    last.1 = end;
                }
            } else {
                score += 1;
                spans.push((byte, end));
            }
            // 词首/边界 bonus（前一字符非字母数字，或行首）。
            if prev_char.map(|c| !c.is_alphanumeric()).unwrap_or(true) {
                score += 3;
            }
            prev_match_end = Some(end);
            qi += 1;
        }
        prev_char = Some(ch);
    }
    if qi == query_lower.len() {
        // 短文本加权（匹配密度）：文本越短越相关。
        score += (100i64 - text.len() as i64).max(0) / 10;
        Some((score, spans))
    } else {
        None
    }
}

/// 去重并保留最近 `max` 条：同 text 的条目合并（保留 exit_code 非 None 的，
/// 否则取 ts 最新的）；同时统计各 text 的出现次数写入 `count`（验收④）。
/// 输出按原始时间升序（老 → 新）。
fn deduplicate(mut raw: Vec<HistoryEntry>, max: usize) -> Vec<HistoryEntry> {
    // 第一遍：统计每个 text 的出现次数、最新有效 exit_code。
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut exit_codes: std::collections::HashMap<String, (i32, u64)> =
        std::collections::HashMap::new();
    for e in &raw {
        *counts.entry(e.text.clone()).or_insert(0) += 1;
        if let Some(ec) = e.exit_code {
            let prev_ts = exit_codes.get(&e.text).map(|(_, t)| *t).unwrap_or(0);
            if e.ts >= prev_ts {
                exit_codes.insert(e.text.clone(), (ec, e.ts));
            }
        }
    }

    // 第二遍：逆序去重（取最新出现的条目），同时写入 count 和 exit_code。
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped: Vec<HistoryEntry> = Vec::with_capacity(raw.len().min(max));
    for e in raw.iter_mut().rev() {
        if seen.insert(e.text.clone()) {
            // 写入出现次数（至少 1）。
            e.count = counts.get(&e.text).copied().unwrap_or(1).max(1);
            // 补充 exit_code（若本条目没有，但其他批次有）。
            if e.exit_code.is_none() {
                if let Some(&(ec, _)) = exit_codes.get(&e.text) {
                    e.exit_code = Some(ec);
                }
            }
            deduped.push(e.clone());
        }
    }
    // 逆序 → 恢复升序，再取最近 max 条。
    deduped.reverse();
    let start = deduped.len().saturating_sub(max);
    deduped[start..].to_vec()
}

/// 增高防抖纯函数（设计稿 §7.1 防 resize 风暴，M4.1 批D2）。
///
/// - 目标高 > 当前高（增高）：需稳定 [`DEBOUNCE_MS`] 毫秒才提交。
/// - 目标高 < 当前高（缩回）：立即提交（回 1 行无风暴风险且体感跟手）。
/// - 目标高 == 当前高：不提交（无变化）。
///
/// # Arguments
/// * `current_h` - 当前 footer 像素高度。
/// * `target_h`  - 目标 footer 像素高度。
/// * `changed_at` - 上次高度变化的时刻。
/// * `now`       - 当前时刻。
///
/// # Returns
/// `true` = 应提交新高度（走 footer_height 链路）；`false` = 继续等待。
pub fn footer_height_debounce(
    current_h: f32,
    target_h: f32,
    changed_at: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    const DEBOUNCE_MS: u64 = 100;
    if target_h < current_h {
        // 缩回：立即提交。
        return true;
    }
    if (target_h - current_h).abs() < 0.5 {
        // 等高：无变化，不提交。
        return false;
    }
    // 增高：稳定 DEBOUNCE_MS 后才提交。
    now.duration_since(changed_at).as_millis() >= DEBOUNCE_MS as u128
}

// ── 临时文件清理（测试辅助）────────────────────────────────────────
/// 测试专用辅助：创建不绑文件路径的内存 HistoryStore，供跨模块单测注入条目。
#[cfg(test)]
impl HistoryStore {
    /// 构造纯内存 HistoryStore（路径为占位符，不读/写磁盘）。
    pub fn new_in_memory() -> Self {
        Self {
            path: PathBuf::from("__in_memory__"),
            entries: Vec::new(),
            cursor: None,
            draft: None,
            abandoned: None,
        }
    }

    /// 直接向内存条目列表追加一条历史记录（不去重、不落盘；仅供单测）。
    pub fn inject_entry(&mut self, entry: HistoryEntry) {
        self.entries.push(entry);
    }
}

#[cfg(test)]
impl Drop for HistoryStore {
    fn drop(&mut self) {
        // 测试中 path 指向 tempdir，Drop 时不自动写文件（只在 flush_on_exit 时写）。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造使用临时文件的 HistoryStore（不读系统路径）。
    fn make_store(path: PathBuf) -> HistoryStore {
        HistoryStore {
            path,
            entries: Vec::new(),
            cursor: None,
            draft: None,
            abandoned: None,
        }
    }

    // ── 模糊搜索（M4.3）────────────────────────────────────────────────

    /// 用给定文本构造内存 store（ts 按下标递增 = 越后越新；count=1）。
    fn store_with(texts: &[&str]) -> HistoryStore {
        let mut s = make_store(std::env::temp_dir().join("lumen_fuzzy_test"));
        for (i, text) in texts.iter().enumerate() {
            s.entries.push(HistoryEntry {
                text: (*text).into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: (i + 1) as u64,
                count: 1,
            });
        }
        s
    }

    #[test]
    fn fuzzy_子序列匹配_高亮区间_不匹配() {
        let s = store_with(&["git status", "git commit -m x", "docker run nginx"]);
        // "git" 命中两条 git、不命中 docker；高亮区间 (0,3)。
        let hits = s.fuzzy_search("git");
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert_eq!(h.match_spans, vec![(0, 3)], "git 连续命中应合并为单区间");
        }
        // 子序列：'dkr' 命中 "docker run"（d..k..r）。
        let hits2 = s.fuzzy_search("dkr");
        assert!(hits2
            .iter()
            .any(|h| s.entries[h.entry_idx].text.starts_with("docker")));
        // 全不命中。
        assert!(s.fuzzy_search("zzz").is_empty());
    }

    #[test]
    fn fuzzy_大小写不敏感_空query按频率() {
        let s = store_with(&["LS -la", "PWD"]);
        assert_eq!(s.fuzzy_search("ls").len(), 1, "大小写不敏感");
        // 空 query：store_with 给每条 count=1，同 count 按近因（新→旧）。
        let all = s.fuzzy_search("");
        assert_eq!(all.len(), 2);
        assert_eq!(
            s.entries[all[0].entry_idx].text, "PWD",
            "同 count 时最新条目排首"
        );
    }

    #[test]
    fn fuzzy_连续与短文本优先() {
        let s = store_with(&["git", "g-i-t-x-y-z"]);
        let hits = s.fuzzy_search("git");
        assert_eq!(
            s.entries[hits[0].entry_idx].text, "git",
            "连续命中 + 短文本应排在分散命中之前"
        );
    }

    #[test]
    fn fuzzy_跳过多行条目() {
        let s = store_with(&["foo\nbar", "foobar"]);
        let hits = s.fuzzy_search("foo");
        assert_eq!(hits.len(), 1, "多行条目应跳过");
        assert_eq!(s.entries[hits[0].entry_idx].text, "foobar");
    }

    #[test]
    fn fuzzy_空query按频率降序排列() {
        // count 高的应排在前面（验收④）。
        let mut s = make_store(std::env::temp_dir().join("lumen_fuzzy_freq_test"));
        s.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 5,
        });
        s.entries.push(HistoryEntry {
            text: "pwd".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 2,
            count: 10,
        });
        s.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 3,
            count: 3,
        });
        let hits = s.fuzzy_search("");
        assert_eq!(s.entries[hits[0].entry_idx].text, "pwd", "count=10 应排首");
        assert_eq!(s.entries[hits[1].entry_idx].text, "ls", "count=5 应排第二");
        assert_eq!(
            s.entries[hits[2].entry_idx].text, "git status",
            "count=3 应排第三"
        );
    }

    #[test]
    fn fuzzy_结果最多20条() {
        // 超过 20 条时应截断（验收④）。
        let texts: Vec<String> = (0..30).map(|i| format!("command-{i}")).collect();
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let s = store_with(&text_refs);
        let hits = s.fuzzy_search("");
        assert_eq!(hits.len(), 20, "空 query 结果应截断至 20 条");
        let hits2 = s.fuzzy_search("command");
        assert_eq!(hits2.len(), 20, "非空 query 结果也应截断至 20 条");
    }

    #[test]
    fn fuzzy_输出层去重() {
        // 即使 entries 中有重复文本（防御性），输出也应去重（验收②）。
        let mut s = make_store(std::env::temp_dir().join("lumen_fuzzy_dedup_test"));
        // 直接插入两条相同 text（绕过 deduplicate，模拟极端情况）。
        for i in 0..3u64 {
            s.entries.push(HistoryEntry {
                text: "git status".into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: i,
                count: 1,
            });
        }
        s.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 10,
            count: 1,
        });
        let hits = s.fuzzy_search("git");
        let texts: Vec<&str> = hits
            .iter()
            .map(|h| s.entries[h.entry_idx].text.as_str())
            .collect();
        assert_eq!(
            texts.iter().filter(|&&t| t == "git status").count(),
            1,
            "输出层去重应只保留一条 git status"
        );
    }

    // ── 去重函数 ─────────────────────────────────────────────────────

    #[test]
    fn deduplicate_保留最新条目() {
        let raw = vec![
            HistoryEntry {
                text: "ls".into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: 100,
                count: 0,
            },
            HistoryEntry {
                text: "pwd".into(),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: 200,
                count: 0,
            },
            HistoryEntry {
                text: "ls".into(),
                cwd: None,
                exit_code: Some(0),
                duration_ms: None,
                ts: 300,
                count: 0,
            },
        ];
        let result = deduplicate(raw, 100);
        assert_eq!(result.len(), 2, "ls 重复应去重，保留 2 条");
        // 升序：pwd(200) < ls(300)
        assert_eq!(result[0].text, "pwd");
        assert_eq!(result[1].text, "ls");
        assert_eq!(result[1].ts, 300, "应保留最新 ts 的 ls");
        assert_eq!(result[1].exit_code, Some(0), "exit_code 应被保留");
        // count 统计：ls 出现 2 次，pwd 出现 1 次
        assert_eq!(result[1].count, 2, "ls 出现 2 次，count 应为 2");
        assert_eq!(result[0].count, 1, "pwd 出现 1 次，count 应为 1");
    }

    #[test]
    fn deduplicate_超限截断() {
        let raw: Vec<HistoryEntry> = (0..10)
            .map(|i| HistoryEntry {
                text: format!("cmd{i}"),
                cwd: None,
                exit_code: None,
                duration_ms: None,
                ts: i as u64,
                count: 0,
            })
            .collect();
        let result = deduplicate(raw, 5);
        assert_eq!(result.len(), 5, "应截断到 5 条");
        // 保留最新 5 条（ts: 5,6,7,8,9）
        assert_eq!(result[0].text, "cmd5");
        assert_eq!(result[4].text, "cmd9");
    }

    // ── 历史导航 ─────────────────────────────────────────────────────

    #[test]
    fn navigate_空历史_返回none() {
        let mut store = make_store(PathBuf::from("test_nav_empty.jsonl"));
        assert!(store.navigate_up("").is_none(), "空历史 ↑ 应返回 None");
        assert!(store.navigate_down().is_none(), "空历史 ↓ 应返回 None");
    }

    #[test]
    fn navigate_上下来回_草稿恢复() {
        let mut store = make_store(PathBuf::from("test_nav_basic.jsonl"));
        // 手动添加条目（不写文件）。
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        store.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 2,
            count: 1,
        });

        // ↑ 首按：保存草稿，跳最新。
        let t1 = store.navigate_up("我的草稿").unwrap();
        assert_eq!(t1, "ls", "首按 ↑ 应返回最新历史");
        assert_eq!(store.draft, Some("我的草稿".to_owned()), "草稿应被保存");

        // ↑ 再按：向旧。
        let t2 = store.navigate_up("ls").unwrap();
        assert_eq!(t2, "git status", "第二按 ↑ 应返回更旧的历史");

        // ↑ 再按：已在最旧，停在最旧。
        let t3 = store.navigate_up("git status").unwrap();
        assert_eq!(t3, "git status", "最旧处 ↑ 应停在最旧");

        // ↓：向新。
        let t4 = store.navigate_down().unwrap();
        assert_eq!(t4, "ls");

        // ↓：最新之后恢复草稿。
        let t5 = store.navigate_down().unwrap();
        assert_eq!(t5, "我的草稿", "↓ 到达末尾应恢复草稿");
        assert!(!store.is_navigating(), "恢复草稿后导航态应退出");
    }

    #[test]
    fn navigate_编辑退出导航() {
        let mut store = make_store(PathBuf::from("test_nav_edit.jsonl"));
        store.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });

        let _ = store.navigate_up("");
        assert!(store.is_navigating(), "导航后应处于导航态");

        store.exit_navigation();
        assert!(!store.is_navigating(), "exit_navigation 后应退出导航态");
    }

    #[test]
    fn navigate_abandoned_优先呈现() {
        let mut store = make_store(PathBuf::from("test_nav_abandoned.jsonl"));
        store.entries.push(HistoryEntry {
            text: "cmd1".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        store.abandoned = Some("放弃的命令".to_owned());

        // 首按：呈现 abandoned，不进历史。
        let t1 = store.navigate_up("").unwrap();
        assert_eq!(t1, "放弃的命令", "首按 ↑ 有 abandoned 时应呈现 abandoned");
        assert!(store.abandoned.is_none(), "呈现后 abandoned 应被清除");

        // 再按 ↑：进入历史。
        let t2 = store.navigate_up(&t1).unwrap();
        assert_eq!(t2, "cmd1", "第二按 ↑ 应进入历史");
    }

    // ── backfill ─────────────────────────────────────────────────────

    #[test]
    fn backfill_按idx回填() {
        let mut store = make_store(PathBuf::from("test_backfill.jsonl"));
        // 直接添加条目。
        store.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1000,
            count: 1,
        });
        let idx = 0;
        store.backfill(idx, "ls", 1000, 0, 150);
        assert_eq!(store.entries[0].exit_code, Some(0), "exit_code 应被回填");
        assert_eq!(
            store.entries[0].duration_ms,
            Some(150),
            "duration_ms 应被回填"
        );
    }

    #[test]
    fn reconcile_一致时不改() {
        let mut store = make_store(PathBuf::from("test_reconcile_same.jsonl"));
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1000,
            count: 1,
        });
        store.reconcile_text(0, "git status", 1000, "git status");
        assert_eq!(store.entries[0].text, "git status", "一致时文本不变");
    }

    #[test]
    fn reconcile_不一致以shell为准() {
        let mut store = make_store(PathBuf::from("test_reconcile_diff.jsonl"));
        // 编辑器记录的是历史展开前的 "!!"，shell 实际执行的是展开后的命令。
        store.entries.push(HistoryEntry {
            text: "!!".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 2000,
            count: 1,
        });
        store.reconcile_text(0, "!!", 2000, "cargo build");
        assert_eq!(
            store.entries[0].text, "cargo build",
            "不一致时以 shell 权威文本为准"
        );
    }

    #[test]
    fn 多块闭合_先全backfill再对账_退出码不丢() {
        // 回归（审查 finding）：同批多块闭合时 reconcile 必须在所有
        // backfill **之后**统一做一次——backfill 以 submitted 为匹配键，
        // 若循环内提前 reconcile 把 text 改成 authoritative，会毒化同批
        // 后续块的 backfill 匹配，致最后一条命令退出码丢失。本测试钉住
        // 「先两次 backfill（均以 submitted 命中），再一次 reconcile」的
        // 正确组合：退出码取自最后一块、文本被对账。
        let mut store = make_store(PathBuf::from("test_multi_block_reconcile.jsonl"));
        store.entries.push(HistoryEntry {
            text: "submitted".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 5000,
            count: 1,
        });
        store.backfill(0, "submitted", 5000, 1, 100); // 块1 exit=1
        store.backfill(0, "submitted", 5000, 0, 200); // 块2 exit=0（最后一块覆盖）
        store.reconcile_text(0, "submitted", 5000, "authoritative");
        assert_eq!(
            store.entries[0].exit_code,
            Some(0),
            "退出码应取自最后一块（backfill 匹配键未被提前 reconcile 毒化）"
        );
        assert_eq!(store.entries[0].duration_ms, Some(200));
        assert_eq!(store.entries[0].text, "authoritative", "文本已以 shell 为准对账");
    }

    #[test]
    fn reconcile_权威文本为空不改() {
        let mut store = make_store(PathBuf::from("test_reconcile_empty.jsonl"));
        store.entries.push(HistoryEntry {
            text: "ls".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 3000,
            count: 1,
        });
        store.reconcile_text(0, "ls", 3000, "");
        assert_eq!(store.entries[0].text, "ls", "权威文本为空时不覆盖");
    }

    // ── find_ghost_prefix ghost text 前缀匹配 ───────────────────────

    #[test]
    fn ghost_空前缀_返回none() {
        let mut store = make_store(PathBuf::from("test_ghost_empty.jsonl"));
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        assert!(
            store.find_ghost_prefix("").is_none(),
            "空前缀不应产生 ghost text"
        );
    }

    #[test]
    fn ghost_多行前缀_返回none() {
        let mut store = make_store(PathBuf::from("test_ghost_multiline.jsonl"));
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        assert!(
            store.find_ghost_prefix("git\nstatus").is_none(),
            "多行前缀不应产生 ghost text"
        );
    }

    #[test]
    fn ghost_命中最新条目() {
        let mut store = make_store(PathBuf::from("test_ghost_hit.jsonl"));
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        store.entries.push(HistoryEntry {
            text: "git diff".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 2,
            count: 1,
        });
        // 最新条目是 "git diff"，前缀 "git " → 联想为 "diff"
        let ghost = store.find_ghost_prefix("git ");
        assert_eq!(
            ghost.as_deref(),
            Some("diff"),
            "应联想最新匹配 'git diff' 的后缀 'diff'"
        );
    }

    #[test]
    fn ghost_完全相等_跳过() {
        let mut store = make_store(PathBuf::from("test_ghost_equal.jsonl"));
        store.entries.push(HistoryEntry {
            text: "git status".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        // 前缀 = 条目本身，应跳过（避免 ghost 为空串）
        assert!(
            store.find_ghost_prefix("git status").is_none(),
            "前缀与条目完全相等时不应产生 ghost"
        );
    }

    #[test]
    fn ghost_跳过多行历史条目() {
        let mut store = make_store(PathBuf::from("test_ghost_skip_multiline.jsonl"));
        // 最新条目：多行（应跳过）
        store.entries.push(HistoryEntry {
            text: "git\nstatus".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 2,
            count: 1,
        });
        // 次新条目：单行（应命中）
        store.entries.push(HistoryEntry {
            text: "git status --short".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        let ghost = store.find_ghost_prefix("git s");
        assert_eq!(
            ghost.as_deref(),
            Some("tatus --short"),
            "应跳过多行条目命中次新单行条目"
        );
    }

    #[test]
    fn ghost_无匹配_返回none() {
        let mut store = make_store(PathBuf::from("test_ghost_nomatch.jsonl"));
        store.entries.push(HistoryEntry {
            text: "ls -la".into(),
            cwd: None,
            exit_code: None,
            duration_ms: None,
            ts: 1,
            count: 1,
        });
        assert!(
            store.find_ghost_prefix("git").is_none(),
            "无前缀匹配时应返回 None"
        );
    }

    // ── 增高防抖纯函数 ────────────────────────────────────────────────

    #[test]
    fn debounce_增高需稳定100ms() {
        use std::time::{Duration, Instant};
        // 目标高 > 当前高：需防抖（稳定 100ms 才提交）。
        let now = Instant::now();
        let changed_at = now - Duration::from_millis(50); // 只过了 50ms
        let result = footer_height_debounce(30.0, 50.0, changed_at, now);
        assert!(!result, "未稳定 100ms，不应提交增高");

        let changed_at2 = now - Duration::from_millis(101); // 过了 101ms
        let result2 = footer_height_debounce(30.0, 50.0, changed_at2, now);
        assert!(result2, "稳定 101ms 后，应提交增高");
    }

    #[test]
    fn debounce_缩回立即提交() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let changed_at = now - Duration::from_millis(5); // 刚刚变化
                                                         // 目标高 < 当前高（缩回）：立即提交。
        let result = footer_height_debounce(50.0, 30.0, changed_at, now);
        assert!(result, "缩回应立即提交，不防抖");
    }

    #[test]
    fn debounce_等高不提交() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let changed_at = now - Duration::from_millis(200); // 很久前
                                                           // 目标高 == 当前高：无变化，不提交。
        let result = footer_height_debounce(30.0, 30.0, changed_at, now);
        assert!(!result, "等高时不应提交（无变化）");
    }
}
