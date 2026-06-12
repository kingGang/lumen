//! 全量文案结构体（编译期完备性保证）。
//!
//! # 新增文案纪律
//! 每新增一条用户可见文案：
//! 1. 在 [`Strings`] 加对应字段（`pub xxx: &'static str`）；
//! 2. 在 [`super::zh_cn`]、[`super::zh_tw`]、[`super::en`] 三个文件里
//!    各填一条——只要有一个实例缺字段，**编译就会报错**，这是本方案的
//!    核心保证，不依赖运行期检查；
//! 3. 插值文案用 `{}` 单参或 `{0}` `{1}` 双参占位符，调用方用
//!    [`super::fmt1`] / [`super::fmt2`] 组装。

/// 全量 UI 文案（三语实例：[`super::zh_cn::STRINGS`] /
/// [`super::zh_tw::STRINGS`] / [`super::en::STRINGS`]）。
///
/// 缺任何字段 → 编译错误：无法在运行期出现翻译遗漏。
pub struct Strings {
    // ── 侧栏 / 窗格标题栏 ───────────────────────────────────────────
    /// "会话" 侧栏分组标签
    pub sidebar_sessions: &'static str,
    /// 右键菜单"重命名"
    pub menu_rename: &'static str,
    /// 右键菜单"关闭"
    pub menu_close: &'static str,
    /// 窗格数指示，单参 `{}`：格数字
    pub pane_count_fmt: &'static str,
    // sidebar_settings_btn / sidebar_settings_tip / sidebar_new_session_btn
    // 已于 R8 删除（底部按钮区删除，入口改为头像菜单 + 侧栏标题栏小「＋」）。
    /// 窗格 ✕ tooltip
    pub pane_close_tip: &'static str,
    /// 还原窗格 tooltip（最大化态）
    pub pane_restore_tip: &'static str,
    /// 最大化窗格 tooltip（普通态）
    pub pane_maximize_tip: &'static str,
    /// shell 忙 toast "Shell 正忙，未执行 cd"
    pub shell_busy_cd: &'static str,

    // ── 顶栏 ─────────────────────────────────────────────────────────
    /// 窗控：最小化按钮 tooltip
    pub wc_minimize: &'static str,
    /// 窗控：最大化按钮 tooltip（普通态）
    pub wc_maximize: &'static str,
    /// 窗控：还原按钮 tooltip（最大化态）
    pub wc_restore: &'static str,
    /// 窗控：关闭按钮 tooltip
    pub wc_close: &'static str,
    /// 新增窗格 tooltip "新增窗格 (Ctrl+Shift+D)"
    pub topbar_new_pane_tip: &'static str,
    /// 新增窗格禁用 tooltip，单参 `{}`：MAX_PANES 数字
    pub topbar_max_panes_fmt: &'static str,
    /// 头像 tooltip（未登录态）"未登录"
    pub topbar_not_logged_in: &'static str,
    /// 头像菜单 Settings
    pub menu_settings: &'static str,
    /// 头像菜单 Keyboard shortcuts
    pub menu_keyboard_shortcuts: &'static str,
    /// 头像菜单 Documentation（灰显占位）
    pub menu_documentation: &'static str,
    /// 头像菜单 Log out
    pub menu_log_out: &'static str,
    /// 头像菜单 Log in
    pub menu_log_in: &'static str,

    // ── 设置页 ───────────────────────────────────────────────────────
    /// 设置页顶栏标题 "Settings"
    pub settings_title: &'static str,
    /// 导航 "Account"
    pub nav_account: &'static str,
    /// 导航 "Appearance"
    pub nav_appearance: &'static str,
    /// 导航 "Keyboard shortcuts"
    pub nav_keyboard_shortcuts: &'static str,
    /// 导航 "About"
    pub nav_about: &'static str,
    // Account 页
    /// 未登录文字 "未登录"
    pub account_not_logged_in: &'static str,
    /// 未登录副文字
    pub account_not_logged_in_sub: &'static str,
    /// Log out 按钮
    pub account_log_out: &'static str,
    /// Log in 按钮
    pub account_log_in: &'static str,
    // Appearance 页
    /// Appearance heading
    pub appearance_heading: &'static str,
    /// Themes 组标题
    pub appearance_themes: &'static str,
    /// "Sync with OS" 开关标签
    pub appearance_sync_with_os: &'static str,
    /// Sync 副文字
    pub appearance_sync_sub: &'static str,
    /// Sync 开启时的双槽说明，双参 `{0}`=深色主题名 `{1}`=浅色主题名
    pub appearance_sync_slots_fmt: &'static str,
    /// Current theme 标签
    pub appearance_current_theme: &'static str,
    /// Text 组标题
    pub appearance_text: &'static str,
    /// 终端字体标签
    pub appearance_font_family: &'static str,
    /// 字体下拉"自定义…"
    pub appearance_font_custom: &'static str,
    /// 字体下拉"自动（系统等宽）"
    pub appearance_font_auto: &'static str,
    /// 字体输入框 hint
    pub appearance_font_hint: &'static str,
    /// "应用" 按钮
    pub appearance_font_apply: &'static str,
    /// 终端字号标签
    pub appearance_font_size: &'static str,
    /// 背景图片组标题
    pub appearance_bg_title: &'static str,
    /// 启用背景图片开关标签
    pub appearance_bg_enable: &'static str,
    /// "选择图片…" 按钮
    pub appearance_bg_pick: &'static str,
    /// rfd 对话框标题 "选择背景图片"
    pub appearance_bg_dialog_title: &'static str,
    /// rfd 过滤器名 "图片文件"
    pub appearance_bg_filter_name: &'static str,
    /// "清除" 按钮
    pub appearance_bg_clear: &'static str,
    /// "未选择图片" 占位
    pub appearance_bg_none: &'static str,
    /// 不透明度标签
    pub appearance_bg_opacity: &'static str,
    /// 暗化标签
    pub appearance_bg_dim: &'static str,
    /// 暗化说明
    pub appearance_bg_dim_sub: &'static str,
    /// 主题卡徽标"浅色"
    pub appearance_theme_badge_light: &'static str,
    /// 主题卡徽标"深色"
    pub appearance_theme_badge_dark: &'static str,
    // Keyboard shortcuts 页
    /// Keyboard shortcuts heading
    pub shortcuts_heading: &'static str,
    // 快捷键说明列（键位列不翻）
    pub shortcut_new_session: &'static str,
    pub shortcut_close_session: &'static str,
    pub shortcut_next_prev_session: &'static str,
    pub shortcut_filetree_toggle: &'static str,
    pub shortcut_settings_toggle: &'static str,
    pub shortcut_jump_block: &'static str,
    pub shortcut_copy_or_interrupt: &'static str,
    pub shortcut_paste: &'static str,
    pub shortcut_scroll: &'static str,
    pub shortcut_close_settings: &'static str,
    // About 页
    /// About heading
    pub about_heading: &'static str,
    /// 版本标签，单参 `{}`：版本字符串
    pub about_version_fmt: &'static str,

    // ── 语言设置组（设置页 Appearance 内）───────────────────────────
    /// "语言 / Language" 组标题
    pub appearance_language: &'static str,

    // ── 登录页 ───────────────────────────────────────────────────────
    /// 登录副标题
    pub login_subtitle: &'static str,
    /// 邮箱 hint
    pub login_email_hint: &'static str,
    /// 密码 hint
    pub login_password_hint: &'static str,
    /// 登录按钮
    pub login_btn: &'static str,

    // ── 文件树 UI ────────────────────────────────────────────────────
    /// 刷新按钮标签
    pub filetree_refresh: &'static str,
    /// 刷新 tooltip
    pub filetree_refresh_tip: &'static str,
    /// 搜索按钮 tooltip
    pub filetree_search_tip: &'static str,
    /// 树根无 cwd 时的占位标题 "文件"
    pub filetree_root_placeholder: &'static str,
    /// 搜索输入框 hint
    pub filetree_search_hint: &'static str,
    /// shell 忙碌轻提示（树内）
    pub filetree_shell_busy: &'static str,
    /// 等待 shell 上报路径占位
    pub filetree_waiting_cwd: &'static str,
    /// 搜索中占位
    pub filetree_searching: &'static str,
    /// 无匹配项占位
    pub filetree_no_results: &'static str,
    /// 结果截断占位
    pub filetree_truncated: &'static str,
    /// 搜索结果截断 toast
    pub filetree_search_truncated_toast: &'static str,
    /// 溢出行，单参 `{}`：未显示条目数
    pub filetree_overflow_fmt: &'static str,
    /// "无法读取" 占位
    pub filetree_unreadable: &'static str,
    /// "加载中…" 占位
    pub filetree_loading: &'static str,
    // 新建对话框
    /// "新建文件夹" 对话框标题
    pub filetree_create_dir_title: &'static str,
    /// "新建文件" 对话框标题
    pub filetree_create_file_title: &'static str,
    /// 位于路径行，单参 `{}`：目录显示名
    pub filetree_create_location_fmt: &'static str,
    /// 名称输入框 hint
    pub filetree_create_name_hint: &'static str,
    /// "创建" 按钮
    pub filetree_create_btn: &'static str,
    /// "取消" 按钮
    pub filetree_cancel_btn: &'static str,
    // 删除确认对话框
    /// "删除" 对话框标题
    pub filetree_delete_title: &'static str,
    /// 类型词"文件夹（含其中全部内容）"
    pub filetree_delete_what_dir: &'static str,
    /// 类型词"文件"
    pub filetree_delete_what_file: &'static str,
    /// 删除确认文案，双参 `{0}`=类型词 `{1}`=名称
    pub filetree_delete_confirm_fmt: &'static str,
    /// "移入回收站" 确认按钮
    pub filetree_delete_trash_btn: &'static str,
    // 右键菜单
    /// "进入文件夹"
    pub filetree_menu_enter_dir: &'static str,
    /// "新建文件"
    pub filetree_menu_new_file: &'static str,
    /// "新建文件夹"
    pub filetree_menu_new_dir: &'static str,
    /// "在文件管理器中打开"
    pub filetree_menu_reveal: &'static str,
    /// "复制绝对路径"
    pub filetree_menu_copy_abs: &'static str,
    /// "复制相对路径"
    pub filetree_menu_copy_rel: &'static str,
    /// "删除（移入回收站）"
    pub filetree_menu_delete: &'static str,

    // ── main.rs toast ────────────────────────────────────────────────
    /// 背景图加载失败 toast，单参 `{}`：错误文本
    pub toast_bg_load_failed_fmt: &'static str,
    /// 每个会话最多 N 个窗格 toast，单参 `{}`：MAX_PANES
    pub toast_max_panes_fmt: &'static str,
    /// 新建窗格失败 toast，单参 `{}`：错误文本
    pub toast_new_pane_failed_fmt: &'static str,
    /// 旧 cwd 失效 toast，单参 `{}`：失效会话数
    pub toast_stale_cwd_fmt: &'static str,
    /// 字体回退提示，双参 `{0}`=请求字体名 `{1}`=实际字体名
    pub toast_font_fallback_fmt: &'static str,
    /// 设置保存失败 toast，单参 `{}`：错误文本
    pub toast_settings_save_failed_fmt: &'static str,
    /// 登录成功 toast，单参 `{}`：展示名
    pub toast_logged_in_fmt: &'static str,
    /// 复制成功 toast，单参 `{}`：复制内容
    pub toast_copied_fmt: &'static str,
    /// 复制失败 toast
    pub toast_copy_failed: &'static str,
    /// 窗格兜底名，单参 `{}`：(index+1)
    pub pane_default_name_fmt: &'static str,
    /// 会话兜底名，单参 `{}`：(id+1)
    pub session_default_name_fmt: &'static str,

    // ── filetree 后台操作 toast（OpReply 结果枚举化后的文案）────────
    /// "已创建：{name}"，单参 `{}`：名称
    pub filetree_created_fmt: &'static str,
    /// "创建失败：「{name}」已存在"，单参 `{}`：名称
    pub filetree_create_exists_fmt: &'static str,
    /// "创建失败：{e}"，单参 `{}`：错误文本
    pub filetree_create_failed_fmt: &'static str,
    /// "已移入回收站：{name}"，单参 `{}`：名称
    pub filetree_trashed_fmt: &'static str,
    /// "删除失败：{e}"，单参 `{}`：错误文本
    pub filetree_delete_failed_fmt: &'static str,
    /// "打开文件管理器失败：{e}"，单参 `{}`：错误文本
    pub filetree_reveal_failed_fmt: &'static str,

    // ── M4.1 批B：经典直通模式切换 toast ────────────────────────────
    /// 切换为经典直通模式的 toast（Ctrl+Shift+E 开启）
    pub toast_fallback_enabled: &'static str,
    /// 关闭经典直通模式的 toast（Ctrl+Shift+E 关闭）
    pub toast_fallback_disabled: &'static str,

    // ── M4.1 批C：footer 状态条文案（input-editor feature）──────────
    // 字段在 feature 剔除时不被读取（dead_code）；用 cfg_attr 消除警告，
    // 字段仍保留在 struct 内（三语编译期完备性要求不能 cfg 删字段）。
    /// Running 态状态条主文案（等高状态条中央文本）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub footer_running_text: &'static str,

    // ── M4.1 批D1：Compose 态键位占位提示 ──────────────────────────
    /// Compose 态 Tab 键占位提示 toast（M3.4 补全未实现）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub toast_compose_tab_hint: &'static str,
    /// Compose 态 Ctrl+R 占位提示 toast（D2 历史搜索未实现）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub toast_compose_history_hint: &'static str,

    // ── 侧栏标题栏（R8）─────────────────────────────────────────────────────
    /// 侧栏标题栏「＋」按钮 tooltip（新建会话，含快捷键）
    pub sidebar_new_session_tip: &'static str,

    // ── 顶栏三视图切换按钮（问题7）────────────────────────────────────
    /// 显示/隐藏会话栏 tooltip（展开态）
    pub topbar_sidebar_show_tip: &'static str,
    /// 显示/隐藏会话栏 tooltip（隐藏态）
    pub topbar_sidebar_hide_tip: &'static str,
    /// 显示/隐藏文件树 tooltip（展开态）
    pub topbar_filetree_show_tip: &'static str,
    /// 显示/隐藏文件树 tooltip（隐藏态）
    pub topbar_filetree_hide_tip: &'static str,
    /// 还原窗格大小 tooltip（启用态，对应原「▦」功能）
    pub topbar_reset_layout_tip: &'static str,
    /// 还原窗格大小禁用 tooltip（单窗格时）
    pub topbar_reset_layout_disabled_tip: &'static str,

    // ── profile 校验错误（UI 侧翻译）────────────────────────────────
    /// 邮箱格式不正确
    pub login_err_invalid_email: &'static str,
    /// 请输入密码
    pub login_err_empty_password: &'static str,

    // ── M4.1 批E：底部状态栏（M3.8 海风哥反馈 #3/#6）────────────────
    /// 状态栏：Compose 态模式指示文字（含图标前缀）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_compose: &'static str,
    /// 状态栏：Running 态模式指示文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_running: &'static str,
    /// 状态栏：AltScreen 态模式指示文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_altscreen: &'static str,
    /// 状态栏：Fallback 态模式指示文字（警示色）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_fallback: &'static str,
    /// 状态栏：经典模式切换按钮关态文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_off: &'static str,
    /// 状态栏：经典模式切换按钮开态文字（已开启时显示）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_on: &'static str,
    /// 状态栏：经典模式切换按钮 hover tooltip
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_tip: &'static str,
    /// Compose 态输入框占位提示文字（缓冲为空时 footer 显示）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub composer_placeholder: &'static str,

    // ── 输入框右键菜单（第十一轮）────────────────────────────────────
    /// 右键菜单：复制
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_copy: &'static str,
    /// 右键菜单：剪切
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_cut: &'static str,
    /// 右键菜单：粘贴
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_paste: &'static str,
    /// 右键菜单：全选
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_select_all: &'static str,

    // ── filetree 名字校验错误（UI 侧翻译）──────────────────────────
    /// 名称不能为空
    pub validate_name_empty: &'static str,
    /// 名称不合法（"." / ".."）
    pub validate_name_illegal: &'static str,
    /// 名称不能包含控制字符
    pub validate_name_control_chars: &'static str,
    /// 名称不能包含非法字符
    pub validate_name_bad_chars: &'static str,
    /// 名称不能以点或空格结尾
    pub validate_name_trailing: &'static str,
}
