//! 数据模型：提醒模板 + 按日期生成的独立记录。

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 模板的重复周期。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepeatRule {
    Daily,
    Weekly { weekdays: [bool; 7] },
    Monthly { day: u32 },
    Yearly { month: u32, day: u32 },
}

impl Default for RepeatRule {
    fn default() -> Self {
        Self::Daily
    }
}

impl RepeatRule {
    pub fn label(self) -> String {
        match self {
            Self::Daily => "每天".into(),
            Self::Weekly { weekdays } => {
                let names = ["一", "二", "三", "四", "五", "六", "日"];
                let picked: Vec<&str> = names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| weekdays[i].then_some(*n))
                    .collect();
                if picked.is_empty() {
                    "每周（未选择）".into()
                } else if picked.len() == 7 {
                    "每天".into()
                } else {
                    format!("每周{}", picked.join("、"))
                }
            }
            Self::Monthly { day } => format!("每月 {day} 日"),
            Self::Yearly { month, day } => format!("每年 {month} 月 {day} 日"),
        }
    }

    /// 某天是否属于该规则。月度按月底兜底，年度 2/29 在平年按 2/28。
    pub fn occurs_on(self, date: NaiveDate) -> bool {
        match self {
            Self::Daily => true,
            Self::Weekly { weekdays } => weekdays[date.weekday().num_days_from_monday() as usize],
            Self::Monthly { day } => {
                let last = last_day_of_month(date.year(), date.month());
                date.day() == day.clamp(1, 31).min(last)
            }
            Self::Yearly { month, day } => {
                if date.month() != month.clamp(1, 12) {
                    return false;
                }
                let last = last_day_of_month(date.year(), date.month());
                date.day() == day.clamp(1, 31).min(last)
            }
        }
    }

    pub fn validate(self) -> Result<Self, String> {
        match self {
            Self::Weekly { weekdays } if !weekdays.iter().any(|v| *v) => {
                Err("每周提醒至少选择一天".into())
            }
            Self::Monthly { day } if !(1..=31).contains(&day) => {
                Err("每月日期需在 1 到 31 之间".into())
            }
            Self::Yearly { month, day }
                if !(1..=12).contains(&month) || !(1..=31).contains(&day) =>
            {
                Err("年度提醒的月份和日期无效".into())
            }
            other => Ok(other),
        }
    }
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(31)
}

/// 提醒窗口：从「计划时间 - 提前天数 - 提前分钟数」开始，到「计划时间 + 延后天数」结束。
///
/// 窗口内按 `every_minutes` 反复提醒；`grace_days` 为 `None` 表示一直催到完成为止。
/// 窗口结束后仍未完成，记录标为「逾期」，不再提醒。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemindWindow {
    /// 提前几天开始提醒（0 = 就在计划当天开始）。
    #[serde(default)]
    pub advance_days: u32,
    /// 提前多少分钟开始提醒（0 = 不做分钟级提前）。
    ///
    /// 与 `advance_days` **叠加**：起点 = 计划时刻 - advance_days 天 - advance_minutes 分钟。
    /// 天级提前的起点按「那天 0 点」算，分钟级提前在此基础上再往前推；
    /// 例：计划 11-07 14:00，提前 1 天 + 30 分钟 → 从 11-06 00:00 起（天级生效）；
    /// 计划 11-07 14:00，提前 0 天 + 30 分钟 → 从 11-07 13:30 起（保留时分）。
    #[serde(default)]
    pub advance_minutes: u32,
    /// 延后几天结束提醒；`None` = 不限，一直催到完成。
    #[serde(default)]
    pub grace_days: Option<u32>,
    /// 窗口内的重响间隔（分钟）。
    #[serde(default = "default_every")]
    pub every_minutes: u32,
}

fn default_every() -> u32 {
    30
}

impl Default for RemindWindow {
    fn default() -> Self {
        Self {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 30,
        }
    }
}

impl RemindWindow {
    pub const MAX_ADVANCE: u32 = 365;
    /// 分钟级提前的上限（7 天 = 10080 分钟，足够覆盖任何「提前 X 分钟」场景）。
    pub const MAX_ADVANCE_MINUTES: u32 = 10080;
    pub const MAX_GRACE: u32 = 365;

    pub fn validate(self) -> Result<Self, String> {
        if self.advance_days > Self::MAX_ADVANCE {
            return Err(format!("提前天数最多 {} 天", Self::MAX_ADVANCE));
        }
        if let Some(g) = self.grace_days {
            if g > Self::MAX_GRACE {
                return Err(format!("延后天数最多 {} 天", Self::MAX_GRACE));
            }
        }
        if !(1..=10080).contains(&self.every_minutes) {
            return Err("提醒间隔需在 1 分钟到 7 天之间".into());
        }
        Ok(self)
    }

    /// 卡片角落的小标记；全部取默认值时返回空串（不占地方）。
    pub fn hint_short(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.advance_days > 0 {
            parts.push(format!("提前{}天", self.advance_days));
        }
        match self.grace_days {
            None => parts.push("不限".into()),
            Some(0) => {}
            Some(d) => parts.push(format!("延后{}天", d)),
        }
        parts.join(" · ")
    }

    /// 详细的说明文字。
    pub fn describe(self) -> String {
        let every = fmt_minutes(self.every_minutes);
        let mut s = String::new();
        // 起点永远是 0 点：提前 0 天就是计划当天 0 点。
        if self.advance_days > 0 {
            s.push_str(&format!(
                "提前 {} 天（那天 0 点起）提醒，",
                self.advance_days
            ));
        } else {
            s.push_str("计划当天 0 点起提醒，");
        }
        s.push_str(&format!("每 {} 提醒一次", every));
        match self.grace_days {
            None => s.push_str("，一直提醒到标记完成为止"),
            Some(0) => s.push_str("，计划时间过后不再提醒"),
            Some(d) => s.push_str(&format!("，计划时间过后再提醒 {} 天", d)),
        }
        s
    }
}

/// 把分钟数说人话：30分钟 / 2小时 / 1天。
pub fn fmt_minutes(minutes: u32) -> String {
    if minutes >= 1440 && minutes % 1440 == 0 {
        format!("{}天", minutes / 1440)
    } else if minutes >= 60 && minutes % 60 == 0 {
        format!("{}小时", minutes / 60)
    } else {
        format!("{minutes}分钟")
    }
}

/// 提醒模板：只描述周期，不保存某天的完成状态。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    #[serde(default)]
    pub id: Uuid,
    pub title: String,
    /// 用户自定义标签（每日计划 / 生日提醒 / 纪念日 …）。空串表示未分类。
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub note: String,
    pub time: NaiveTime,
    #[serde(default)]
    pub repeat: RepeatRule,
    /// 提醒策略（到点 / 提前 / 间隔重响），生成记录时复制过去。
    #[serde(default)]
    pub remind: RemindWindow,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "Local::now")]
    pub created_at: DateTime<Local>,

    // ---- v1 兼容字段：仅用于迁移，v2 保存时不再写回 ----
    #[serde(default, rename = "done_date", skip_serializing)]
    pub legacy_done_date: Option<NaiveDate>,
    #[serde(default, rename = "last_fired", skip_serializing)]
    pub legacy_last_fired: Option<DateTime<Local>>,
    #[serde(default, rename = "snooze_until", skip_serializing)]
    pub legacy_snooze_until: Option<DateTime<Local>>,
}

fn yes() -> bool {
    true
}

impl Plan {
    pub fn new(title: String, note: String, time: NaiveTime) -> Self {
        Self {
            id: Uuid::new_v4(),
            title,
            tag: String::new(),
            note,
            time,
            repeat: RepeatRule::Daily,
            remind: RemindWindow::default(),
            enabled: true,
            created_at: Local::now(),
            legacy_done_date: None,
            legacy_last_fired: None,
            legacy_snooze_until: None,
        }
    }
}

/// 某一天真正落地的记录。模板生成后会复制标题、标签、备注、时间和提醒策略，之后可单独编辑。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayRecord {
    #[serde(default)]
    pub id: Uuid,
    #[serde(default)]
    pub template_id: Option<Uuid>,
    pub date: NaiveDate,
    pub title: String,
    /// 与模板一致的用户标签，可单独修改不影响模板。
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub note: String,
    pub time: NaiveTime,
    /// 提醒窗口（提前 / 延后 / 间隔）。
    #[serde(default)]
    pub remind: RemindWindow,
    #[serde(default)]
    pub done: bool,
    /// 标记完成的时间；取消完成时清空。
    #[serde(default)]
    pub done_at: Option<DateTime<Local>>,
    /// 删除模板生成的记录时保留墓碑，避免自动生成逻辑再次补回。
    #[serde(default)]
    pub cancelled: bool,
    /// 用户单独修改过这条记录，后续修改模板时不覆盖它。
    #[serde(default)]
    pub customized: bool,
    #[serde(default)]
    pub last_fired: Option<DateTime<Local>>,
    #[serde(default)]
    pub snooze_until: Option<DateTime<Local>>,
    #[serde(default = "Local::now")]
    pub created_at: DateTime<Local>,
}

impl Plan {
    /// 模板下一次发生时间，最多向前查 8 年。
    pub fn next_occurrence(&self, now: &DateTime<Local>) -> Option<DateTime<Local>> {
        if !self.enabled {
            return None;
        }
        let start = now.date_naive();
        for offset in 0..=366 * 8 {
            let date = start + chrono::Duration::days(offset);
            if !self.repeat.occurs_on(date) {
                continue;
            }
            if let Some(occ) = date.and_time(self.time).and_local_timezone(Local).single() {
                if occ > *now {
                    return Some(occ);
                }
            }
        }
        None
    }
}

impl DayRecord {
    pub fn from_template(template: &Plan, date: NaiveDate) -> Self {
        Self {
            id: Uuid::new_v4(),
            template_id: Some(template.id),
            date,
            title: template.title.clone(),
            tag: template.tag.clone(),
            note: template.note.clone(),
            time: template.time,
            remind: template.remind,
            done: false,
            done_at: None,
            cancelled: false,
            customized: false,
            last_fired: None,
            snooze_until: None,
            created_at: Local::now(),
        }
    }

    #[cfg(test)]
    pub fn standalone(date: NaiveDate, title: String, note: String, time: NaiveTime) -> Self {
        Self {
            id: Uuid::new_v4(),
            template_id: None,
            date,
            title,
            tag: String::new(),
            note,
            time,
            remind: RemindWindow::default(),
            done: false,
            done_at: None,
            cancelled: false,
            customized: false,
            last_fired: None,
            snooze_until: None,
            created_at: Local::now(),
        }
    }

    pub fn occurrence_at(&self) -> Option<DateTime<Local>> {
        self.date
            .and_time(self.time)
            .and_local_timezone(Local)
            .single()
    }

    pub fn mark_done(&mut self) {
        self.done = true;
        self.done_at = Some(Local::now());
        self.snooze_until = None;
        if let Some(occ) = self.occurrence_at() {
            if self.last_fired.is_none_or(|lf| lf < occ) {
                self.last_fired = Some(occ);
            }
        }
    }

    pub fn clear_done(&mut self) {
        self.done = false;
        self.done_at = None;
    }

    /// 稍后提醒（UI 已不再提供入口，保留模型能力以兼容旧数据与后续扩展）。
    #[allow(dead_code)]
    pub fn snooze(&mut self, until: DateTime<Local>) {
        self.snooze_until = Some(until);
    }

    /// 按提醒窗口判断此刻是否该响；返回需要回写的提醒状态。
    ///
    /// 窗口 = [窗口起点, 计划时刻 + 延后天数]，窗口内按 `every_minutes` 反复提醒。
    /// `grace_days` 为 `None` 时右边界无限，一直催到完成为止。
    ///
    /// **重响节奏以「窗口起点 + N × 间隔」的整点槽位为锚**，而不是「上次响的时刻 + 间隔」。
    /// 原因：如果用上次响的时间做锚，每条记录会各走各的相位 —— 只要有一条的
    /// `last_fired` 被重置过（编辑保存、模板同步、程序暂停后醒来），它就会和别的记录
    /// 错开，之后一直交替弹窗，用户看到的现象就是「同频率的两条提醒从不同时出现」。
    /// 用槽位对齐后，只要 `every_minutes` 相同，所有记录必然在同一时刻一起响。
    pub fn check_fire(&self, now: &DateTime<Local>) -> Option<FireUpdate> {
        if self.cancelled || self.done {
            return None;
        }

        // 稍后提醒：到点重响。
        if let Some(s) = self.snooze_until {
            if s <= *now {
                return Some(FireUpdate {
                    last_fired: None,
                    clear_snooze: true,
                });
            }
        }

        let occ = self.occurrence_at()?;
        let Some((start, end)) = self.remind.window(occ) else {
            return None;
        };

        // 还没进窗口，或者窗口已经结束（逾期）。
        if *now < start {
            return None;
        }
        if let Some(end) = end {
            if *now > end {
                return None;
            }
        }

        let step_min = self.remind.every_minutes.clamp(1, 10080) as i64;

        // 首次进窗口；或上次提醒早于窗口起点（改过日期/时间，窗口整体挪了），
        // 都当作首响。注意这里**不**因为「刚编辑过」就无条件重置，
        // 手动改标题之类不应打断已对齐的节奏。
        let Some(lf) = self.last_fired.filter(|lf| *lf >= start) else {
            return Some(FireUpdate {
                last_fired: Some(*now),
                clear_snooze: true,
            });
        };

        // 当前落在第几个槽位，以及该槽位的起始时刻。
        // 例：起点 00:00、间隔 60 分钟、现在 03:30 → 槽位 3，槽位起点 03:00。
        let slots = (*now - start).num_minutes() / step_min;
        let slot_start = start + chrono::Duration::minutes(slots * step_min);

        // 本槽位已经响过，就等下一个槽位。
        if lf >= slot_start {
            return None;
        }
        Some(FireUpdate {
            last_fired: Some(*now),
            clear_snooze: false,
        })
    }

    /// 此刻是否仍处于「提醒窗口」内（供提醒弹窗清理使用）。
    pub fn in_reminder_window(&self, now: &DateTime<Local>) -> bool {
        if self.cancelled || self.done {
            return false;
        }
        let Some(occ) = self.occurrence_at() else {
            return false;
        };
        let Some((start, end)) = self.remind.window(occ) else {
            return false;
        };
        if *now < start {
            return false;
        }
        end.is_none_or(|e| *now <= e)
    }

    /// 是否已经过了提醒窗口但还没完成（供「逾期」标记使用）。
    pub fn is_overdue(&self, now: &DateTime<Local>) -> bool {
        if self.cancelled || self.done {
            return false;
        }
        let Some(occ) = self.occurrence_at() else {
            return false;
        };
        let Some((_, end)) = self.remind.window(occ) else {
            return false;
        };
        end.is_some_and(|e| *now > e)
    }
}

impl RemindWindow {
    /// 计算这条记录的提醒窗口：返回 `(起点, 终点)`，终点为 `None` 表示不限。
    ///
    /// **起点**：天级部分取「计划日往前 `advance_days` 天」那一天的 **00:00**
    /// （**不**保留计划时分），分钟级部分再往前推 `advance_minutes` 分钟。
    /// - `advance_days == 0, advance_minutes == 0` → 计划当天 00:00 起就开始判断。
    ///   例：计划 11-06 12:00，提前 0 天 0 分 → 从 11-06 00:00 起响，不必等到 12:00。
    /// - `advance_days == 7` → 从 10-30 00:00 起。
    /// - `advance_days == 0, advance_minutes == 30`，计划 11-07 14:00 → 从 13:30 起。
    ///   （分钟级提前保留计划时分——「会议前 15 分钟」这类场景。）
    /// - 两者叠加：起点 = 天级起点再减 advance_minutes。
    ///
    /// **终点** = 计划时刻 + 延后天数；`grace_days == None` 时返回 `None`。
    pub fn window(self, occ: DateTime<Local>) -> Option<(DateTime<Local>, Option<DateTime<Local>>)> {
        let days = self.advance_days.min(Self::MAX_ADVANCE) as i64;
        // 起点统一按「那一天的 0 点」算，提前 0 天也走这条路径。
        let day = occ.date_naive() - chrono::Duration::days(days);
        let start = day
            .and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local)
            .single()
            // 夏令时等极端情况下本地时间可能无唯一解，退回整天平移。
            .unwrap_or_else(|| occ - chrono::Duration::days(days));
        // 分钟级提前：
        //  - minutes == 0 → 天级行为完全不变（提前 0 天仍从当天 0 点起）
        //  - minutes > 0, days == 0 → 「计划时刻 - N 分钟」起（保留时分，会前提醒场景）
        //  - minutes > 0, days > 0 → 天级 0 点之上再往前推（叠加）
        let minutes = self.advance_minutes.min(Self::MAX_ADVANCE_MINUTES) as i64;
        let start = if minutes == 0 {
            start
        } else if self.advance_days == 0 {
            occ - chrono::Duration::minutes(minutes)
        } else {
            start - chrono::Duration::minutes(minutes)
        };
        let end = self
            .grace_days
            .map(|g| occ + chrono::Duration::days(g.min(Self::MAX_GRACE) as i64));
        Some((start, end))
    }
}

#[derive(Clone, Debug, Default)]
pub struct FireUpdate {
    pub last_fired: Option<DateTime<Local>>,
    pub clear_snooze: bool,
}

impl FireUpdate {
    pub fn apply_to(&self, record: &mut DayRecord) {
        if let Some(v) = self.last_fired {
            record.last_fired = Some(v);
        }
        if self.clear_snooze {
            record.snooze_until = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc {
    #[serde(default = "two")]
    pub version: u32,
    /// 仅用于读取 v1 的 plans，v2 新格式使用 templates。
    #[serde(default, rename = "plans", skip_serializing)]
    legacy_templates: Vec<Plan>,
    #[serde(default)]
    pub templates: Vec<Plan>,
    #[serde(default)]
    pub records: Vec<DayRecord>,
    /// 用户建立的标签字典。即便某个标签暂时没有记录在用也会保留，
    /// 删除记录不会把标签从下拉里弄丢。
    #[serde(default)]
    pub tags: Vec<String>,
}

fn two() -> u32 {
    2
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            version: 2,
            legacy_templates: Vec::new(),
            templates: Vec::new(),
            records: Vec::new(),
            tags: Vec::new(),
        }
    }
}

impl Doc {
    /// 规范化并迁移 v1：把旧计划里的完成/提醒状态落到对应日期记录。
    pub fn normalized(mut self) -> Self {
        let was_legacy = self.version < 2;
        if was_legacy {
            self.templates.extend(self.legacy_templates.drain(..));
            for template in &self.templates {
                let date = template
                    .legacy_done_date
                    .or_else(|| template.legacy_last_fired.map(|v| v.date_naive()))
                    .or_else(|| template.legacy_snooze_until.map(|v| v.date_naive()));
                let Some(date) = date else {
                    continue;
                };
                if self
                    .records
                    .iter()
                    .any(|r| r.template_id == Some(template.id) && r.date == date)
                {
                    continue;
                }
                let mut record = DayRecord::from_template(template, date);
                record.done = template.legacy_done_date == Some(date);
                record.last_fired = template
                    .legacy_last_fired
                    .filter(|v| v.date_naive() == date);
                record.snooze_until = template
                    .legacy_snooze_until
                    .filter(|v| v.date_naive() == date);
                self.records.push(record);
            }
        }
        self.version = 2;

        for template in &mut self.templates {
            if template.id.is_nil() {
                template.id = Uuid::new_v4();
            }
        }
        for record in &mut self.records {
            if record.id.is_nil() {
                record.id = Uuid::new_v4();
            }
        }
        self
    }

    pub fn template(&self, id: Uuid) -> Option<&Plan> {
        self.templates.iter().find(|t| t.id == id)
    }

    pub fn record(&self, id: Uuid) -> Option<&DayRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    pub fn record_for(&self, template_id: Uuid, date: NaiveDate) -> Option<&DayRecord> {
        self.records
            .iter()
            .find(|r| r.template_id == Some(template_id) && r.date == date)
    }

    pub fn record_indices_for(&self, date: NaiveDate) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.date == date && !r.cancelled)
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|&i| (self.records[i].time, self.records[i].title.as_str()));
        indices
    }

    /// 汇总标签字典 + 模板/记录里出现过的标签（去重、去空串），按字典序返回。
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .tags
            .iter()
            .map(|t| t.trim())
            .chain(self.templates.iter().map(|t| t.tag.trim()))
            .chain(
                self.records
                    .iter()
                    .filter(|r| !r.cancelled)
                    .map(|r| r.tag.trim()),
            )
            .filter(|t| !t.is_empty())
            .map(|t| t.to_owned())
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    /// 把标签记入字典（重复或空白自动忽略）。返回是否新增。
    pub fn remember_tag(&mut self, tag: &str) -> bool {
        let tag = tag.trim();
        if tag.is_empty() || self.tags.iter().any(|t| t == tag) {
            return false;
        }
        self.tags.push(tag.to_owned());
        true
    }

    /// 从字典移除标签（不影响已有记录上的标签文字）。
    pub fn forget_tag(&mut self, tag: &str) {
        self.tags.retain(|t| t != tag);
    }
}

pub fn fmt_time(t: NaiveTime) -> String {
    t.format("%H:%M").to_string()
}

// ============================== 按年拆分 ==============================
//
// 数据量随年份线性增长（每天 3 条 ≈ 每年 600 KB），单个 plans.json 会越来越臃肿：
// 每次保存都要整份序列化重写，Git 里也会变成一份不断膨胀的不可 diff 的巨型文件。
//
// 于是把**记录**按 `date.year()` 拆成多个文件，模板与标签留在索引文件里：
//
//   plans/plans.json      索引：version / templates / tags / years
//   plans/2026.json       该年的所有 records
//   plans/2027.json
//
// 内存里仍然是一个完整的 `Doc`（界面代码完全不用改），只在读写磁盘时做拆分。

/// 索引文件：不放 records，只放随年份不变的元数据 + 已有年份清单。
#[derive(Debug, Serialize, Deserialize)]
struct YearIndex {
    #[serde(default = "two")]
    version: u32,
    #[serde(default, rename = "plans", skip_serializing)]
    legacy_templates: Vec<Plan>,
    #[serde(default)]
    templates: Vec<Plan>,
    #[serde(default)]
    tags: Vec<String>,
    /// 本目录下有哪些年份文件（排序后便于人工阅读与 diff）。
    #[serde(default)]
    years: Vec<i32>,
}

impl Default for YearIndex {
    fn default() -> Self {
        Self {
            version: 2,
            legacy_templates: Vec::new(),
            templates: Vec::new(),
            tags: Vec::new(),
            years: Vec::new(),
        }
    }
}

/// 单个年份文件：只装这一年（含当年跨年记录）的 records。
#[derive(Debug, Serialize, Deserialize)]
struct YearRecords {
    #[serde(default = "two")]
    version: u32,
    #[serde(default)]
    year: i32,
    #[serde(default)]
    records: Vec<DayRecord>,
}

impl Doc {
    /// 索引文件的内容（不含 records）。
    pub fn to_year_index_json(&self) -> Result<String, serde_json::Error> {
        let mut years: Vec<i32> = self.records.iter().map(|r| r.date.year()).collect();
        years.sort_unstable();
        years.dedup();
        let index = YearIndex {
            version: self.version,
            legacy_templates: Vec::new(),
            templates: self.templates.clone(),
            tags: self.tags.clone(),
            years,
        };
        serde_json::to_string_pretty(&index)
    }

    /// 按年份分组后的 `(年份, 该年文件内容)` 列表。
    pub fn to_year_files(&self) -> Result<Vec<(i32, String)>, serde_json::Error> {
        use std::collections::BTreeMap;
        // BTreeMap 保证年份升序输出，文件顺序稳定（便于 diff）
        let mut by_year: BTreeMap<i32, Vec<DayRecord>> = BTreeMap::new();
        for record in &self.records {
            by_year.entry(record.date.year()).or_default().push(record.clone());
        }
        let mut out = Vec::with_capacity(by_year.len());
        for (year, records) in by_year {
            let file = YearRecords {
                version: self.version,
                year,
                records,
            };
            out.push((year, serde_json::to_string_pretty(&file)?));
        }
        Ok(out)
    }

    /// 由索引内容 + 各年记录内容重建完整 `Doc`。
    ///
    /// 索引里没有 records，年份文件里没有模板，两者合并才是完整文档。
    pub fn from_year_parts(index: &str, year_files: &[String]) -> Result<Self, serde_json::Error> {
        let idx: YearIndex = serde_json::from_str(index)?;
        let mut records = Vec::new();
        for text in year_files {
            let file: YearRecords = serde_json::from_str(text)?;
            records.extend(file.records);
        }
        // 记录按日期稳定排序，让「同一天的多条」在界面上顺序固定
        records.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then_with(|| a.time.cmp(&b.time))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(Self {
            version: idx.version,
            legacy_templates: idx.legacy_templates,
            templates: idx.templates,
            records,
            tags: idx.tags,
        })
    }

    /// 单个旧格式 `plans.json` 的内容 → Doc（迁移用；格式与 v2 一致）。
    pub fn from_legacy_plans_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str::<Self>(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("测试时间必须合法")
    }

    fn time(h: u32, mi: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, mi, 0).unwrap()
    }

    fn date(y: i32, mo: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap()
    }

    fn record_at(h: u32, mi: u32, day: NaiveDate) -> DayRecord {
        DayRecord::standalone(day, "t".into(), String::new(), time(h, mi))
    }

    #[test]
    fn window_fires_repeatedly_inside_range() {
        // 计划 11/05 09:00，提前 2 天 → 窗口从 11/03 **00:00** 开始（不是 09:00），每 30 分钟
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 2,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 30,
        };

        // 窗口外不响（11/02 深夜里）
        assert!(r.check_fire(&local(2026, 11, 2, 23, 59)).is_none());
        // 进窗口首响：11/03 00:00 —— 验证起点是 0 点而非计划时分
        let upd = r.check_fire(&local(2026, 11, 3, 0, 0)).expect("进窗口应首响");
        upd.apply_to(&mut r);
        assert_eq!(r.last_fired, Some(local(2026, 11, 3, 0, 0)));
        // 未满间隔不响
        assert!(r.check_fire(&local(2026, 11, 3, 0, 29)).is_none());
        // 满 30 分钟重响
        let upd = r.check_fire(&local(2026, 11, 3, 0, 30)).expect("应重响");
        upd.apply_to(&mut r);

        // 计划时刻之后仍继续（grace=None 不限）
        assert!(r.check_fire(&local(2026, 11, 9, 12, 0)).is_some());

        // 完成后停止
        r.mark_done();
        assert!(r.check_fire(&local(2026, 11, 9, 13, 0)).is_none());
    }

    /// 同频率的两条记录必须**同时**响，不能各走各的相位。
    ///
    /// 回归场景：原来用「上次响 + 间隔」做锚点，只要其中一条的 `last_fired`
    /// 被重置过（编辑保存就会重置），它就会立刻单独响一次并和另一条错开，
    /// 之后一直交替弹，用户看到「同频率的提醒从不同时出现」。
    #[test]
    fn same_interval_records_fire_together() {
        // 用户真实场景：两条都在 11/06，间隔 60 分钟。
        // A：12:00，提前 7 天 → 窗口起点 10/30 00:00
        // B：23:00，提前 3 天 → 窗口起点 11/03 00:00
        let mut a = record_at(12, 0, date(2026, 11, 6));
        a.remind = RemindWindow {
            advance_days: 7,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 60,
        };
        let mut b = record_at(23, 0, date(2026, 11, 6));
        b.remind = RemindWindow {
            advance_days: 3,
            advance_minutes: 0,
            grace_days: Some(6),
            every_minutes: 60,
        };

        // 模拟 A 的节奏被人为打断：只给 A 记一个很早的 last_fired，
        // B 则已按正常节奏响过。两条都应落在同一个整点槽位上。
        a.last_fired = Some(local(2026, 11, 3, 0, 0));
        b.last_fired = Some(local(2026, 11, 3, 10, 0));

        // 11/04 15:00 这个整点槽位，两条都必须响。
        let t = local(2026, 11, 4, 15, 0);
        let fa = a.check_fire(&t);
        let fb = b.check_fire(&t);
        assert!(fa.is_some(), "A 应在 15:00 这个槽位响");
        assert!(fb.is_some(), "B 应在 15:00 这个槽位响");

        // 同一个槽位内再问一次，都不该重复响（已记录 last_fired）。
        fa.unwrap().apply_to(&mut a);
        fb.unwrap().apply_to(&mut b);
        assert!(a.check_fire(&local(2026, 11, 4, 15, 30)).is_none(), "A 同槽位内不应再响");
        assert!(b.check_fire(&local(2026, 11, 4, 15, 30)).is_none(), "B 同槽位内不应再响");

        // 下一个整点槽位，两条又一起响。
        let t2 = local(2026, 11, 4, 16, 0);
        assert!(a.check_fire(&t2).is_some(), "A 应在 16:00 响");
        assert!(b.check_fire(&t2).is_some(), "B 应在 16:00 响");
    }

    /// 编辑保存（不清 last_fired）不应让记录提前单独响。
    #[test]
    fn editing_does_not_shift_fire_phase() {
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 60,
        };
        // 已经在本槽位响过。
        r.last_fired = Some(local(2026, 11, 5, 3, 0));
        // 同一小时里只改标题，不应重响。
        assert!(r.check_fire(&local(2026, 11, 5, 3, 40)).is_none());
        // 下一个槽位正常响。
        assert!(r.check_fire(&local(2026, 11, 5, 4, 0)).is_some());
    }

    /// 改了日期导致窗口整体后移，旧 last_fired 落在窗口外 → 当作首响。
    #[test]
    fn moving_window_into_future_refires() {
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 60,
        };
        // 旧窗口里响过。
        r.last_fired = Some(local(2026, 11, 5, 1, 0));
        // 把计划日期改到 11/08：新窗口起点 11/08 00:00，旧 last_fired 在窗口外。
        r.date = date(2026, 11, 8);
        assert!(
            r.check_fire(&local(2026, 11, 8, 0, 5)).is_some(),
            "窗口整体后移后应在新区间首响"
        );
    }

    /// 提前天数 > 0 时，起点必须是那一天的 0 点，不带计划时分。
    #[test]
    fn advance_starts_at_midnight_not_plan_time() {
        let occ = local(2026, 11, 6, 12, 0); // 计划 11/06 12:00
        let w = RemindWindow {
            advance_days: 7,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 60,
        };
        let (start, _) = w.window(occ).unwrap();
        assert_eq!(
            start,
            local(2026, 10, 30, 0, 0),
            "提前 7 天应从 10/30 00:00 开始，而不是 10/30 12:00"
        );

        // 提前 1 天同理。
        let w = RemindWindow {
            advance_days: 1,
            ..w
        };
        let (start, _) = w.window(occ).unwrap();
        assert_eq!(start, local(2026, 11, 5, 0, 0));
    }

    /// 提前 0 天也从**当天 0 点**开始，不必等到计划时分。
    #[test]
    fn zero_advance_also_starts_at_midnight() {
        let occ = local(2026, 11, 6, 12, 0);
        let w = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: None,
            every_minutes: 60,
        };
        let (start, _) = w.window(occ).unwrap();
        assert_eq!(
            start,
            local(2026, 11, 6, 0, 0),
            "提前 0 天应从计划当天 0 点开始，而不是计划时刻 12:00"
        );
    }

    /// 提前 0 天 + 忽略延后：0 点进窗口就响，计划时刻过后立刻停。
    #[test]
    fn zero_advance_fires_from_midnight_then_stops_at_plan_time() {
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: Some(0),
            every_minutes: 60,
        };
        // 当天 0 点就进窗口
        assert!(r.in_reminder_window(&local(2026, 11, 5, 0, 0)));
        // 前一天深夜还没进
        assert!(!r.in_reminder_window(&local(2026, 11, 4, 23, 59)));
        // 0 点首响
        let upd = r.check_fire(&local(2026, 11, 5, 0, 0)).expect("0 点应首响");
        upd.apply_to(&mut r);
        // 计划时刻前仍在窗口内
        assert!(r.check_fire(&local(2026, 11, 5, 8, 0)).is_some());
        // 计划时刻一过即停
        assert!(r.check_fire(&local(2026, 11, 5, 9, 1)).is_none());
    }

    #[test]
    fn grace_days_stops_after_deadline() {
        // 计划 11/05 09:00，延后 1 天 → 窗口到 11/06 09:00
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: Some(1),
            every_minutes: 60,
        };

        let upd = r.check_fire(&local(2026, 11, 5, 9, 0)).expect("首响");
        upd.apply_to(&mut r);
        // 窗口内仍会响
        assert!(r.check_fire(&local(2026, 11, 6, 8, 0)).is_some());
        // 超过延后期，停止提醒
        assert!(r.check_fire(&local(2026, 11, 6, 9, 1)).is_none());
        // 并标记为逾期
        assert!(r.is_overdue(&local(2026, 11, 6, 9, 1)));
        assert!(!r.is_overdue(&local(2026, 11, 5, 12, 0)));
    }

    #[test]
    fn zero_grace_means_stop_right_at_plan_time() {
        let mut r = record_at(9, 0, date(2026, 11, 5));
        r.remind = RemindWindow {
            advance_days: 0,
            advance_minutes: 0,
            grace_days: Some(0),
            every_minutes: 30,
        };
        let upd = r.check_fire(&local(2026, 11, 5, 9, 0)).expect("首响");
        upd.apply_to(&mut r);
        // 计划时刻过后立即停止
        assert!(r.check_fire(&local(2026, 11, 5, 9, 31)).is_none());
        assert!(r.is_overdue(&local(2026, 11, 5, 9, 31)));
    }

    #[test]
    fn window_helpers_and_labels() {
        assert!(RemindWindow {
            advance_days: 366,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(RemindWindow {
            advance_days: 365,
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(RemindWindow {
            every_minutes: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(RemindWindow {
            every_minutes: 10080,
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(RemindWindow {
            grace_days: Some(366),
            ..Default::default()
        }
        .validate()
        .is_err());

        // 全默认：无标记
        assert_eq!(RemindWindow::default().hint_short(), "不限");
        assert_eq!(
            RemindWindow {
                advance_days: 3,
                advance_minutes: 0,
                grace_days: Some(2),
                every_minutes: 30,
            }
            .hint_short(),
            "提前3天 · 延后2天"
        );
        assert_eq!(
            RemindWindow {
                advance_days: 0,
                advance_minutes: 0,
                grace_days: Some(0),
                every_minutes: 30,
            }
            .hint_short(),
            ""
        );
        assert_eq!(fmt_minutes(1440), "1天");
    }

    #[test]
    fn done_at_is_set_and_cleared() {
        let mut r = record_at(9, 0, date(2026, 11, 2));
        assert!(r.done_at.is_none());
        r.mark_done();
        assert!(r.done_at.is_some(), "完成时应记录时间");
        r.clear_done();
        assert!(r.done_at.is_none(), "取消完成应清空时间");
    }

    #[test]
    fn weekly_rule_and_next_occurrence() {
        let mut p = Plan::new("复盘".into(), String::new(), time(9, 0));
        p.repeat = RepeatRule::Weekly {
            weekdays: [true, false, true, false, true, false, false],
        };
        assert!(p.repeat.occurs_on(date(2026, 11, 2)));
        assert!(!p.repeat.occurs_on(date(2026, 11, 3)));
        assert_eq!(
            p.next_occurrence(&local(2026, 11, 3, 8, 0)),
            Some(local(2026, 11, 4, 9, 0))
        );
    }

    #[test]
    fn monthly_and_yearly_edge_days() {
        let mut p = Plan::new("月报".into(), String::new(), time(9, 0));
        p.repeat = RepeatRule::Monthly { day: 31 };
        assert!(p.repeat.occurs_on(date(2026, 2, 28)));
        assert!(p.repeat.occurs_on(date(2026, 4, 30)));
        p.repeat = RepeatRule::Yearly { month: 2, day: 29 };
        assert!(p.repeat.occurs_on(date(2027, 2, 28)));
        assert!(!p.repeat.occurs_on(date(2027, 3, 1)));
    }

    #[test]
    fn invalid_rules_are_rejected() {
        assert!(RepeatRule::Weekly {
            weekdays: [false; 7]
        }
        .validate()
        .is_err());
        assert!(RepeatRule::Monthly { day: 0 }.validate().is_err());
    }

    #[test]
    fn legacy_plan_migrates_to_template_and_dated_record() {
        let json = r#"{"version":1,"plans":[{"id":"c0b1bb14-6024-43b3-bca9-9ba1b52dccf3","title":"跑步","time":"07:30","done_date":"2026-11-02","last_fired":"2026-11-02T07:30:00+08:00"}]}"#;
        let doc = serde_json::from_str::<Doc>(json).unwrap().normalized();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.templates.len(), 1);
        assert_eq!(doc.records.len(), 1);
        assert_eq!(doc.records[0].date, date(2026, 11, 2));
        assert!(doc.records[0].done);
        assert_eq!(doc.records[0].template_id, Some(doc.templates[0].id));
    }

    #[test]
    fn records_are_independent_and_fire_only_on_their_date() {
        // 默认窗口：不提前、不限延后、每 30 分钟
        let mut r = record_at(8, 30, date(2026, 11, 2));
        assert!(r.check_fire(&local(2026, 11, 1, 8, 31)).is_none());
        let upd = r.check_fire(&local(2026, 11, 2, 8, 31)).expect("应提醒");
        upd.apply_to(&mut r);
        // 首响记录的是「实际响的时刻」
        assert_eq!(r.last_fired, Some(local(2026, 11, 2, 8, 31)));
        // 未满间隔不响
        assert!(r.check_fire(&local(2026, 11, 2, 8, 59)).is_none());
        // 满 30 分钟重响
        assert!(r.check_fire(&local(2026, 11, 2, 9, 1)).is_some());
        r.mark_done();
        assert!(r.done);
        assert!(r.snooze_until.is_none());
        assert!(r.check_fire(&local(2026, 11, 2, 12, 0)).is_none());
    }

    #[test]
    fn snooze_refires_after_until() {
        let mut r = record_at(8, 30, date(2026, 11, 2));
        r.last_fired = Some(local(2026, 11, 2, 8, 30));
        r.snooze_until = Some(local(2026, 11, 2, 8, 40));
        assert!(r.check_fire(&local(2026, 11, 2, 8, 39)).is_none());
        assert!(
            r.check_fire(&local(2026, 11, 2, 8, 40))
                .expect("稍后到期")
                .clear_snooze
        );
    }

    #[test]
    fn cancelled_record_never_fires() {
        let mut r = record_at(8, 30, date(2026, 11, 2));
        r.cancelled = true;
        assert!(r.check_fire(&local(2026, 11, 2, 8, 31)).is_none());
    }

    #[test]
    fn v2_roundtrip_keeps_templates_and_records() {
        let mut doc = Doc::default();
        let template = Plan::new("喝水".into(), String::new(), time(9, 0));
        doc.records
            .push(DayRecord::from_template(&template, date(2026, 11, 2)));
        doc.templates.push(template);
        let text = serde_json::to_string(&doc).unwrap();
        assert!(text.contains("templates"));
        assert!(text.contains("records"));
        let back = serde_json::from_str::<Doc>(&text).unwrap().normalized();
        assert_eq!(back.templates.len(), 1);
        assert_eq!(back.records.len(), 1);
    }

    #[test]
    fn time_format_helper() {
        assert_eq!(fmt_time(time(9, 5)), "09:05");
    }

    /// 分钟级提前：起点保留计划时分（会前 N 分钟场景）。
    #[test]
    fn advance_minutes_shifts_start_keeping_time() {
        let rw = RemindWindow {
            advance_days: 0,
            advance_minutes: 30,
            grace_days: Some(0),
            every_minutes: 15,
        };
        let occ = local(2026, 11, 7, 14, 0);
        let (start, end) = rw.window(occ).unwrap();
        assert_eq!(start, local(2026, 11, 7, 13, 30), "起点=计划时刻前 30 分钟");
        assert_eq!(end, Some(local(2026, 11, 7, 14, 0)), "延后 0 天=到点即停");
    }

    /// 天与分钟叠加：天级起点（0 点）再往前推分钟。
    #[test]
    fn advance_days_and_minutes_stack() {
        let rw = RemindWindow {
            advance_days: 1,
            advance_minutes: 45,
            grace_days: None,
            every_minutes: 60,
        };
        let occ = local(2026, 11, 7, 14, 0);
        let (start, end) = rw.window(occ).unwrap();
        // 天级：11-06 00:00；再提前 45 分钟 → 11-05 23:15
        assert_eq!(start, local(2026, 11, 5, 23, 15));
        assert_eq!(end, None);
    }

    /// 旧数据（无 advance_minutes 字段）反序列化后默认 0，行为与从前一致。
    #[test]
    fn advance_minutes_defaults_to_zero() {
        let json = r#"{"advance_days": 2, "grace_days": null, "every_minutes": 30}"#;
        let rw: RemindWindow = serde_json::from_str(json).unwrap();
        assert_eq!(rw.advance_minutes, 0);
        let occ = local(2026, 11, 7, 14, 0);
        let (start, _) = rw.window(occ).unwrap();
        assert_eq!(start, local(2026, 11, 5, 0, 0), "纯天级行为不变");
    }
}



