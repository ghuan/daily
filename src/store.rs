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

/// Git 同步通道：**数据目录本身就是 git 工作副本**。
///
/// `dir` 直接指向 `bell-data/plans/` —— 那里既是 `DocStore` 的数据源，也是
/// git 的工作区（`.git` 子目录放仓库元数据）。没有独立的中转副本：pull 就是
/// `fetch + reset --hard`（直接作用在数据上），push 就是 `add -A + commit + push`。
///
/// 目录里因此会多出一个 `plans/.git/` 子目录；它对 `DocStore::load` 透明
/// （load 只认 `NNNN.json` 和 `data.json`），对用户也只是个不可见的文件夹。
/// 注意：**不要**把 `plans/` 挪动或整目录删除来「重置」—— 那会连历史一起丢掉；
/// 重置请用设置页切换到本地再切回来。
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

    /// 准备仓库：数据目录没有 `.git` 就地初始化并接上远端；有则校正 remote。
    ///
    /// 初始化流程：`git init` → `remote add origin` → `fetch` → 远端有数据时
    /// `reset --hard origin/<branch>`（把远端内容铺进数据目录）→ `checkout -B`。
    /// 若数据目录里已有本地数据而远端为空，fetch 会成功但没有分支 —— 此时不
    /// reset，保持本地数据原样，等第一次 push 建分支。
    pub fn prepare(&self) -> Result<()> {
        if !self.cfg.is_complete() {
            bail!("Git 配置不完整：请填写仓库地址");
        }
        let want = self.cfg.authed_url();
        if self.dir.join(".git").is_dir() {
            // 已有仓库：remote 一致就不动（省一次 35–70ms 的 git 子进程）
            if self.current_origin_url().as_deref() != Some(want.as_str()) {
                self.run(&["remote", "set-url", "origin", &want])?;
            }
            return Ok(());
        }
        // 就地初始化
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建目录失败：{}", self.dir.display()))?;
        self.run(&["init"])?;
        self.run(&["remote", "add", "origin", &want])?;
        let branch = self.cfg.branch.trim();
        // 远端有内容就铺进来；空仓库（或远端不可达时的失败）都容忍 ——
        // 本地数据保持原样，等首次推送。
        if self.run(&["fetch", "origin", branch]).is_ok()
            && self
                .run(&["rev-parse", "--verify", &format!("origin/{branch}")])
                .is_ok()
        {
            self.run(&["reset", "--hard", &format!("origin/{branch}")])?;
            self.run(&["checkout", "-B", branch, &format!("origin/{branch}")])?;
        }
        Ok(())
    }

    /// 拉取远端最新到数据目录。
    ///
    /// 返回 `true` = 远端有内容且已写入本目录；`false` = 远端还没有分支/数据。
    /// **只能在「本地没有未提交改动」时调用**（即 `sync_round` 判定为纯落后后），
    /// 因为 `reset --hard` 会丢掉工作区未提交内容 —— 而在这个模型里工作区就是
    /// 用户数据本身。
    pub fn pull(&self) -> Result<bool> {
        self.prepare()?;
        let branch = self.cfg.branch.trim();
        if self
            .run(&["rev-parse", "--verify", &format!("origin/{branch}")])
            .is_err()
        {
            return Ok(false);
        }
        self.run(&["fetch", "origin", branch])?;
        self.run(&["reset", "--hard", &format!("origin/{branch}")])?;
        if self.run(&["rev-parse", "--verify", branch]).is_err() {
            self.run(&["checkout", "-B", branch, &format!("origin/{branch}")])?;
        }
        Ok(true)
    }

    /// 把数据目录的当前内容提交并推送。
    ///
    /// 没有 diff 就静默返回（调用方无需预判）。分支不存在（远端为空的首次推送）
    /// 就先建出来。
    pub fn push(&self, message: &str) -> Result<()> {
        self.prepare()?;
        let branch = self.cfg.branch.trim().to_string();
        self.run(&["add", "-A"])?;
        if self
            .run(&["status", "--porcelain"])?
            .trim()
            .is_empty()
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
        if self.run(&["rev-parse", "--verify", &branch]).is_err() {
            self.run(&["checkout", "-B", &branch])?;
        }
        self.run(&["push", "origin", &format!("HEAD:{branch}")])?;
        Ok(())
    }

    /// 本地是否未提交改动（工作区与 HEAD 的差异，含未跟踪文件）。
    fn local_dirty(&self) -> Result<bool> {
        Ok(!self.run(&["status", "--porcelain"])?.trim().is_empty())
    }

    /// 远端分支上有没有本地没有的新提交（需要 fetch 才准确）。
    fn remote_ahead(&self) -> Result<u64> {
        let branch = self.cfg.branch.trim();
        if self
            .run(&["rev-parse", "--verify", &format!("origin/{branch}")])
            .is_err()
        {
            return Ok(0);
        }
        self.run(&["fetch", "origin", branch])?;
        // 本地分支还不存在 → 远端一切内容都算"新"
        if self.run(&["rev-parse", "--verify", branch]).is_err() {
            return Ok(1);
        }
        let out = self.run(&["rev-list", "--count", &format!("{branch}..origin/{branch}")])?;
        Ok(out.trim().parse().unwrap_or(0))
    }

    /// 一轮「拉取 + 推送」的智能同步（有道云笔记式冲突处理）。
    ///
    /// 与 `push`（无脑用本地覆盖远端）不同，这里先**分别探测**两边：
    ///
    /// | 本地有改动 | 远端有新提交 | 动作 |
    /// |---|---|---|
    /// | ✗ | ✗ | `Noop` |
    /// | ✓ | ✗ | 直接推送（`Pushed`） |
    /// | ✗ | ✓ | `reset --hard` 到远端（数据目录直接变成远端内容），返回 `Pulled` |
    /// | ✓ | ✓ | **冲突副本**：本机版本先 commit 到一个 `conflict/` 分支留底，
    ///   然后照常 reset 到远端。本机改动不丢（在分支里），同步不被打断 |
    ///
    /// 有道云笔记的冲突策略是「生成冲突副本，保留两个版本」，用户事后自行
    /// 核对合并 —— 从不打断正在编辑的人。这里用 git 分支实现同样的语义：
    /// 本机改动永远在 `conflict/<时间戳>` 分支里可找回，主数据跟随远端。
    pub fn sync_round(&self, message: &str) -> Result<SyncOutcome> {
        self.prepare()?;
        let branch = self.cfg.branch.trim().to_string();
        let remote_ahead = self.remote_ahead()?;
        let local_dirty = self.local_dirty()?;

        match (local_dirty, remote_ahead > 0) {
            (false, false) => Ok(SyncOutcome::Noop),
            (true, false) => {
                self.push(message)?;
                Ok(SyncOutcome::Pushed)
            }
            (false, true) => {
                // 纯落后：数据目录直接对齐远端
                self.pull()?;
                Ok(SyncOutcome::Pulled)
            }
            (true, true) => {
                // 冲突 → 冲突副本：本机当前状态存进 conflict 分支留底，
                // 然后主数据对齐远端（Pulled），同步流程不中断。
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                let conflict_branch = format!("conflict/{}", stamp);
                self.run(&["checkout", "-b", &conflict_branch])?;
                self.run(&["add", "-A"])?;
                self.run(&[
                    "-c",
                    "user.name=bell-reminder",
                    "-c",
                    "user.email=daily@local",
                    "commit",
                    "-m",
                    &format!("冲突副本：本机改动（原准备：{}）", message),
                ])?;
                // 回到主分支并对齐远端；冲突分支留在本地（也推到远端，多机都能看到）
                self.run(&["checkout", &branch])?;
                let _ = self.run(&["push", "origin", &conflict_branch]);
                self.pull()?;
                Ok(SyncOutcome::ConflictCopied)
            }
        }
    }

    // ==================== 冲突副本管理 ====================

    /// 列出所有冲突副本分支（`conflict/<时间戳>`），按时间倒序。
    ///
    /// 返回 `(分支名, 提交时间, 提交说明)`。时间取自提交的 committer date
    /// （创建副本的时刻），说明即「冲突副本：本机改动（原准备：…）」。
    pub fn list_conflict_branches(&self) -> Result<Vec<(String, String, String)>> {
        if !self.dir.join(".git").is_dir() {
            return Ok(Vec::new());
        }
        // % (objectname) 不需要；refname + committerdate + subject
        let out = match self.run(&[
            "for-each-ref",
            "refs/heads/conflict/*",
            "--sort=-committerdate",
            "--format=%(refname:short)%09%(committerdate:local)%09%(contents:subject)",
        ]) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()), // 没有任何匹配时 git 也返回成功+空串，这里兜底
        };
        let mut list = Vec::new();
        for line in out.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, '\t');
            if let (Some(name), Some(time), Some(subject)) = (parts.next(), parts.next(), parts.next()) {
                list.push((name.trim().to_string(), time.trim().to_string(), subject.trim().to_string()));
            }
        }
        Ok(list)
    }

    /// 读取某份冲突副本的完整数据。
    ///
    /// 把分支上的 `data.json` + `NNNN.json` 拼回一个 `Doc`——副本就是冲突
    /// 时刻本机数据目录的快照，结构与主数据完全一致，可以直接复用解析。
    pub fn load_conflict_doc(&self, branch: &str) -> Result<Doc> {
        if !branch.starts_with("conflict/") {
            bail!("只允许读取 conflict/ 分支");
        }
        // data.json（索引：模板/标签）
        let index_text = self.run(&["show", &format!("{}:{}", branch, DOC_INDEX_FILE)])?;
        // 年份文件：从索引里的 years 字段拿清单（保存时写入），缺失就跳过
        #[derive(Deserialize)]
        struct YearsProbe {
            #[serde(default)]
            years: Vec<i32>,
        }
        let years: Vec<i32> = serde_json::from_str::<YearsProbe>(&index_text)
            .map(|p| p.years)
            .unwrap_or_default();
        let mut year_texts = Vec::new();
        for y in years {
            if let Ok(text) = self.run(&["show", &format!("{}:{}.json", branch, y)]) {
                year_texts.push(text);
            }
        }
        Doc::from_year_parts(&index_text, &year_texts)
            .map(|d| d.normalized())
            .with_context(|| format!("解析冲突副本失败：{}", branch))
    }

    /// 删除一份冲突副本（本地 + 远端）。
    ///
    /// 本地分支强制删除（-D，因为副本分支从不在主分支历史里）；远端同名
    /// 分支也一并删除。远端删除失败不阻断——本地已删，下次 push 不会再带它，
    /// 仅远端残留一个孤儿分支，无害。
    pub fn delete_conflict_branch(&self, branch: &str) -> Result<()> {
        if !branch.starts_with("conflict/") {
            bail!("只允许删除 conflict/ 分支");
        }
        self.run(&["branch", "-D", branch])?;
        let _ = self.run(&["push", "origin", "--delete", branch]);
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

/// 一轮完整同步的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 本地有改动且远端无新内容：已推送。
    Pushed,
    /// 本地无改动且远端有新内容：数据目录已对齐到远端，
    /// 调用方需要重新读盘刷新界面。
    Pulled,
    /// 两边都有改动：本机版本已存为冲突分支留底，主数据已对齐远端。
    ConflictCopied,
    /// 两边都没有改动。
    Noop,
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

    // ==================== Git 同步（数据目录即工作副本） ====================

    /// 推送 → 另一端拉取的完整往返。
    #[test]
    fn git_push_then_pull_roundtrip() {
        use crate::model::DayRecord;
        if !git_available() {
            eprintln!("跳过：本机没有 git");
            return;
        }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };

        // A 端：数据目录直接 init 接上远端，写入并推送
        let a_store = DocStore::new(base.join("a"));
        let template = Plan::new("跑步".into(), String::new(), NaiveTime::from_hms_opt(7, 30, 0).unwrap());
        let mut doc = Doc::default();
        doc.templates.push(template.clone());
        doc.records.push(DayRecord::from_template(&template, chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap()));
        a_store.save(&doc).unwrap();
        let a = GitStore::new(base.join("a"), cfg.clone());
        a.prepare().unwrap();
        a.push("add running").unwrap();

        // B 端：空目录 init 接上远端，pull 后应得到 A 的数据
        let b_store = DocStore::new(base.join("b"));
        b_store.save(&Doc::default()).unwrap();
        let b = GitStore::new(base.join("b"), cfg);
        b.prepare().unwrap();
        assert!(b.pull().unwrap());
        let got = b_store.load().unwrap();
        assert_eq!(got.records.len(), 1);
        assert_eq!(got.records[0].title, "跑步");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// pull/push 重复执行是安全的：无 diff 时不产生提交、不报错。
    #[test]
    fn git_repeat_operations_are_noop() {
        if !git_available() { return; }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };
        let ds = DocStore::new(base.join("w"));
        ds.save(&Doc::default()).unwrap();
        let git = GitStore::new(base.join("w"), cfg);
        git.prepare().unwrap();
        git.push("first").unwrap();
        let head1 = git.run(&["rev-parse", "HEAD"]).unwrap();
        // 无变化再推：不应产生新提交
        git.push("second").unwrap();
        let head2 = git.run(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head1, head2, "无 diff 的 push 不应产生提交");
        // 无变化再拉：不报错
        assert!(git.pull().unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// sync_round 四象限：本机无改动 + 远端无新内容 → Noop。
    #[test]
    fn sync_round_noop() {
        if !git_available() { return; }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };
        let ds = DocStore::new(base.join("w"));
        ds.save(&Doc::default()).unwrap();
        let git = GitStore::new(base.join("w"), cfg);
        git.prepare().unwrap();
        git.push("init").unwrap();
        assert_eq!(git.sync_round("x").unwrap(), SyncOutcome::Noop);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// sync_round 四象限：本机有改动 + 远端无新内容 → 直接 Pushed。
    #[test]
    fn sync_round_pushes_local_changes() {
        use crate::model::DayRecord;
        if !git_available() { return; }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };
        let ds = DocStore::new(base.join("w"));
        ds.save(&Doc::default()).unwrap();
        let git = GitStore::new(base.join("w"), cfg.clone());
        git.prepare().unwrap();
        git.push("init").unwrap();

        // 本地加一条记录（模拟延迟队列里的未推送改动）
        let t = Plan::new("本机改动".into(), String::new(), NaiveTime::from_hms_opt(9,0,0).unwrap());
        let mut doc = ds.load().unwrap();
        doc.records.push(DayRecord::from_template(&t, chrono::NaiveDate::from_ymd_opt(2026,11,7).unwrap()));
        ds.save(&doc).unwrap();

        assert_eq!(git.sync_round("push local").unwrap(), SyncOutcome::Pushed);
        // 远端应包含本机改动
        let check = GitStore::new(base.join("check"), cfg);
        check.prepare().unwrap();
        check.pull().unwrap();
        let text = std::fs::read_to_string(base.join("check").join("2026.json")).unwrap();
        assert!(text.contains("本机改动"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// sync_round 四象限：本机无改动 + 远端有新内容 → Pulled（数据目录直接变成远端内容）。
    #[test]
    fn sync_round_pulls_remote_only() {
        use crate::model::DayRecord;
        if !git_available() { return; }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };

        // 本机：推一个空数据基线
        let ds = DocStore::new(base.join("w"));
        ds.save(&Doc::default()).unwrap();
        let git = GitStore::new(base.join("w"), cfg.clone());
        git.prepare().unwrap();
        git.push("baseline").unwrap();

        // 「另一台电脑」在其基线之上推入新内容（使远端领先本机提交）
        let other_ds = DocStore::new(base.join("other"));
        let t = Plan::new("远端新增".into(), String::new(), NaiveTime::from_hms_opt(9,0,0).unwrap());
        let mut rdoc = Doc::default();
        rdoc.records.push(DayRecord::from_template(&t, chrono::NaiveDate::from_ymd_opt(2026,11,7).unwrap()));
        other_ds.save(&rdoc).unwrap();
        let other = GitStore::new(base.join("other"), cfg);
        other.prepare().unwrap();
        other.push("other push").unwrap();

        assert_eq!(git.sync_round("x").unwrap(), SyncOutcome::Pulled);
        // 数据目录应直接包含「远端新增」
        let text = std::fs::read_to_string(base.join("w").join("2026.json")).unwrap();
        assert!(text.contains("远端新增"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// sync_round 冲突：两边都有改动 → 本机版本存入 conflict/ 分支留底，
    /// 主数据对齐远端，流程不中断（有道云笔记「冲突副本」语义）。
    #[test]
    fn sync_round_saves_conflict_copy_and_follows_remote() {
        use crate::model::DayRecord;
        if !git_available() { return; }
        let base = temp_dir();
        let remote = base.join("remote.git");
        git_ok(&base, &["init", "--bare", &remote.to_string_lossy()]);
        let cfg = GitConfig { url: remote.to_string_lossy().into_owned(), branch: "main".into(), path: DOC_DIR_NAME.into(), ..Default::default() };

        let ds = DocStore::new(base.join("w"));
        ds.save(&Doc::default()).unwrap();
        let git = GitStore::new(base.join("w"), cfg.clone());
        git.prepare().unwrap();
        git.push("baseline").unwrap();

        // 远端新增（另一台电脑）
        let other_ds = DocStore::new(base.join("other"));
        let t = Plan::new("远端改动".into(), String::new(), NaiveTime::from_hms_opt(9,0,0).unwrap());
        let mut rdoc = Doc::default();
        rdoc.records.push(DayRecord::from_template(&t, chrono::NaiveDate::from_ymd_opt(2026,11,7).unwrap()));
        other_ds.save(&rdoc).unwrap();
        let other = GitStore::new(base.join("other"), cfg.clone());
        other.prepare().unwrap();
        other.push("other push").unwrap();

        // 本地也新增（延迟队列场景）
        let t2 = Plan::new("本机改动".into(), String::new(), NaiveTime::from_hms_opt(10,0,0).unwrap());
        let mut ldoc = ds.load().unwrap();
        ldoc.records.push(DayRecord::from_template(&t2, chrono::NaiveDate::from_ymd_opt(2026,11,7).unwrap()));
        ds.save(&ldoc).unwrap();

        assert_eq!(git.sync_round("x").unwrap(), SyncOutcome::ConflictCopied);

        // ① 主数据已对齐远端：包含「远端改动」，不含「本机改动」
        let main_text = std::fs::read_to_string(base.join("w").join("2026.json")).unwrap();
        assert!(main_text.contains("远端改动"), "主数据应含远端改动");
        assert!(!main_text.contains("本机改动"), "主数据不应再含本机改动");

        // ② 本机改动没有丢：在 conflict/ 分支里
        let branches = git.run(&["branch", "--list", "conflict/*"]).unwrap();
        assert!(branches.contains("conflict/"), "应存在 conflict 分支：{}", branches);
        let saved = git.run(&["show", &format!("{}:2026.json", branches.split_whitespace().find(|b| b.starts_with("conflict/")).unwrap().trim())]).unwrap();
        assert!(saved.contains("本机改动"), "冲突副本应包含本机改动");

        // ③ 当前在主分支上
        let cur = git.run(&["branch", "--show-current"]).unwrap();
        assert_eq!(cur.trim(), "main");

        // ④ 副本管理 API：list → load → delete 全链路
        let list = git.list_conflict_branches().unwrap();
        assert_eq!(list.len(), 1, "应列出 1 份副本：{:?}", list);
        let (name, _, subject) = &list[0];
        assert!(name.starts_with("conflict/"));
        assert!(subject.contains("冲突副本"), "提交说明应可读：{}", subject);
        let cdoc = git.load_conflict_doc(name).unwrap();
        assert!(cdoc.records.iter().any(|r| r.title == "本机改动"), "副本 Doc 应含本机改动");
        git.delete_conflict_branch(name).unwrap();
        assert!(git.list_conflict_branches().unwrap().is_empty(), "删除后列表应为空");

        let _ = std::fs::remove_dir_all(&base);
    }
}
