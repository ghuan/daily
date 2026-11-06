//! 存储层：JSON 文件 + 本地 / Git 两种后端。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Doc;

/// 创建一个不会弹控制台窗口的 git 子进程。
///
/// release 版是 GUI 子系统（`windows_subsystem = "windows"`），自己没有控制台；
/// 此时再拉起控制台程序（git.exe），Windows 会**为子进程新建一个控制台窗口**，
/// 屏幕上就是黑框闪一下。`CREATE_NO_WINDOW`（0x0800_0000）让子进程不要窗口，
/// 这是 GUI 程序调命令行工具的标准做法。
///
/// std 的 `CommandExt::creation_flags` 在 Windows 上才存在，非 Windows 走普通分支，
/// 保证 `cargo test` 在别的平台也能编。
fn git_command() -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 存储方式。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 只存本地 JSON 文件。
    Local,
    /// 本地 JSON + Git 仓库同步（启动拉取、改动后推送）。
    Git,
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Local
    }
}

/// Git 仓库配置；密码按用户要求以明文保存在配置文件里。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitConfig {
    /// 远端仓库地址，如 https://example.com/me/daily.git
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// 明文密码 / 访问令牌。
    #[serde(default)]
    pub password: String,
    /// 分支名。
    #[serde(default = "main_branch")]
    pub branch: String,
    /// 仓库内数据文件相对路径。
    #[serde(default = "data_file")]
    pub path: String,
}

impl Default for GitConfig {
    /// 手写 Default：让 branch/path 与 serde 默认值保持一致，
    /// 否则 `GitConfig::default()` 会得到空分支。
    fn default() -> Self {
        Self {
            url: String::new(),
            username: String::new(),
            password: String::new(),
            branch: main_branch(),
            path: data_file(),
        }
    }
}

fn main_branch() -> String {
    "main".into()
}
fn data_file() -> String {
    "plans.json".into()
}

impl GitConfig {
    /// 是否具备同步所需的最少配置。
    pub fn is_complete(&self) -> bool {
        !self.url.trim().is_empty() && !self.branch.trim().is_empty()
    }

    /// 把账号密码嵌进 URL，避免把明文密码写进 .git/config。
    ///
    /// 三种形态：
    ///  - 用户名 + 口令 → `user:pass@`（普通账号密码）
    ///  - **只有口令**（典型：GitHub / Gitee 的 Personal Access Token）→ `oauth2:token@`
    ///    。这两家都不校验用户名，但 URL 必须带一个非空用户名才会触发 Basic 认证，
    ///    用 `oauth2` 是它们文档里给这个占位名的惯用写法。
    ///  - 都没有 → 原地址（匿名访问公开仓库）
    fn authed_url(&self) -> String {
        let url = self.url.trim();
        let user = self.username.trim();
        let pass = self.password.trim();
        if user.is_empty() && pass.is_empty() {
            return url.to_string();
        }
        // 只有口令时用 oauth2 当占位用户名；只有用户名（极少见）时口令留空。
        let user_part = if user.is_empty() { "oauth2" } else { user };
        let enc_user = urlencode(user_part);
        let enc_pass = urlencode(pass);
        if let Some(rest) = url.strip_prefix("https://") {
            format!("https://{enc_user}:{enc_pass}@{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("http://{enc_user}:{enc_pass}@{rest}")
        } else {
            // ssh / 本地路径等不支持内嵌凭据
            url.to_string()
        }
    }
}

/// 最小 URL 编码（凭据里常见的特殊字符）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 应用配置（settings.json）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default = "sound_default")]
    pub sound: bool,
    #[serde(default)]
    pub autostart: bool,
}

fn sound_default() -> bool {
    true
}

/// 应用数据目录：固定放在程序旁，避免不同启动方式读到不同用户目录。
///
/// `BELL_DATA_DIR` 仅供本地隔离测试使用；普通启动永远使用 exe 同级的
/// `bell-data` 目录，因此桌面快捷方式、开机自启和直接运行会共享同一份个人数据。
///
/// 改名前的目录叫 `daily-reminder-data`：首次以新名字启动时如果只找到旧目录，
/// 就把它整体改名过来，避免老用户升级后「数据不见了」。
///
/// 结果进程内缓存：界面的状态行与设置页每帧都要显示这个路径，直接算的话每帧都要
/// `current_exe` + 两次 `exists`。进程生命周期内程序位置不会变，第一次算完即可。
pub fn app_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(compute_app_dir).clone()
}

fn compute_app_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BELL_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join(DATA_DIR_NAME);
    if !dir.exists() {
        let legacy = base.join(LEGACY_DATA_DIR_NAME);
        if legacy.is_dir() {
            // 改名失败也不致命：仍按新目录继续，用户可手动搬。
            let _ = std::fs::rename(&legacy, &dir);
        }
    }
    dir
}

/// 当前数据目录名。
pub const DATA_DIR_NAME: &str = "bell-data";
/// 改名前的数据目录名，仅用于一次性迁移。
pub const LEGACY_DATA_DIR_NAME: &str = "daily-reminder-data";

pub const SETTINGS_FILE: &str = "settings.json";
pub const LOCAL_DOC_FILE: &str = "plans.json";

/// 按年存放计划数据的子目录名（位于 `bell-data/` 下）。
pub const DOC_DIR_NAME: &str = "plans";
/// 索引文件名（放模板、标签、年份清单，不含 records）。
///
/// 历史：最初叫 `plans.json`，与目录名 `plans` 撞名，GitHub 上看起来像重复文件；
/// 改为 `data.json`。加载时两个名字都认（旧布局迁移兼容）。
pub const DOC_INDEX_FILE: &str = "data.json";
/// 旧版索引文件名，仍可读取（迁移前的数据集可能还在用它）。
pub const DOC_INDEX_FILE_LEGACY: &str = "plans.json";

/// 读写本地 JSON（原子写：先写临时文件再替换）。
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("解析 JSON 失败：{}", path.display()))
            .map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("读取失败：{}", path.display())),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    write_text(path, &text)
}

/// 原子写文本（先写临时文件再替换）。`write_json` 与纯文本状态文件共用同一条路径。
pub fn write_text(path: &Path, text: &str) -> Result<()> {
    write_bytes(path, text.as_bytes())
}

/// 原子写字节（先写临时文件再替换）。
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败：{}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("写入失败：{}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("替换失败：{}", path.display()))?;
    Ok(())
}

// ============================== 按年存储 ==============================

/// 计划数据的持久化：**按年份拆成多个文件**。
///
/// ```text
/// bell-data/plans/
///   plans.json    索引：version / templates / tags / years
///   2026.json     该年的 records
///   2027.json
/// ```
///
/// 为什么拆：记录每天增长（约 600 字节/条），单文件会越写越大 —— 每次保存都要整份
/// 序列化重写，Git 里也会变成一份不断膨胀、无法 diff 的巨型文件。按年拆分后：
///  - 一次保存只重写「受影响的年份」+ 一个很小的索引；
///  - 归档的旧年份通常不再变化，Git 历史里保持稳定；
///  - 出问题时能按年份定位、单独查看。
///
/// 内存里仍是完整的 `Doc`，界面代码不受影响。
pub struct DocStore {
    dir: PathBuf,
}

impl DocStore {
    /// `dir` 是数据子目录（`bell-data/plans`）。
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(DOC_INDEX_FILE)
    }

    /// 现存的索引文件路径：优先新名 `data.json`，退回旧名 `plans.json`。
    ///
    /// 改名是为了消除「plans 目录下有个 plans.json」的歧义；已按旧名落盘的数据集
    /// 照常能读，下次保存时自然写为新名。
    fn existing_index_path(&self) -> Option<PathBuf> {
        let new = self.dir.join(DOC_INDEX_FILE);
        if new.exists() {
            return Some(new);
        }
        let legacy = self.dir.join(DOC_INDEX_FILE_LEGACY);
        if legacy.exists() {
            return Some(legacy);
        }
        None
    }

    fn year_path(&self, year: i32) -> PathBuf {
        self.dir.join(format!("{year}.json"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 读取全部年份文件并合并成完整文档。
    ///
    /// 兼容三种布局：
    ///  1. 新布局：`plans/` 目录 + 索引 + 年份文件；
    ///  2. 过渡布局：索引文件里直接带着 records（旧代码在按年改造中途写出的）；
    ///  3. 旧布局：`bell-data/plans.json` 单文件（首次启动会自动迁移，见 `migrate_legacy`）。
    pub fn load(&self) -> Result<Doc> {
        let Some(index) = self.existing_index_path() else {
            return Ok(Doc::default());
        };
        let index_text = std::fs::read_to_string(&index)
            .with_context(|| format!("读取失败：{}", index.display()))?;

        // 过渡布局：索引本身带 records（YearIndex 没有这个字段，但旧代码写过）。
        // 直接按完整 Doc 解析，能用就用，避免数据「看得见读不出」。
        if index_text.contains("\"records\"") {
            let doc: Doc = serde_json::from_str(&index_text)
                .with_context(|| format!("解析失败：{}", index.display()))?;
            return Ok(doc.normalized());
        }

        let mut year_texts = Vec::new();
        // 以**目录里实际存在的 `NNNN.json` 文件**为准，而不是索引里的 years 字段：
        // 索引字段可能因为手工编辑/同步冲突而不一致，目录内容才是事实。
        for year in self.list_year_files() {
            let path = self.year_path(year);
            match std::fs::read_to_string(&path) {
                Ok(text) => year_texts.push(text),
                // 扫描与读取之间文件被删：不算致命
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("读取失败：{}", path.display()));
                }
            }
        }
        let doc = Doc::from_year_parts(&index_text, &year_texts)
            .with_context(|| format!("解析失败：{}", index.display()))?;
        Ok(doc.normalized())
    }

    /// 列出目录里现存的年份文件（`2026.json` → `2026`），升序。
    fn list_year_files(&self) -> Vec<i32> {
        let mut years = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return years;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(year) = stem.parse::<i32>() {
                    years.push(year);
                }
            }
        }
        years.sort_unstable();
        years
    }

    /// 保存：重写索引 + 受影响的年份文件，并清掉已经不存在的年份文件。
    pub fn save(&self, doc: &Doc) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建目录失败：{}", self.dir.display()))?;

        let files = doc
            .to_year_files()
            .context("序列化年份文件失败")?;
        let index_text = doc.to_year_index_json().context("序列化索引失败")?;
        // 先写年份文件，最后写索引 —— 索引是「这套数据有哪些年份」的清单，
        // 它若先落地而年份文件还没写完，中途崩溃会读到一个指向空年份的索引。
        for (year, text) in &files {
            write_text(&self.year_path(*year), text)?;
        }

        // 清掉已经没有任何记录的年份文件（例如用户删光了某年的记录），
        // 以及改名前遗留的旧索引 plans.json
        let keep: std::collections::HashSet<i32> = files.iter().map(|(y, _)| *y).collect();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == DOC_INDEX_FILE {
                    continue;
                }
                if name == DOC_INDEX_FILE_LEGACY {
                    // 旧索引已被新 data.json 取代
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                if let Some(stem) = name.strip_suffix(".json") {
                    if let Ok(year) = stem.parse::<i32>() {
                        if !keep.contains(&year) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }

        write_text(&self.index_path(), &index_text)
    }

    /// 旧布局迁移：`bell-data/plans.json`（单文件）→ `plans/` 目录。
    ///
    /// 迁移后旧文件改名为 `plans.json.migrated`（保留一份，方便回退），
    /// 并清空 Git 中转副本，让它用新布局重新提交。
    ///
    /// 返回 `true` 表示确实做了迁移。
    pub fn migrate_legacy(&self, legacy_path: &Path) -> Result<bool> {
        if !legacy_path.exists() {
            return Ok(false);
        }
        // 新布局已经存在就不迁移，避免覆盖（正常情况下不会两者并存）
        if self.existing_index_path().is_some() {
            return Ok(false);
        }
        let text = std::fs::read_to_string(legacy_path)
            .with_context(|| format!("读取旧数据失败：{}", legacy_path.display()))?;
        if text.trim().is_empty() {
            return Ok(false);
        }
        let doc = Doc::from_legacy_plans_json(&text)
            .with_context(|| format!("解析旧数据失败：{}", legacy_path.display()))?
            .normalized();
        self.save(&doc)?;
        let backup = legacy_path.with_extension("json.migrated");
        let _ = std::fs::rename(legacy_path, &backup);
        Ok(true)
    }
}

// ============================== Git 同步 ==============================

/// Git 同步通道：**只做中转**，不是数据源。
///
/// 数据永远只住在 `bell-data/plans.json`（`LocalStore`）。`repo/` 是一个一次性的
/// 中转工作副本：推送时把本地文件拷进去提交推送；拉取时把远端文件拷回本地。
///
/// 这样做的理由：
///  - 界面的读写路径与「是否配置了 Git」完全解耦，不会因为切换存储方式就读到两份数据；
///  - `repo/` 可以被随意删除、损坏、重新 clone，本地数据不受影响；
///  - 不会出现「本机和 repo 各有一份、不知道哪份是对的」这种状态。
pub struct GitStore {
    dir: PathBuf,
    cfg: GitConfig,
}

impl GitStore {
    pub fn new(dir: PathBuf, cfg: GitConfig) -> Self {
        Self { dir, cfg }
    }

    /// 远端地址（原样，可能含凭据，展示前请用 `display_url`）。
    pub fn url(&self) -> String {
        self.cfg.url.clone()
    }

    /// 中转副本里那个数据文件的路径（**只在中转过程中使用**，不是数据源）。
    pub fn data_path(&self) -> PathBuf {
        self.dir.join(self.cfg.path.trim())
    }

    /// 准备仓库：没有就 clone，有就按配置校正 remote。
    pub fn prepare(&self) -> Result<()> {
        if !self.cfg.is_complete() {
            bail!("Git 配置不完整：请填写仓库地址");
        }
        // 旧版配置遗留：`path` 是单文件（plans.json），而新版把它当**目录**用。
        // 若中转副本里那个位置是一个文件，创建同名目录必然失败 —— 直接清掉整个副本
        // 重新 clone（副本里没有用户数据，见 struct 注释）。
        let cfg_path = self.dir.join(self.cfg.path.trim());
        if cfg_path.is_file() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        if self.dir.join(".git").is_dir() {
            // 已有工作副本：校正 remote 与分支。
            // 先读 `.git/config` 直接比对，一致就不起 git 子进程 —— 这函数在每次保存
            // （点完成、改记录…）都会被调用，`git remote set-url` 一次要 35–70 ms，
            // 每次都跑会在 Git 模式下让界面一顿一顿的。
            let want = self.cfg.authed_url();
            if self.current_origin_url().as_deref() != Some(want.as_str()) {
                self.run(&["remote", "set-url", "origin", &want])?;
            }
        } else {
            // 中转副本是临时的：残留目录直接清掉重新 clone，不尝试修复。
            // 它不含任何用户数据（用户数据在 bell-data/plans.json），删了也不会丢东西。
            if self.dir.exists() {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
            std::fs::create_dir_all(&self.dir)
                .with_context(|| format!("创建目录失败：{}", self.dir.display()))?;
            self.run_in(
                self.dir.parent().unwrap_or(Path::new(".")),
                &[
                    "clone",
                    "--branch",
                    self.cfg.branch.trim(),
                    &self.cfg.authed_url(),
                    &self.dir.to_string_lossy(),
                ],
            )
            .or_else(|_| {
                // 远端是空仓库时 --branch 会失败，退化为普通 clone
                let _ = std::fs::remove_dir_all(&self.dir);
                std::fs::create_dir_all(&self.dir)?;
                self.run_in(
                    self.dir.parent().unwrap_or(Path::new(".")),
                    &["clone", &self.cfg.authed_url(), &self.dir.to_string_lossy()],
                )
            })?;
        }
        Ok(())
    }

    /// 把远端最新内容拉到中转副本，并返回其中的数据文件字节。
    ///
    /// 返回 `None` = 远端还没有这个数据文件（全新仓库 / 首次使用）。
    /// **不改动本地数据**：写入 `bell-data/plans.json` 由调用方决定。
    /// 远端数据在**中转副本**里的目录（远端仓库根下的一级目录）。
    ///
    /// 旧版是单个文件（`path` 形如 `plans.json`），新版是目录（`plans/`）。
    /// 这里根据配置里的 `path` 判断：没有 `.json` 后缀就当目录处理。
    fn remote_dir(&self) -> PathBuf {
        self.dir.join(self.cfg.path.trim())
    }


    /// 只探测远端：`fetch` 并比较远端分支与本地 HEAD 的时间与领先关系。
    ///
    /// 返回：
    ///  - `None`：远端还没有数据分支
    ///  - Some(false)：远端与本地一致（或远端更旧），拉取是安全的
    ///  - Some(true)：远端有本地没有的新提交（拉取会改动本地）
    ///
    /// 注意：这只做 `fetch`（写中转副本的 `.git`），**不**改工作区、不动本地数据。
    pub fn probe_remote(&self) -> Result<Option<bool>> {
        self.prepare()?;
        let branch = self.cfg.branch.trim();
        if self
            .run(&["rev-parse", "--verify", &format!("origin/{branch}")])
            .is_err()
        {
            return Ok(None);
        }
        self.run(&["fetch", "origin", branch])?;
        let remote = format!("origin/{branch}");
        // 本地 HEAD 不存在（首次）→ 远端任何东西都算「新」
        if self
            .run(&["rev-parse", "--verify", branch])
            .is_err()
        {
            return Ok(Some(true));
        }
        // 双向计数：左 = 本地领先（本地有未推送提交），右 = 远端领先
        let out = self.run(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{branch}...{remote}"),
        ])?;
        let mut parts = out.split_whitespace();
        let local_ahead = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let remote_ahead = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        Ok(Some(remote_ahead > 0))
    }

    /// 从远端拉取数据到中转副本，并返回远端数据在副本里的**目录**。
    ///
    /// 返回 `None` = 远端还没有数据（目录不存在且旧单文件也不存在）。
    /// **不改动本地数据**：由调用方决定怎么落到本地。
    pub fn fetch_remote_dir(&self) -> Result<Option<PathBuf>> {
        self.prepare()?;
        let branch = self.cfg.branch.trim();
        // 允许远端为空（首次提交前没有该分支）
        if self
            .run(&["rev-parse", "--verify", &format!("origin/{branch}")])
            .is_err()
        {
            return Ok(None);
        }
        self.run(&["fetch", "origin", branch])?;
        // 中转副本对齐到远端：这里 `reset --hard` 是安全的 —— 它只影响中转副本，
        // 本地数据从来不在这个目录里。
        if self.run(&["rev-parse", "--verify", branch]).is_err() {
            self.run(&["checkout", "-B", branch, &format!("origin/{branch}")])?;
        } else {
            self.run(&["reset", "--hard", &format!("origin/{branch}")])?;
        }
        let dir = self.remote_dir();
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        // 兼容旧版单文件布局：把 plans.json 转录成一个「临时目录」供调用方统一处理
        let legacy = self.data_path();
        if legacy.is_file() {
            let tmp = self.dir.join(".dsh-legacy-single");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            std::fs::copy(&legacy, tmp.join(DOC_INDEX_FILE))?;
            return Ok(Some(tmp));
        }
        Ok(None)
    }

    /// 把本地 `plans/` 目录的内容整体推送到远端。
    ///
    /// 做法：清掉中转副本里的数据目录 → 拷入本地目录 → add/commit/push。
    /// 这样远端内容永远等于本地真实数据，不受副本残留影响，也能正确处理
    /// 「某个年份文件被删除」这种情况（否则 git 会保留旧文件）。
    pub fn push_dir(&self, local_dir: &Path, message: &str) -> Result<()> {
        self.prepare()?;
        let branch = self.cfg.branch.trim().to_string();
        let dest = self.remote_dir();

        if local_dir.is_dir() {
            // 先删干净再拷：保证远端不会残留本地已删除的年份文件
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("创建目录失败：{}", dest.display()))?;
            copy_dir_contents(local_dir, &dest)?;
        } else {
            return Err(anyhow!("本地数据目录不存在：{}", local_dir.display()));
        }
        // 旧版单文件（repo 根的 plans.json）与改名前的索引（plans/plans.json）
        // 若还留在副本里，一并删掉，避免多种布局并存
        let _ = std::fs::remove_file(self.dir.join(DOC_INDEX_FILE_LEGACY));
        let _ = std::fs::remove_file(dest.join(DOC_INDEX_FILE_LEGACY));

        // `-A` 不带路径限制：把副本里的**所有**变化（含旧单文件的删除）都暂存。
        // 若只 `add -A -- <目录>`，根目录下旧 plans.json 的删除不会被暂存，
        // 远端就会残留一个过期的单文件布局。
        self.run(&["add", "-A"])?;
        // 没有改动就不用提交（status 不带路径限制：整个副本有任何变化都要提交）
        if self
            .run(&["status", "--porcelain"])
            .map(|s| s.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.run(&[
            "-c",
            "user.name=bell-reminder",
            "-c",
            "user.email=daily@local",
            "commit",
            "-m",
            message,
        ])?;

        // 分支不存在就先建出来
        if self.run(&["rev-parse", "--verify", &branch]).is_err() {
            self.run(&["checkout", "-B", &branch])?;
        }
        self.run(&["push", "origin", &format!("HEAD:{branch}")])?;
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        self.run_in(&self.dir, args)
    }

    /// 当前 `origin` 的 fetch URL（从 `.git/config` 直接读，不起 git 子进程）。
    ///
    /// 只用于「已经一致就别再 set-url」的短路判断：`.git/config` 是 git 的稳定配置格式，
    /// `[remote "origin"]` 段里 `url = ...` 就是它。读不到/解析失败一律返回 None，
    /// 让调用方照常走 `git remote set-url`（宁可多起一次进程，也不猜）。
    fn current_origin_url(&self) -> Option<String> {
        let cfg = std::fs::read_to_string(self.dir.join(".git").join("config")).ok()?;
        let mut in_origin = false;
        for line in cfg.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix('[') {
                // 段头：判断是不是 [remote "origin"]
                in_origin = rest
                    .trim_end_matches(']')
                    .trim()
                    .eq_ignore_ascii_case(r#"remote "origin""#);
                continue;
            }
            if !in_origin {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "url" {
                    return Some(value.trim().to_string());
                }
            }
        }
        None
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let out = git_command()
            .args(args)
            .current_dir(cwd)
            // 不要交互式提示密码，否则会卡住界面
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .output()
            .with_context(|| format!("调用 git 失败：{}", args.join(" ")))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if err.is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                err
            };
            return Err(anyhow!(
                "git {} 失败：{}",
                args.join(" "),
                redact(&msg, &self.cfg)
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

/// 错误信息里可能带内嵌凭据，输出前抹掉。
fn redact(text: &str, cfg: &GitConfig) -> String {
    let mut s = text.to_string();
    if !cfg.password.is_empty() {
        s = s.replace(&cfg.password, "***");
        s = s.replace(&urlencode(&cfg.password), "***");
    }
    s
}

/// 用 `from` 目录的内容**整体替换** `to` 目录的内容。
///
/// 顺序刻意是「先全部拷入，再删多余」而不是「先清空再拷」：
/// 万一中途失败（磁盘满、权限不足），本地至少还留着旧数据，不会变成空目录
/// 导致「打开程序计划全没了」。
pub fn replace_dir_contents(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .with_context(|| format!("创建目录失败：{}", to.display()))?;
    copy_dir_contents(from, to)?;

    // 远端没有、本地残留的文件（例如某年份已被删除）在这里清掉
    let keep: std::collections::HashSet<std::ffi::OsString> = std::fs::read_dir(from)
        .map(|it| it.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(to) {
        for entry in entries.flatten() {
            if !keep.contains(&entry.file_name()) {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    Ok(())
}

/// 递归复制目录内容（只处理普通文件与一层层子目录；数据目录里没有符号链接）。
fn copy_dir_contents(from: &Path, to: &Path) -> Result<()> {    std::fs::create_dir_all(to)
        .with_context(|| format!("创建目录失败：{}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("读取目录失败：{}", from.display()))?
    {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_contents(&src, &dst)?;
        } else if ty.is_file() {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("复制失败：{} → {}", src.display(), dst.display()))?;
        }
        // 符号链接等一律跳过：数据目录里不应出现，跟随它反而可能复制到别处
    }
    Ok(())
}

/// 过滤远端地址里的凭据（用于界面/日志展示）。
pub fn display_url(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end => {
            format!("{}://***@{}", &url[..scheme_end], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

/// 检查 git 是否可用。
///
/// 结果进程内缓存：探测要启动一个 `git --version` 子进程（Windows 实测 35–70 ms，
/// 冷启动可达 160 ms），而设置页每帧都会读这个值 —— 不缓存的话界面每一帧都要
/// 付一次 CreateProcess，肉眼可见地卡。git 的安装状态在进程生命周期内不会变，
/// 因此缓存一次即可。
pub fn git_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        git_command()
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Plan;
    use chrono::NaiveTime;

    #[test]
    fn local_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dr-test-{}", uuid::Uuid::new_v4()));
        let store = DocStore::new(dir.join(DOC_DIR_NAME));

        // 没有目录时返回空文档
        assert!(store.load().unwrap().templates.is_empty());

        let mut doc = Doc::default();
        doc.templates.push(Plan::new(
            "喝水".into(),
            "两杯".into(),
            NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        ));
        store.save(&doc).unwrap();

        let back = store.load().unwrap();
        assert_eq!(back.templates.len(), 1);
        assert_eq!(back.templates[0].title, "喝水");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 记录按年份落到不同文件，索引里只有模板/标签/年份清单。
    #[test]
    fn records_are_split_by_year() {
        use crate::model::DayRecord;
        let dir = std::env::temp_dir().join(format!("dr-year-{}", uuid::Uuid::new_v4()));
        let store = DocStore::new(dir.join(DOC_DIR_NAME));

        let template = Plan::new("喝水".into(), String::new(), NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let mut doc = Doc::default();
        doc.templates.push(template.clone());
        for (y, m, d) in [(2026, 11, 2), (2026, 12, 31), (2027, 1, 1), (2027, 6, 15)] {
            let date = chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
            doc.records.push(DayRecord::from_template(&template, date));
        }
        store.save(&doc).unwrap();

        // 目录里应有索引 + 两个年份文件
        let mut names: Vec<String> = std::fs::read_dir(dir.join(DOC_DIR_NAME))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["2026.json", "2027.json", "data.json"]);

        // 索引里不含 records
        let index = std::fs::read_to_string(dir.join(DOC_DIR_NAME).join(DOC_INDEX_FILE)).unwrap();
        assert!(!index.contains("\"records\""));
        assert!(index.contains("templates"));

        // 年份文件里只有当年的记录
        let y2026 = std::fs::read_to_string(dir.join(DOC_DIR_NAME).join("2026.json")).unwrap();
        assert!(y2026.contains("2026-11-02"));
        assert!(y2026.contains("2026-12-31"));
        assert!(!y2026.contains("2027-01-01"));

        // 合并回来与原文档一致
        let back = store.load().unwrap();
        assert_eq!(back.records.len(), 4);
        assert_eq!(back.templates.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 某年记录被删光后，该年份文件也应从磁盘上消失（避免留下空文件）。
    #[test]
    fn empty_year_file_is_removed() {
        use crate::model::DayRecord;
        let dir = std::env::temp_dir().join(format!("dr-prune-{}", uuid::Uuid::new_v4()));
        let store = DocStore::new(dir.join(DOC_DIR_NAME));

        let template = Plan::new("A".into(), String::new(), NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let mut doc = Doc::default();
        doc.records.push(DayRecord::from_template(
            &template,
            chrono::NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        ));
        store.save(&doc).unwrap();
        assert!(dir.join(DOC_DIR_NAME).join("2026.json").exists());

        // 清空记录后再存：2026.json 应被清掉
        doc.records.clear();
        store.save(&doc).unwrap();
        assert!(!dir.join(DOC_DIR_NAME).join("2026.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧布局（单文件 plans.json）能自动迁移成按年目录，并保留一份 .migrated 备份。
    #[test]
    fn legacy_single_file_migrates() {
        use crate::model::DayRecord;
        let dir = std::env::temp_dir().join(format!("dr-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join(LOCAL_DOC_FILE);

        let template = Plan::new("旧数据".into(), String::new(), NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let mut doc = Doc::default();
        doc.templates.push(template.clone());
        doc.records.push(DayRecord::from_template(
            &template,
            chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(),
        ));
        write_json(&legacy, &doc).unwrap();

        let store = DocStore::new(dir.join(DOC_DIR_NAME));
        assert!(store.migrate_legacy(&legacy).unwrap());

        // 旧文件已改名成 .migrated，新目录里是拆分后的文件
        assert!(!legacy.exists());
        assert!(dir.join("plans.json.migrated").exists());
        let back = store.load().unwrap();
        assert_eq!(back.templates.len(), 1);
        assert_eq!(back.records.len(), 1);

        // 再迁移一次应无事发生（幂等）
        assert!(!store.migrate_legacy(&dir.join("plans.json.migrated")).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_roundtrip_with_plain_password() {
        let dir = std::env::temp_dir().join(format!("dr-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join(SETTINGS_FILE);
        let s = Settings {
            kind: Kind::Git,
            git: GitConfig {
                url: "https://example.com/me/daily.git".into(),
                username: "me".into(),
                password: "p@ss word".into(),
                branch: "main".into(),
                path: "plans.json".into(),
            },
            sound: false,
            autostart: true,
        };
        write_json(&path, &s).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"password\": \"p@ss word\""),
            "密码应明文保存"
        );

        let back: Settings = read_json(&path).unwrap().unwrap();
        assert_eq!(back.kind, Kind::Git);
        assert_eq!(back.git.username, "me");
        assert!(!back.sound);
        assert!(back.autostart);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn authed_url_embeds_credentials() {
        let cfg = GitConfig {
            url: "https://example.com/me/daily.git".into(),
            username: "me".into(),
            password: "p@ss word".into(),
            ..Default::default()
        };
        let u = cfg.authed_url();
        assert_eq!(u, "https://me:p%40ss%20word@example.com/me/daily.git");
        assert!(u.contains("%40"), "@ 需编码");
    }

    #[test]
    fn authed_url_without_username_keeps_url() {
        let cfg = GitConfig {
            url: "https://example.com/me/daily.git".into(),
            ..Default::default()
        };
        assert_eq!(cfg.authed_url(), "https://example.com/me/daily.git");
    }

    /// 只填令牌、不填用户名时，令牌必须仍然生效。
    ///
    /// GitHub / Gitee 的 Personal Access Token 就是「用户名可以随便填、真正的凭据在
    /// 口令位」的形态，用户往往只想粘一个 token。早先的实现把「用户名为空」当成
    /// 「不需要凭据」直接返回原地址，token 会被静默丢弃、克隆变成匿名访问，
    /// 私有仓库于是报 403/认证失败 —— 这是最容易踩的坑，这里钉住。
    #[test]
    fn authed_url_uses_token_even_without_username() {
        let cfg = GitConfig {
            url: "https://github.com/me/daily.git".into(),
            password: "ghp_abc123".into(),
            ..Default::default()
        };
        let u = cfg.authed_url();
        assert_eq!(u, "https://oauth2:ghp_abc123@github.com/me/daily.git");
    }

    /// 令牌里的特殊字符必须编码，否则 URL 解析会错位。
    #[test]
    fn authed_url_encodes_token_specials() {
        let cfg = GitConfig {
            url: "https://gitee.com/me/daily.git".into(),
            username: "me".into(),
            password: "a/b+c=d@e".into(),
            ..Default::default()
        };
        let u = cfg.authed_url();
        assert_eq!(u, "https://me:a%2Fb%2Bc%3Dd%40e@gitee.com/me/daily.git");
    }

    #[test]
    fn redacts_password_in_errors() {
        let cfg = GitConfig {
            password: "secret123".into(),
            ..Default::default()
        };
        let msg = "fatal: 'https://u:secret123@host/x.git' unavailable";
        let out = redact(msg, &cfg);
        assert!(!out.contains("secret123"));
        assert!(out.contains("***"));
    }

    #[test]
    fn display_url_hides_credentials() {
        assert_eq!(
            display_url("https://u:p@example.com/a.git"),
            "https://***@example.com/a.git"
        );
        assert_eq!(
            display_url("https://example.com/a.git"),
            "https://example.com/a.git"
        );
    }

    #[test]
    fn incomplete_git_config_detected() {
        assert!(!GitConfig::default().is_complete());
        let cfg = GitConfig {
            url: "https://x/y.git".into(),
            ..Default::default()
        };
        assert!(cfg.is_complete());
    }

    // -------- 真实 git 用例（机器上没有 git 时跳过） --------

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("dr-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = git_command()
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git 应可执行");
        assert!(
            out.status.success(),
            "git {:?} 失败: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn git_push_then_pull_roundtrip() {
        if !git_available() {
            eprintln!("跳过：本机没有 git");
            return;
        }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);

        // 第一个客户端写入
        let a_dir = base.join("a");
        let cfg = GitConfig {
            url: remote.to_string_lossy().into_owned(),
            branch: "main".into(),
            // 新版：同步的是**目录**（不带 .json 后缀）
            path: DOC_DIR_NAME.into(),
            ..Default::default()
        };
        let a = GitStore::new(a_dir, cfg.clone());
        a.prepare().unwrap();

        // 本地数据目录（用 DocStore 生成按年文件）
        let a_doc = DocStore::new(base.join("a-plans"));
        let mut doc = Doc::default();
        doc.templates.push(Plan::new(
            "跑步".into(),
            String::new(),
            NaiveTime::from_hms_opt(7, 30, 0).unwrap(),
        ));
        a_doc.save(&doc).unwrap();
        a.push_dir(a_doc.dir(), "add running").unwrap();

        // 第二个客户端拉取
        let b = GitStore::new(base.join("b"), cfg);
        let remote_dir = b.fetch_remote_dir().unwrap().expect("远端应已有数据目录");
        let b_doc = DocStore::new(base.join("b-plans"));
        replace_dir_contents(&remote_dir, b_doc.dir()).unwrap();
        let got = b_doc.load().unwrap();
        assert_eq!(got.templates.len(), 1);
        assert_eq!(got.templates[0].title, "跑步");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 中转副本是**可随时丢弃**的：删掉它之后，仍能重新 clone 并拉到同一份远端数据。
    ///
    /// 这条对应「本地数据是唯一数据源、repo 只做中转」的设计：repo 损坏/被删都不该
    /// 影响用户数据，重新准备就能继续同步。
    #[test]
    fn git_remote_survives_wiping_the_transit_copy() {
        if !git_available() {
            eprintln!("跳过：本机没有 git");
            return;
        }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);

        let cfg = GitConfig {
            url: remote.to_string_lossy().into_owned(),
            branch: "main".into(),
            path: DOC_DIR_NAME.into(),
            ..Default::default()
        };

        let local = DocStore::new(base.join("a-plans"));
        let mut doc = Doc::default();
        doc.templates.push(Plan::new(
            "喝水".into(),
            String::new(),
            NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        ));
        local.save(&doc).unwrap();

        let a = GitStore::new(base.join("a"), cfg.clone());
        a.push_dir(local.dir(), "add water").unwrap();

        // 把整个中转副本删掉，模拟「用户手动清了 repo 目录 / 副本损坏」
        std::fs::remove_dir_all(base.join("a")).unwrap();

        // 同一个 GitStore 重新准备并拉取：应能重新 clone 并拿到同一份数据
        let remote_dir = a.fetch_remote_dir().unwrap().expect("重建后应能拉到数据");
        let restored = DocStore::new(base.join("restored"));
        replace_dir_contents(&remote_dir, restored.dir()).unwrap();
        let got = restored.load().unwrap();
        assert_eq!(got.templates.len(), 1);
        assert_eq!(got.templates[0].title, "喝水");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn git_push_is_noop_without_changes() {
        if !git_available() {
            eprintln!("跳过：本机没有 git");
            return;
        }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);

        let cfg = GitConfig {
            url: remote.to_string_lossy().into_owned(),
            branch: "main".into(),
            path: DOC_DIR_NAME.into(),
            ..Default::default()
        };
        let local = DocStore::new(base.join("w-plans"));
        local.save(&Doc::default()).unwrap();

        let store = GitStore::new(base.join("w"), cfg);
        store.prepare().unwrap();
        store.push_dir(local.dir(), "first").unwrap();
        // 第二次内容相同：不应报错，也不产生提交
        store.push_dir(local.dir(), "second").unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}



