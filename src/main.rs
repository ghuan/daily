//! 小铃（Bell）：按日期管理计划与提醒的 Windows 小工具。
//! 核心模型是「提醒模板 → 具体日期记录」：支持每天、每周、每月、每年重复。
//! 存储为 JSON：本地文件，或 Git 仓库（启动拉取、改动后推送）。
//! 纯用户权限：数据在 exe 同级目录，自启写 HKCU Run 键。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod model;
mod notify;
mod store;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveTime, Timelike};
use eframe::egui::{self, Align, Color32, Layout, RichText, ViewportCommand};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;

use model::{DayRecord, Doc, Plan, RemindWindow, RepeatRule};
use store::{DocStore, GitConfig, GitStore, Kind, Settings};

const APP_TITLE: &str = "小铃 · 计划与提醒";
const APP_NAME: &str = "小铃";
const APP_SUBTITLE: &str = "计划与提醒";
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

const MENU_OPEN: &str = "open";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";

// ============================== 单实例 ==============================

#[cfg(windows)]
mod single_instance {
    //! 单实例守卫：第二个进程不启动新窗口，而是通知已有实例把主界面调出来。

    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };

    const MUTEX_NAME: &str = "Local\\bell-single-instance";
    const EVENT_NAME: &str = "Local\\bell-show-window";

    static MUTEX: OnceLock<usize> = OnceLock::new();
    static SHOW_EVENT: OnceLock<usize> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 首个实例返回 true 并持有互斥量；已有实例时唤醒它并返回 false。
    pub fn claim() -> bool {
        unsafe {
            let name = wide(MUTEX_NAME);
            let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if mutex.is_null() {
                return true; // 拿不到互斥量就不阻止启动
            }
            let already = GetLastError() == ERROR_ALREADY_EXISTS;
            if already {
                CloseHandle(mutex);
                signal_existing();
                return false;
            }
            let _ = MUTEX.set(mutex as usize);

            let ev_name = wide(EVENT_NAME);
            // 自动复位事件：每次启动信号只唤醒一次
            let ev = CreateEventW(std::ptr::null(), 0, 0, ev_name.as_ptr());
            if !ev.is_null() {
                let _ = SHOW_EVENT.set(ev as usize);
            }
            true
        }
    }

    /// 唤醒已有实例。
    fn signal_existing() {
        unsafe {
            let ev_name = wide(EVENT_NAME);
            let ev = OpenEventW(EVENT_MODIFY_STATE, 0, ev_name.as_ptr());
            if !ev.is_null() {
                SetEvent(ev);
                CloseHandle(ev);
            }
        }
    }

    /// 阻塞等待「显示主界面」信号（在后台线程里用）。
    pub fn wait_for_show() {
        let Some(&raw) = SHOW_EVENT.get() else {
            return;
        };
        if raw == 0 {
            return;
        }
        let ev = raw as HANDLE;
        unsafe {
            loop {
                let r = WaitForSingleObject(ev, INFINITE);
                if r == WAIT_OBJECT_0 {
                    crate::request_show_from_other_instance();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod single_instance {
    pub fn claim() -> bool {
        true
    }
    pub fn wait_for_show() {}
}

/// 另一个进程请求显示主界面：置标志，由 UI 线程下一帧处理。
static SHOW_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn request_show_from_other_instance() {
    SHOW_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
    request_ui_wake();
}

// ============================== 托盘事件唤醒 ==============================

/// 托盘事件在独立 UI 线程上产生。保存 egui 上下文，让回调可以主动请求下一帧，
/// 即使主窗口已隐藏也不依赖系统重绘或轮询，避免事件积压。
static UI_CONTEXT: OnceLock<Mutex<Option<egui::Context>>> = OnceLock::new();
static TRAY_EVENTS: OnceLock<Mutex<Vec<TrayAction>>> = OnceLock::new();

enum TrayAction {
    Open,
    ToggleAutostart,
    Quit,
}

fn install_tray_event_handlers(ctx: &egui::Context) {
    let cell = UI_CONTEXT.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = cell.lock() {
        *slot = Some(ctx.clone());
    }

    let tray_events = TRAY_EVENTS.get_or_init(|| Mutex::new(Vec::new()));
    let menu_events = tray_events;
    let tray_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            if let Ok(mut queue) = menu_events.lock() {
                queue.push(TrayAction::Open);
            }
            tray_ctx.request_repaint();
        }
    }));

    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.0.as_str() {
            MENU_OPEN => Some(TrayAction::Open),
            MENU_AUTOSTART => Some(TrayAction::ToggleAutostart),
            MENU_QUIT => Some(TrayAction::Quit),
            _ => None,
        };
        if let Some(action) = action {
            if let Ok(mut queue) = menu_events.lock() {
                queue.push(action);
            }
            menu_ctx.request_repaint();
        }
    }));
}

fn request_ui_wake() {
    if let Some(Ok(slot)) = UI_CONTEXT.get().map(|cell| cell.lock()) {
        if let Some(ctx) = slot.as_ref() {
            ctx.request_repaint();
        }
    }
}

// ============================== 配色 ==============================

/// 现代工作台配色（统一主界面、导航、弹窗与提醒卡片）。
mod theme {
    use eframe::egui::Color32;

    pub const SIDEBAR: Color32 = Color32::from_rgb(23, 29, 48);
    pub const SIDEBAR_HOVER: Color32 = Color32::from_rgb(34, 42, 67);
    pub const BG: Color32 = Color32::from_rgb(244, 245, 249);
    pub const CARD: Color32 = Color32::from_rgb(255, 255, 255);
    pub const CARD_HOVER: Color32 = Color32::from_rgb(248, 249, 253);
    pub const BORDER: Color32 = Color32::from_rgb(226, 229, 238);
    pub const TEXT: Color32 = Color32::from_rgb(35, 40, 57);
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(112, 120, 141);
    pub const ACCENT: Color32 = Color32::from_rgb(89, 101, 232);
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(79, 91, 217);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(237, 239, 255);
    pub const OK: Color32 = Color32::from_rgb(34, 164, 111);
    pub const OK_DIM: Color32 = Color32::from_rgb(226, 247, 237);
    pub const WARN: Color32 = Color32::from_rgb(224, 151, 44);
    pub const DANGER: Color32 = Color32::from_rgb(224, 83, 94);
    pub const TOAST_BG: Color32 = Color32::from_rgb(31, 37, 56);
    /// 深色提醒窗里的次要按钮底色 / 描边（比窗底亮一档，带描边，看得见也点得准）。
    pub const TOAST_BTN: Color32 = Color32::from_rgb(56, 66, 98);
    pub const TOAST_BTN_STROKE: Color32 = Color32::from_rgb(110, 122, 158);
}

/// 全局样式：深色、圆角、宽松间距。
fn install_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    ctx.all_styles_mut(|style| {
        let mut v = egui::Visuals::light();
        v.panel_fill = theme::BG;
        v.window_fill = theme::CARD;
        v.window_stroke = egui::Stroke::new(1.0_f32, theme::BORDER);
        v.window_corner_radius = egui::CornerRadius::same(16);
        v.menu_corner_radius = egui::CornerRadius::same(10);
        v.window_shadow = egui::epaint::Shadow {
            offset: [0, 12],
            blur: 32,
            spread: 0,
            color: egui::Color32::from_black_alpha(38),
        };
        v.popup_shadow = egui::epaint::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: egui::Color32::from_black_alpha(28),
        };
        v.override_text_color = Some(theme::TEXT);
        v.hyperlink_color = theme::ACCENT;
        v.faint_bg_color = theme::CARD_HOVER;
        v.extreme_bg_color = Color32::from_rgb(248, 249, 252);
        v.code_bg_color = theme::CARD_HOVER;
        v.selection.bg_fill = theme::ACCENT_SOFT;
        v.selection.stroke = egui::Stroke::new(1.0_f32, theme::ACCENT);
        v.warn_fg_color = theme::WARN;
        v.error_fg_color = theme::DANGER;

        v.widgets.noninteractive.bg_fill = theme::CARD;
        v.widgets.noninteractive.weak_bg_fill = theme::CARD;
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, theme::BORDER);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, theme::TEXT);
        v.widgets.noninteractive.corner_radius = egui::CornerRadius::same(9);

        v.widgets.inactive.bg_fill = Color32::from_rgb(247, 248, 252);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(247, 248, 252);
        v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, theme::BORDER);
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, theme::TEXT);
        v.widgets.inactive.corner_radius = egui::CornerRadius::same(9);

        v.widgets.hovered.bg_fill = theme::ACCENT_SOFT;
        v.widgets.hovered.weak_bg_fill = theme::ACCENT_SOFT;
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, theme::ACCENT);
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, theme::ACCENT);
        v.widgets.hovered.corner_radius = egui::CornerRadius::same(9);

        v.widgets.active.bg_fill = theme::ACCENT;
        v.widgets.active.weak_bg_fill = theme::ACCENT;
        v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, theme::ACCENT);
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);
        // RichText::strong() 的前景色取自 widgets.active.text_color()，这里是白色。
        // 浅色背景上的标题若只写 .strong() 就会「隐形」，因此 strong 文本一律显式指定颜色。
        v.widgets.active.corner_radius = egui::CornerRadius::same(9);

        v.widgets.open.bg_fill = theme::CARD_HOVER;
        v.widgets.open.weak_bg_fill = theme::CARD_HOVER;
        v.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, theme::BORDER);
        v.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, theme::TEXT);
        v.widgets.open.corner_radius = egui::CornerRadius::same(9);

        v.striped = false;
        v.button_frame = true;
        v.interact_cursor = Some(egui::CursorIcon::PointingHand);
        style.visuals = v;

        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.window_margin = egui::Margin::same(14);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.icon_width = 18.0;
        style.spacing.icon_width_inner = 10.0;
        style.spacing.combo_width = 120.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;

        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(22.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.5));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    });
}

fn main() -> eframe::Result<()> {
    // 已有实例在跑：把它的主界面调出来，本进程直接退出
    if !single_instance::claim() {
        return Ok(());
    }

    // 先把字体读进内存（约 20MB，几百毫秒），**再**建窗：这样窗口出现时首帧能立刻
    // 画出来，不会出现「窗已可见但内容还没画上」的白底。
    let _ = font_cache();

    let icon = load_icon_data();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_app_id("bell-reminder")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(icon),
        persist_window: true,
        ..Default::default()
    };

    // 第二个实例双击/自启时，通过该线程把主界面唤到前台
    std::thread::spawn(single_instance::wait_for_show);

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(PlanApp::new(cc)))),
    )
}

/// 系统字体字节缓存（微软雅黑 + 彩色 emoji）。
///
/// 为什么单独缓存：`msyh.ttc` + `seguiemj.ttf` 约 20MB，从 `C:\Windows\Fonts` 读一次要
/// 几百毫秒。这段耗时若落在「窗口已创建但首帧还没画出来」的区间里，用户看到的就是
/// 白底空窗（eframe 对不可见窗口每 100ms 补画一次，期间会反复显示/隐藏）。
/// 在 `main` 里、建窗**之前**先把字节读进内存，之后 `install_cjk_fonts` 只做解析。
fn font_cache() -> &'static [(&'static str, Vec<u8>)] {
    static CACHE: OnceLock<Vec<(&'static str, Vec<u8>)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut out = Vec::new();
        for (name, path) in [
            ("msyh", r"C:\Windows\Fonts\msyh.ttc"),
            ("emoji", r"C:\Windows\Fonts\seguiemj.ttf"),
        ] {
            match std::fs::read(path) {
                Ok(bytes) => out.push((name, bytes)),
                Err(e) => eprintln!("[bell] 字体读取失败 {path}：{e}"),
            }
        }
        out
    })
}

/// egui 默认字体不含中文：装上系统字体（微软雅黑 + 彩色 emoji 兜底）。
fn install_cjk_fonts(ctx: &egui::Context) {
    use std::sync::Arc;

    let mut fonts = egui::FontDefinitions::default();
    let mut is_primary = true;
    for (name, bytes) in font_cache() {
        fonts
            .font_data
            .insert((*name).into(), Arc::new(egui::FontData::from_owned(bytes.clone())));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.entry(fam).or_default();
            if is_primary {
                list.insert(0, (*name).into());
            } else {
                list.push((*name).into());
            }
        }
        is_primary = false;
    }
    ctx.set_fonts(fonts);
}

/// 取主窗口的 HWND。
#[cfg(windows)]
fn window_hwnd(cc: &eframe::CreationContext<'_>) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = cc.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

/// 进程内缓存的主窗口句柄（托盘唤醒原生窗口时用）。
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);


/// 窗口是否处于「收到托盘」状态。
static HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 记录主窗口句柄，供托盘/双击唤醒使用。
/// 把标题栏强制设为浅色（与界面的浅色主题一致）。
///
/// 为什么需要：升级到 eframe 0.37（egui-winit 0.37）后，标题栏变成深色，而界面是
/// 浅色 —— 视觉上非常突兀。
///
/// 排查结论：窗口的 `DWMWA_USE_IMMERSIVE_DARK_MODE` 被子设成了 1（深色），
/// 运行时把它改回 0 立刻恢复浅色标题栏（实测标题栏由 R=38 变为 R=194）。系统本身
/// 是浅色主题（`AppsUseLightTheme=1`），所以这属于库的默认值问题。
///
/// 时机：必须等窗口真正创建后写才生效；在 `PlanApp::new` 里窗口尚未完成初始化，
/// 写进去会被覆盖，因此放到第一次绘制之后（见 `ui()` 里的调用）。
#[cfg(windows)]
fn force_light_titlebar() {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

    let raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = raw as *mut std::ffi::c_void;
    let off: i32 = 0; // 0 = 浅色标题栏
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &off as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn force_light_titlebar() {}

fn remember_main_hwnd(cc: &eframe::CreationContext<'_>) {
    #[cfg(windows)]
    if let Some(hwnd) = window_hwnd(cc) {
        MAIN_HWND.store(hwnd as isize, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 原生兜底：显示并置前主窗口。
fn native_show_window() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };

        let raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        if raw == 0 {
            return;
        }
        let hwnd = raw as *mut std::ffi::c_void;
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
}

/// 原生兜底：隐藏主窗口。用分层窗口（WS_EX_LAYERED + 全透明）而不是 SW_HIDE：
/// SW_HIDE 后 winit 不再派发 RedrawRequested，eframe 的 update() 会停摆，
/// 托盘菜单与每秒调度就都不再运行（表现为「点打开主界面没反应、提醒也不再弹」）。
/// 分层窗口保持「可见」状态以继续收帧；再用省略任务栏扩展样式移除任务栏按钮。
fn native_hide_window() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, ShowWindow,
            GWL_EXSTYLE, HWND_TOP, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WS_EX_APPWINDOW, WS_EX_LAYERED,
            WS_EX_TOOLWINDOW,
        };

        let raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        if raw == 0 {
            return;
        }
        let hwnd = raw as *mut std::ffi::c_void;
        HIDDEN.store(true, std::sync::atomic::Ordering::Relaxed);

        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        // WS_EX_TOOLWINDOW 会移除任务栏按钮；同时保留 WS_EX_LAYERED 让窗口继续收帧。
        SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            (ex | WS_EX_LAYERED as i32 | WS_EX_TOOLWINDOW as i32) & !(WS_EX_APPWINDOW as i32),
        );
        SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        // 样式切换后任务栏偶尔仍保留旧标签，显式刷新隐藏状态。
        // 注意：这对调用只用于「隐藏到托盘」这条路径；启动阶段绝不走这里，否则就是
        // 用户看到的「窗口闪一下」。
        ShowWindow(hwnd, SW_HIDE);
        ShowWindow(hwnd, SW_SHOW);
    }
}

/// 原生兜底：取消透明、恢复任务栏按钮，并显示主窗口。
fn native_restore_window_pos() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, IsIconic, SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos,
            ShowWindow, GWL_EXSTYLE, HWND_TOP, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE,
            SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, SW_SHOW, WS_EX_APPWINDOW,
            WS_EX_LAYERED, WS_EX_TOOLWINDOW,
        };

        if !HIDDEN.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        if raw == 0 {
            return;
        }
        let hwnd = raw as *mut std::ffi::c_void;

        // 恢复为普通应用窗口：不透明、重新回到任务栏。
        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            (ex & !(WS_EX_LAYERED as i32 | WS_EX_TOOLWINDOW as i32)) | WS_EX_APPWINDOW as i32,
        );
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
    }
}

fn load_icon_data() -> egui::IconData {
    let img = image::load_from_memory(ICON_PNG)
        .expect("内嵌图标损坏")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

fn load_tray_icon() -> tray_icon::Icon {
    let img = image::load_from_memory(ICON_PNG)
        .expect("内嵌图标损坏")
        .to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("托盘图标创建失败")
}

// ============================== 存储 ==============================

/// 数据读写后端：**永远是本地 JSON 文件**。
///
/// Git（`repo/`）只作同步中转，不再是数据源 —— 见 `store::GitStore` 的说明。
/// 因此「存储方式」这个设置只影响**要不要同步**，不影响数据放在哪里。
struct Backend {
    local: DocStore,
    /// 配置了 Git 时附带的同步通道（不参与读写）。
    git: Option<GitStore>,
}

impl Backend {
    fn load(&self) -> anyhow::Result<Doc> {
        self.local.load()
    }

    fn save(&self, doc: &Doc) -> anyhow::Result<()> {
        self.local.save(doc)
    }

    fn describe(&self) -> String {
        match &self.git {
            Some(git) => format!("Git {}", store::display_url(&git.url())),
            None => format!("本地 {}", self.local.dir().display()),
        }
    }
}

/// 根据设置构建后端。数据源恒为本地数据目录；Git 只是同一目录上的同步通道
/// （数据目录本身就是 git 工作副本，见 `store::GitStore`）。
fn build_backend(settings: &Settings) -> Backend {
    let dir = store::app_dir();
    let local = DocStore::new(dir.join(store::DOC_DIR_NAME));
    let git = if git_ready(settings) {
        Some(GitStore::new(dir.join(store::DOC_DIR_NAME), settings.git.clone()))
    } else {
        None
    };
    Backend { local, git }
}

/// 防抖间隔：改动停止后 3 秒执行一轮同步（有道云笔记式）。
///
/// 有道云笔记的行为是「保存后几秒内同步到所有设备」——不是攒几分钟的大延迟，
/// 而是一个很短的防抖窗口：连续编辑期间每次改动都会把窗口重置，停手 3 秒后
/// 立即执行一轮 sync_round（本地改动推上去 + 远端新内容拉下来）。
/// 比起 5 分钟长延迟：多端不一致的窗口从分钟级缩到秒级，且用户能立刻看到
/// 侧栏绿点变回「已同步」。
const PUSH_DELAY: Duration = Duration::from_secs(3);

/// 待同步标记（有道云笔记式）。
///
/// 只需要知道「什么时候到期」和「提交说明写什么」；推送内容永远是当时的
/// 本地数据目录（`bell-data/plans/`），所以中途再怎么改都会被下一次推送带上。
#[derive(Clone)]
struct PushQueueEntry {
    /// 入队时刻。`elapsed() >= PUSH_DELAY` 即到期。
    queued_at: Instant,
    /// 最后一次改动的描述（作为提交信息）。
    reason: String,
}

/// 防抖同步的语义（对应 `PlanApp::enqueue_push` / `poll_sync`）：
///
/// ```text
/// 增删改 → persist 立即落盘 → enqueue_push（重置 3 秒窗口）
///   ├─ 空闲且窗口到期 → sync_round：本地改动 push + 远端新内容 pull
///   └─ 正在同步       → 标记保留；本轮结束后下一秒 tick 再查
 /// ```
///
 /// 冲突（两边都有改动）不弹窗打断：生成「冲突副本」后照常同步，见
 /// `sync_round_with_conflict_copy`。

/// 后台同步任务的回报。
enum SyncMsg {
    Finished(&'static str, Result<String, String>),
    /// 切换到 Git 后首次拉取的结果：Ok(true) = 远端有数据并已写回本地；
    /// Ok(false) = 远端还没有数据文件。
    Switched(Result<bool, String>),
}

// ============================== 编辑对话框状态 ==============================

/// 主界面的页面。日期记录与提醒模板分开管理，后续可继续加入每周/每月回顾等模块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspacePage {
    Calendar,
    Templates,
}

/// 日期记录的显示范围。真正的数据始终按具体日期保存。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DateView {
    Day,
    Week,
    Month,
    /// 月历网格：一眼看出哪些日期有计划/提醒，以及当天是否已完成。
    Calendar,
}

impl DateView {
    fn label(self) -> &'static str {
        match self {
            Self::Day => "日",
            Self::Week => "周",
            Self::Month => "月",
            Self::Calendar => "日历",
        }
    }

    const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::Calendar];
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepeatKind {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// 编辑对象：模板或某一天的具体记录。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    Template(Uuid),
    Record(Uuid),
}

struct EditState {
    target: Option<EditTarget>,
    record_only: bool,
    date: NaiveDate,
    title: String,
    tag: String,
    /// 标签下拉是否展开（同一个输入框既选择也输入）。
    tag_open: bool,
    note: String,
    hour: u32,
    minute: u32,
    repeat_kind: RepeatKind,
    /// 提前几天开始提醒。
    adv_days: u32,
    /// 延后几天结束提醒；`None` = 不限（勾选框未选中）。
    grace_days: Option<u32>,
    /// 窗口内的重响间隔（分钟）。
    every_minutes: u32,
    weekdays: [bool; 7],
    month_day: u32,
    year_month: u32,
    year_day: u32,
    /// 日期选择器浮层是否展开。
    date_open: bool,
    /// 日期选择器当前翻到的月份（与选中日期解耦，翻月不会改选中值）。
    cal_month: NaiveDate,
    /// 时间选择器浮层是否展开。
    time_open: bool,
    error: Option<String>,
}

impl EditState {
    fn new_template() -> Self {
        let today = Local::now().date_naive();
        Self {
            target: None,
            record_only: false,
            date: today,
            title: String::new(),
            tag: String::new(),
            tag_open: false,
            note: String::new(),
            hour: 9,
            minute: 0,
            repeat_kind: RepeatKind::Daily,
            adv_days: 0,
            grace_days: None,
            // 新建默认 1 小时提醒一次：30 分钟对大部分事项太频繁。
            every_minutes: 60,
            weekdays: [true, true, true, true, true, false, false],
            month_day: today.day(),
            year_month: today.month(),
            year_day: today.day(),
            date_open: false,
            cal_month: today,
            time_open: false,
            error: None,
        }
    }

    fn new_record(date: NaiveDate) -> Self {
        let mut state = Self::new_template();
        state.record_only = true;
        state.date = date;
        state
    }

    fn from_template(p: &Plan) -> Self {
        let today = Local::now().date_naive();
        let (repeat_kind, weekdays, month_day, year_month, year_day) = match p.repeat {
            RepeatRule::Daily => (
                RepeatKind::Daily,
                [true, true, true, true, true, false, false],
                today.day(),
                today.month(),
                today.day(),
            ),
            RepeatRule::Weekly { weekdays } => (
                RepeatKind::Weekly,
                weekdays,
                today.day(),
                today.month(),
                today.day(),
            ),
            RepeatRule::Monthly { day } => (
                RepeatKind::Monthly,
                [true, true, true, true, true, false, false],
                day,
                today.month(),
                today.day(),
            ),
            RepeatRule::Yearly { month, day } => (
                RepeatKind::Yearly,
                [true, true, true, true, true, false, false],
                today.day(),
                month,
                day,
            ),
        };
        Self {
            target: Some(EditTarget::Template(p.id)),
            record_only: false,
            date: today,
            title: p.title.clone(),
            tag: p.tag.clone(),
            tag_open: false,
            note: p.note.clone(),
            hour: p.time.hour(),
            minute: p.time.minute(),
            repeat_kind,
            adv_days: p.remind.advance_days,
            grace_days: p.remind.grace_days,
            every_minutes: p.remind.every_minutes,
            weekdays,
            month_day,
            year_month,
            year_day,
            date_open: false,
            cal_month: today,
            time_open: false,
            error: None,
        }
    }

    fn from_record(r: &DayRecord) -> Self {
        let mut state = Self::new_record(r.date);
        state.target = Some(EditTarget::Record(r.id));
        state.record_only = true;
        state.title = r.title.clone();
        state.tag = r.tag.clone();
        state.note = r.note.clone();
        state.hour = r.time.hour();
        state.minute = r.time.minute();
        state.adv_days = r.remind.advance_days;
        state.grace_days = r.remind.grace_days;
        state.every_minutes = r.remind.every_minutes;
        state
    }

    fn time(&self) -> NaiveTime {
        NaiveTime::from_hms_opt(self.hour.min(23), self.minute.min(59), 0)
            .unwrap_or_else(|| NaiveTime::from_hms_opt(9, 0, 0).unwrap())
    }

    fn repeat(&self) -> Result<RepeatRule, String> {
        match self.repeat_kind {
            RepeatKind::Daily => Ok(RepeatRule::Daily),
            RepeatKind::Weekly => RepeatRule::Weekly {
                weekdays: self.weekdays,
            }
            .validate(),
            RepeatKind::Monthly => RepeatRule::Monthly {
                day: self.month_day,
            }
            .validate(),
            RepeatKind::Yearly => RepeatRule::Yearly {
                month: self.year_month,
                day: self.year_day,
            }
            .validate(),
        }
    }

    /// 把 UI 上的三项选择组合成提醒策略。
    fn remind(&self) -> Result<RemindWindow, String> {
        RemindWindow {
            advance_days: self.adv_days,
            grace_days: self.grace_days,
            every_minutes: self.every_minutes,
        }
        .validate()
    }

    fn build(&self) -> Result<Draft, String> {
        let title = self.title.trim().to_string();
        if title.is_empty() {
            return Err("请填写标题".into());
        }
        let tag = self.tag.trim();
        if tag.chars().count() > 12 {
            return Err("标签最多 12 个字".into());
        }
        Ok(Draft {
            title,
            tag: tag.to_string(),
            note: self.note.trim().to_string(),
            time: self.time(),
            repeat: self.repeat()?,
            remind: self.remind()?,
        })
    }

    fn is_template(&self) -> bool {
        !self.record_only && matches!(self.target, None | Some(EditTarget::Template(_)))
    }
}

/// 编辑框校验通过后的成品数据。
struct Draft {
    title: String,
    tag: String,
    note: String,
    time: NaiveTime,
    repeat: RepeatRule,
    remind: RemindWindow,
}
fn repeat_kind_label(kind: RepeatKind) -> &'static str {
    match kind {
        RepeatKind::Daily => "每天",
        RepeatKind::Weekly => "每周",
        RepeatKind::Monthly => "每月",
        RepeatKind::Yearly => "每年",
    }
}

fn edit_field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(11.5)
            .strong()
            .color(theme::TEXT_WEAK),
    );
    ui.add_space(2.0);
}

/// 估算提醒窗口内一共会响多少次，用于给用户一个「这会不会太吵」的提示。
fn window_fire_count(state: &EditState) -> u32 {
    let advance_min = state.adv_days.saturating_mul(1440);
    let grace_min = state.grace_days.map(|g| g.saturating_mul(1440));
    // 没有延后上限时，只按「提前 + 1 天」估，避免显示无穷大。
    let span = advance_min.saturating_add(grace_min.unwrap_or(1440));
    let every = state.every_minutes.max(1);
    // 至少 1 次：进入窗口时总会响一次，哪怕窗口在计划时刻就结束。
    (span / every + 1).max(1)
}

/// 「延后」行的小胶囊按钮：选中时强调色实心，未选中时透明底。
fn grace_chip(text: &str, active: bool) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(text.to_owned()).size(11.5).strong().color(if active {
            Color32::WHITE
        } else {
            theme::TEXT_WEAK
        }),
    )
    .fill(if active {
        theme::ACCENT
    } else {
        Color32::TRANSPARENT
    })
    .stroke(if active {
        egui::Stroke::NONE
    } else {
        egui::Stroke::new(1.0_f32, theme::BORDER)
    })
    .corner_radius(egui::CornerRadius::same(7))
    .min_size(egui::vec2(46.0, 24.0))
}

// ============================== 提醒弹窗 ==============================

/// 右下角独立通知小窗里的一条待处理提醒。
struct Popup {
    record_id: Uuid,
    title: String,
    /// 记录标签（每日计划 / 生日提醒 …）。空串表示没打标签。
    tag: String,
    note: String,
    /// 计划日期与时刻，显示在标题下方，让人一眼知道这是哪一场安排。
    date: NaiveDate,
    time: NaiveTime,
}

#[derive(Clone, Copy)]
enum PopupAction {
    Dismiss(Uuid),
    Complete(Uuid),
    /// 一次性收起所有提醒窗（弹窗堆叠时不用一个个点）。
    DismissAll,
}

// ============================== 主应用 ==============================

/// 删模板前的确认：模板本身一定会删，是否连带删掉它铺出来的日期记录由用户勾选。
struct DeletePrompt {
    template_id: Uuid,
    title: String,
    /// 该模板名下仍然存在的（未删除）记录数。
    record_count: usize,
    /// 勾选=连记录一起删；不勾=记录保留并转为独立记录。
    remove_records: bool,
}

/// 删单条记录前的确认：只记 id 和标题，弹窗里显示给用户核对。
struct RecordDeletePrompt {
    record_id: Uuid,
    title: String,
    date: NaiveDate,
    time: NaiveTime,
}

/// 哪一类对话框要保持打开（其余全部关闭）。
///
/// 用来保证「同一时刻只有一个模态对话框」：卡片双击和标题单击曾同时命中，
/// 一帧里连开两个窗，表现为「偶尔弹两个编辑框」。
#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogKeep {
    Edit,
    View,
    DeleteTemplate,
    DeleteRecord,
    Settings,
}

struct PlanApp {
    settings: Settings,
    doc: Doc,
    page: WorkspacePage,
    selected_date: NaiveDate,
    date_view: DateView,
    view_record: Option<Uuid>,
    edit: Option<EditState>,
    delete_prompt: Option<DeletePrompt>,
    /// 删除单条记录前的确认框状态。
    record_delete_prompt: Option<RecordDeletePrompt>,
    /// 日历视图双击某天后弹出的「当天记录」窗口。
    day_popup: Option<NaiveDate>,
    /// 上一帧「当天记录」窗口的位置，用来判断「点了窗口外面就关掉」。
    day_popup_rect: Option<egui::Rect>,
    show_settings: bool,
    /// 设置对话框里的临时编辑值。
    cfg_kind: Kind,
    cfg_git: GitConfig,
    cfg_error: Option<String>,
    cfg_msg: Option<String>,

    popups: Vec<Popup>,
    /// 每个提醒窗上一帧实际占用的内容高度（不含内边距），用于把窗口收到刚好包住内容。
    popup_heights: std::collections::HashMap<Uuid, f32>,
    last_tick: Instant,
    /// 已绘制帧数（首帧创建托盘会拖慢绘制，见 ui() 注释）。
    frames_drawn: u32,
    status: Option<(String, bool, Instant)>,
    syncing: bool,
    /// 有道云笔记式的**延迟推送队列**：增删改只入队，攒 5 分钟再真正 push
    ///（见 `PushQueueEntry`）。`None` = 队列里没有待推送的消息。
    push_queue: Option<PushQueueEntry>,
    sync_tx: Sender<SyncMsg>,
    sync_rx: Receiver<SyncMsg>,

    // 托盘（首帧懒创建，需在事件循环线程上）
    tray: Option<TrayIcon>,
    tray_autostart_item: Option<CheckMenuItem>,
    autolaunch: Option<auto_launch::AutoLaunch>,
    tray_handlers_ready: bool,
    quitting: bool,
}

impl PlanApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);
        remember_main_hwnd(cc);

        let dir = store::app_dir();
        let settings = store::read_json::<Settings>(&dir.join(store::SETTINGS_FILE))
            .ok()
            .flatten()
            .unwrap_or_default();

        // 读本地数据。**不在这里做网络拉取**：`git.pull()` 是 clone/fetch，要几百毫秒
        // 到几十秒；放在构造期会让窗口已出现但内容迟迟画不出来（用户看到的就是白窗）。
        // 启动拉取改为后台线程，结果经 SyncMsg::Finished("startup-pull") 回填。
        let backend = build_backend(&settings);
        // 旧布局迁移：`bell-data/plans.json` 单文件 → `bell-data/plans/` 按年目录。
        // 失败不阻断启动 —— 读旧布局也能用（DocStore::load 会返回空，这里再兜底读旧文件）。
        match backend
            .local
            .migrate_legacy(&dir.join(store::LOCAL_DOC_FILE))
        {
            Ok(true) => eprintln!("[bell] 已把 plans.json 迁移为按年目录"),
            Ok(false) => {}
            Err(e) => eprintln!("[bell] 旧数据迁移失败：{e}"),
        }
        // 同理：settings 里 Git 的 `path` 若还写着旧的单文件名，自动升成目录名并保存。
        // 否则中转副本里会出现「同名文件挡住同名目录」的冲突，同步报「创建目录失败」。
        if settings.kind == Kind::Git && settings.git.path.trim() == store::LOCAL_DOC_FILE {
            let mut s = settings.clone();
            s.git.path = store::DOC_DIR_NAME.to_string();
            let path = dir.join(store::SETTINGS_FILE);
            if store::write_json(&path, &s).is_ok() {
                eprintln!("[bell] 已把 Git 同步路径从 plans.json 升级为 plans/");
            }
        }
        let doc = backend.load().unwrap_or_else(|e| {
            eprintln!("[bell] 读取计划失败：{e}");
            Doc::default()
        });

        let autolaunch = auto_launch::AutoLaunchBuilder::new()
            .set_app_name("bell-reminder")
            .set_app_path(
                &std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
            .build()
            .ok();
        if let Some(a) = &autolaunch {
            // 让注册表与配置保持一致
            let _ = if settings.autostart {
                a.enable()
            } else {
                a.disable()
            };
        }

        let (sync_tx, sync_rx) = channel();

        // 预热 git 可用性探测：这个探测要起一个 `git --version` 子进程（35–70 ms）。
        // 设置页每帧都要读它，缓存一次即可；这里提前在后台线程算好，第一次点开设置也不卡。
        std::thread::spawn(|| {
            let _ = store::git_available();
        });

        let startup_pull = git_ready(&settings);
        // 启动拉取（Git 模式）放后台：网络交互不该挡住首帧。
        // 数据目录本身就是 git 工作副本，拉取 = fetch + reset --hard 直接作用在数据上。
        // 拉到后经 "startup-pull" 重新读盘刷新界面。
        if startup_pull {
            let tx = sync_tx.clone();
            let cfg = settings.git.clone();
            let local_dir = store::app_dir().join(store::DOC_DIR_NAME);
            std::thread::spawn(move || {
                let git = GitStore::new(local_dir, cfg);
                let res = git
                    .pull()
                    .map(|pulled| {
                        if pulled {
                            "已从 Git 拉取最新".to_string()
                        } else {
                            String::new() // 远端还没有数据，静默
                        }
                    })
                    .map_err(|e| e.to_string());
                let _ = tx.send(SyncMsg::Finished("startup-pull", res));
            });
        }

        Self {
            cfg_kind: settings.kind.clone(),
            cfg_git: settings.git.clone(),
            settings,
            doc,
            page: WorkspacePage::Calendar,
            selected_date: Local::now().date_naive(),
            date_view: DateView::Day,
            view_record: None,
            edit: None,
            delete_prompt: None,
            record_delete_prompt: None,
            day_popup: None,
            day_popup_rect: None,
            show_settings: false,
            cfg_error: None,
            cfg_msg: None,
            popups: Vec::new(),
            popup_heights: std::collections::HashMap::new(),
            last_tick: Instant::now(),
            frames_drawn: 0,
            status: None,
            syncing: startup_pull,
            push_queue: None,
            sync_tx,
            sync_rx,
            tray: None,
            tray_autostart_item: None,
            autolaunch,
            tray_handlers_ready: false,
            quitting: false,
        }
    }

    // ---------- 持久化 ----------

    fn persist(&self) -> anyhow::Result<()> {
        // 只写本地文件。Git（若有）是同步通道，写盘这一步与它无关 —— 推送由
        // start_push 在后台单独做，这样「保存」永远是本地的、瞬间完成的动作。
        build_backend(&self.settings).save(&self.doc)
    }

    /// 存盘，并（Git 模式）把一次推送**排进延迟队列**。
    ///
    /// 所有增删改都走这里：本地落盘永远是立即的、同步完成的；网络推送则攒
    /// `PUSH_DELAY`（5 分钟）再做 —— 停手之后才联网，连续编辑只推最后一次。
    fn save_and_sync(&mut self, reason: &str) {
        if let Err(e) = self.persist() {
            self.set_status(format!("保存失败：{e}"), false);
            return;
        }
        self.enqueue_push(reason.to_string());
    }

    /// 把一次推送请求放入延迟队列（有道云笔记式）。
    ///
    /// - 队列空 → 入队开始计时；
    /// - 已有消息且**尚未开始推送** → 丢弃旧消息，新消息重新计时（合并连续编辑）；
    /// - **正在推送** → 正常排队，同样等满 5 分钟。当前任务结束后若尚未到期，
    ///   消息留在队列等 tick 检查；到期才执行。
    ///
    /// 三种情况都统一「等满 5 分钟」，行为一致、更好解释：改动只管入队，
    /// 推送时刻永远是最后一次入队 + PUSH_DELAY。
    fn enqueue_push(&mut self, reason: String) {
        if self.settings.kind != Kind::Git || !git_ready(&self.settings) {
            return;
        }
        self.push_queue = Some(PushQueueEntry {
            queued_at: Instant::now(),
            reason,
        });
    }

    /// 延迟队列到期（或用户点「立即同步」）：执行一轮智能同步。
    ///
    /// 核心是 `store::GitStore::sync_round` 的四象限判定：
    /// 本机没改动+远端没新内容 → no-op；本机有改动+远端没新内容 → 直接推；
    /// 本机没改动+远端有新内容 → 拉；**两边都有 → 弹「同步确认」让用户选**。
    ///
    /// 为什么不是「先 pull 再 push」：队列消息本身就意味着本地有未推送改动，
    /// 先 pull 会用远端全量数据把它冲掉（实测：勾选的完成状态被悄悄还原）。
    fn run_due_push(&mut self, reason: String) {
        if self.settings.kind != Kind::Git || !git_ready(&self.settings) {
            // 环境不可用：直接丢弃队列消息（下次改动会重新入队）
            self.push_queue = None;
            return;
        }
        if self.syncing {
            // 理论上到不了这里（tick 只在空闲时调用），保险起见把消息留回队列
            self.push_queue = Some(PushQueueEntry {
                queued_at: Instant::now() - PUSH_DELAY, // 立即到期
                reason,
            });
            return;
        }
        self.syncing = true;
        let settings = self.settings.clone();
        let tx = self.sync_tx.clone();
        let local_dir = store::app_dir().join(store::DOC_DIR_NAME);
        std::thread::spawn(move || {
            let git = GitStore::new(local_dir, settings.git.clone());
            let res = git
                .sync_round(&reason)
                .map(|outcome| match outcome {
                    store::SyncOutcome::Pushed => "已推送到 Git".to_string(),
                    store::SyncOutcome::Pulled => "已从 Git 拉取最新".to_string(),
                    store::SyncOutcome::Noop => "已是最新，无需同步".to_string(),
                    store::SyncOutcome::ConflictCopied => {
                        "同步冲突：本机改动已存为冲突副本（conflict/ 分支），主数据已更新为远端".to_string()
                    }
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(SyncMsg::Finished("sync", res));
        });
        // 队列消息已被取出执行，清空
        self.push_queue = None;
    }

    /// 手动同步（首页「立即同步」按钮）：立即执行一轮智能同步。
    ///
    /// 与延迟队列到期执行的是同一条路径 —— 只是把计时清零、立刻触发。
    /// 顺带清掉队列：用户主动同步说明现在就要最新状态，排队中的消息并入这次。
    fn manual_sync(&mut self) {
        self.run_due_push("手动同步".to_string());
    }


    fn poll_sync(&mut self) {
        while let Ok(msg) = self.sync_rx.try_recv() {
            match msg {
                SyncMsg::Finished(what, res) => {
                    self.syncing = false;
                    match res {
                        Ok(text) => {
                            if !text.is_empty() {
                                self.set_status(text, true);
                            }
                            // 重新读盘（sync_round 拉取、手动「立即同步」、启动拉取都要；
                            // 新模型下数据目录本身就是 git 工作副本，pull 已直接落到数据上）
                            if matches!(what, "pull" | "startup-pull" | "sync") {
                                let backend = build_backend(&self.settings);
                                if let Ok(doc) = backend.load() {
                                    self.doc = doc;
                                    self.popups.clear();
                                    // 立刻重绘，别等下一次 1 秒 tick —— 用户点「立即同步」
                                    // 后要马上看到卡片状态变化（勾变绿）。
                                    request_ui_wake();
                                }
                            }
                        }
                        Err(e) => self.set_status(format!("同步失败：{e}"), false),
                    }
                }
                SyncMsg::Switched(res) => {
                    self.syncing = false;
                    // 拉取期间用户可能又切回本地存储：此时忽略这份远端结果，避免把本地
                    // 文档覆盖掉。
                    if self.settings.kind != Kind::Git {
                        continue;
                    }
                    match res {
                        Ok(true) => {
                            // 已把远端内容写回本地数据文件，重新读盘
                            let backend = build_backend(&self.settings);
                            if let Ok(doc) = backend.load() {
                                self.doc = doc;
                                self.popups.clear();
                            }
                            self.cfg_error = None;
                            self.cfg_msg = Some("已切换到 Git 存储并同步成功".into());
                            self.set_status("已从 Git 拉取最新".into(), true);
                        }
                        Ok(false) => {
                            // 远端还没有数据文件：把当前计划推上去
                            self.cfg_error = None;
                            self.cfg_msg = Some("已切换到 Git 存储，正在上传现有计划".into());
                            self.save_and_sync("初始化数据文件");
                        }
                        Err(e) => {
                            self.cfg_error = Some(format!("拉取失败：{e}"));
                            self.cfg_msg = None;
                        }
                    }
                }
            }
        }
    }

    fn set_status(&mut self, text: String, ok: bool) {
        self.status = Some((text, ok, Instant::now()));
    }

    // ---------- 托盘 / 窗口 ----------

    fn ensure_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }
        let menu = Menu::new();
        let open = MenuItem::with_id(MENU_OPEN, "打开主界面", true, None);
        let autostart = CheckMenuItem::with_id(
            MENU_AUTOSTART,
            "开机自启",
            true,
            self.settings.autostart,
            None,
        );
        let quit = MenuItem::with_id(MENU_QUIT, "退出", true, None);
        let _ = menu.append(&open);
        let _ = menu.append(&autostart);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);
        match TrayIconBuilder::new()
            .with_tooltip(APP_TITLE)
            .with_icon(load_tray_icon())
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(tray) => {
                self.tray = Some(tray);
                self.tray_autostart_item = Some(autostart);
            }
            Err(e) => eprintln!("[bell] 托盘创建失败：{e}"),
        }
    }

    /// 显示主界面。
    fn show_main_window(&self, ctx: &egui::Context) {
        native_restore_window_pos();
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        native_show_window();
    }

    /// 隐藏到托盘：分层透明，保持事件循环运行。
    fn hide_main_window(&mut self, _ctx: &egui::Context) {
        native_hide_window();
    }

    fn set_autostart(&mut self, on: bool) {
        self.settings.autostart = on;
        self.save_settings();
        if let Some(a) = &self.autolaunch {
            let res = if on { a.enable() } else { a.disable() };
            if let Err(e) = res {
                eprintln!("[bell] 设置开机自启失败：{e}");
            }
        }
        if let Some(item) = &self.tray_autostart_item {
            item.set_checked(on);
        }
    }

    fn save_settings(&mut self) {
        let path = store::app_dir().join(store::SETTINGS_FILE);
        if let Err(e) = store::write_json(&path, &self.settings) {
            self.set_status(format!("保存设置失败：{e}"), false);
        }
    }

    fn handle_tray_events(&mut self, ctx: &egui::Context) {
        if !self.tray_handlers_ready {
            install_tray_event_handlers(ctx);
            self.tray_handlers_ready = true;
        }

        let actions = TRAY_EVENTS
            .get()
            .and_then(|queue| queue.lock().ok().map(|mut q| std::mem::take(&mut *q)))
            .unwrap_or_default();
        for action in actions {
            match action {
                TrayAction::Open => self.show_main_window(ctx),
                TrayAction::ToggleAutostart => {
                    let on = !self.settings.autostart;
                    self.set_autostart(on);
                }
                TrayAction::Quit => {
                    // 托盘回调已主动唤醒 UI；这里走正常关闭流程，释放托盘与进程。
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    // ---------- 模板生成与调度 ----------

    /// 让模板覆盖指定日期范围。已存在（包括被删除的墓碑）与用户定制过的记录都不会被覆盖。
    /// `persist=true` 在用户切换日期视图时落盘；后台预生成只改内存。
    fn materialize_range(&mut self, start: NaiveDate, end: NaiveDate, reason: &str, persist: bool) {
        let mut created = Vec::new();
        for template in &self.doc.templates {
            if !template.enabled {
                continue;
            }
            let mut date = start;
            while date <= end {
                if template.repeat.occurs_on(date)
                    && self.doc.record_for(template.id, date).is_none()
                {
                    created.push(DayRecord::from_template(template, date));
                }
                date += ChronoDuration::days(1);
            }
        }
        if created.is_empty() {
            return;
        }
        let count = created.len();
        self.doc.records.extend(created);
        if persist {
            self.save_and_sync(reason);
            self.set_status(format!("已从模板生成 {count} 条日期记录"), true);
        }
    }

    /// 模板内容变化时，只同步未改过的未来记录，保留历史/手工覆盖。
    fn sync_template_future_records(&mut self, template_id: Uuid, today: NaiveDate) {
        let Some(template) = self.doc.template(template_id).cloned() else {
            return;
        };
        for record in self.doc.records.iter_mut().filter(|r| {
            r.template_id == Some(template_id) && r.date >= today && !r.customized && !r.done
        }) {
            record.title = template.title.clone();
            record.tag = template.tag.clone();
            record.note = template.note.clone();
            record.time = template.time;
            record.remind = template.remind;
            // 不清 `last_fired`：清了会让这条记录提前单独响一次，
            // 与同频率的其它记录错开相位。窗口变化由 `check_fire` 自动处理。
        }
    }

    /// 每秒调度一次：只处理今天已经生成的记录。
    fn tick(&mut self, ctx: &egui::Context) {
        if self.last_tick.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_tick = Instant::now();

        // 延迟推送队列：到期（入队后满 PUSH_DELAY 且当前空闲）就执行一轮
        // 「先 pull 再 push」。materialize/check_fire 可能刚触发过入队，所以
        // 这段放在它们之前、用「到期才执行」的判断自然合并。
        if !self.syncing {
            if let Some(entry) = &self.push_queue {
                if entry.queued_at.elapsed() >= PUSH_DELAY {
                    let reason = entry.reason.clone();
                    self.run_due_push(reason);
                    ctx.request_repaint();
                    return; // 本轮 tick 结束，等后台回报
                }
            }
        }

        let now = Local::now();
        let today = now.date_naive();
        // 今天及之后 90 天预先生成，打开任意日期时都能看到稳定记录。
        self.materialize_range(
            today,
            today + ChronoDuration::days(90),
            "生成日程记录",
            false,
        );

        // 提前提醒的弹窗在「记录日 - N 天」就会出现，所以按各自的提醒窗口判断，
        // 而不是简单地要求「记录日期等于今天」。
        let stale: Vec<Uuid> = self
            .popups
            .iter()
            .filter(|p| {
                self.doc
                    .record(p.record_id)
                    .is_none_or(|r| !r.in_reminder_window(&now))
            })
            .map(|p| p.record_id)
            .collect();
        if !stale.is_empty() {
            self.popups.retain(|p| !stale.contains(&p.record_id));
        }

        let mut fired = Vec::new();
        // 弹窗已存在、只是刷新了 last_fired 的记录：也要落盘，否则下次启动会重算时间。
        let mut state_only = false;
        for r in self.doc.records.iter_mut() {
            if let Some(upd) = r.check_fire(&now) {
                if self.popups.iter().any(|x| x.record_id == r.id) {
                    upd.apply_to(r);
                    state_only = true;
                    continue;
                }
                upd.apply_to(r);
                fired.push((
                    r.id,
                    r.title.clone(),
                    r.tag.clone(),
                    r.note.clone(),
                    r.date,
                    r.time,
                ));
            }
        }

        // 只有「仅刷新状态」而没有新弹窗时，也要保存一次。
        if fired.is_empty() {
            if state_only {
                self.save_and_sync("刷新提醒状态");
            }
            if !self.popups.is_empty() {
                ctx.request_repaint();
            }
            return;
        }

        for (record_id, title, tag, note, date, time) in fired {
            self.popups.push(Popup {
                record_id,
                title,
                tag,
                note,
                date,
                time,
            });
        }
        self.save_and_sync("触发提醒");
        if self.settings.sound {
            notify::play();
        }
        ctx.request_repaint();
    }

    // ---------- 记录与模板操作 ----------

    // ---------- 对话框互斥打开 ----------
    //
    // 所有「打开某个弹窗」的入口都走这几个方法，统一先关掉其它弹窗。
    // 原因：卡片双击 + 标题单击曾同时命中，一帧里连开两个窗，
    // 用户看到的就是「偶尔弹两个编辑框」。从入口处保证单例最可靠。

    /// 关掉除指定项以外的所有弹窗。
    fn close_all_dialogs_except(&mut self, keep: DialogKeep) {
        if keep != DialogKeep::Edit {
            self.edit = None;
        }
        if keep != DialogKeep::View {
            self.view_record = None;
        }
        if keep != DialogKeep::DeleteTemplate {
            self.delete_prompt = None;
        }
        if keep != DialogKeep::DeleteRecord {
            self.record_delete_prompt = None;
        }
        if keep != DialogKeep::Settings {
            self.show_settings = false;
        }
    }

    fn open_edit_record(&mut self, record_id: Uuid) {
        let Some(r) = self.doc.record(record_id) else {
            return;
        };
        let state = EditState::from_record(r);
        self.open_edit_state(state);
    }

    /// 打开编辑框（新建记录 / 新建模板 / 编辑模板 / 编辑记录都走这里），
    /// 并保证同一时刻只开这一个。
    fn open_edit_state(&mut self, state: EditState) {
        self.close_all_dialogs_except(DialogKeep::Edit);
        self.edit = Some(state);
    }

    fn open_settings(&mut self) {
        self.close_all_dialogs_except(DialogKeep::Settings);
        self.show_settings = true;
    }

    fn open_record_view(&mut self, record_id: Uuid) {
        self.close_all_dialogs_except(DialogKeep::View);
        self.view_record = Some(record_id);
    }

    fn open_record_delete(&mut self, prompt: RecordDeletePrompt) {
        self.close_all_dialogs_except(DialogKeep::DeleteRecord);
        self.record_delete_prompt = Some(prompt);
    }

    fn set_done(&mut self, record_id: Uuid, done: bool) {
        if let Some(r) = self.doc.records.iter_mut().find(|x| x.id == record_id) {
            if done {
                r.mark_done();
            } else {
                r.clear_done();
            }
        }
        if done {
            self.popups.retain(|p| p.record_id != record_id);
        }
        self.save_and_sync(if done { "完成记录" } else { "取消完成" });
    }

    fn cancel_record(&mut self, record_id: Uuid) {
        if let Some(r) = self.doc.records.iter_mut().find(|x| x.id == record_id) {
            r.cancelled = true;
            r.snooze_until = None;
        }
        self.popups.retain(|p| p.record_id != record_id);
        self.save_and_sync("删除日期记录");
    }

    fn toggle_template(&mut self, template_id: Uuid, enabled: bool) {
        if let Some(t) = self.doc.templates.iter_mut().find(|x| x.id == template_id) {
            t.enabled = enabled;
        }
        if !enabled {
            self.popups.retain(|p| {
                self.doc.record(p.record_id).and_then(|r| r.template_id) != Some(template_id)
            });
        }
        self.save_and_sync("启用/停用模板");
    }

    /// 点「删除」时先弹确认：模板要删，它铺出来的记录是否一并删除由用户勾选。
    fn ask_delete_template(&mut self, template_id: Uuid) {
        let Some(t) = self.doc.template(template_id) else {
            return;
        };
        let record_count = self
            .doc
            .records
            .iter()
            .filter(|r| r.template_id == Some(template_id) && !r.cancelled)
            .count();
        self.delete_prompt = Some(DeletePrompt {
            template_id,
            title: t.title.clone(),
            record_count,
            // 默认不勾选：删模板不动历史记录，最保守。
            remove_records: false,
        });
    }

    /// 执行删除。`remove_records=true` 时把该模板生成的记录一并标记删除（保留墓碑，
    /// 避免模板复活后补回），否则记录保留并转为独立记录，继续显示在各自日期下。
    fn delete_template(&mut self, template_id: Uuid, remove_records: bool) {
        let affected: Vec<Uuid> = self
            .doc
            .records
            .iter()
            .filter(|r| r.template_id == Some(template_id))
            .map(|r| r.id)
            .collect();

        self.doc.templates.retain(|t| t.id != template_id);

        let mut removed = 0usize;
        for id in &affected {
            if let Some(r) = self.doc.records.iter_mut().find(|r| r.id == *id) {
                if remove_records {
                    if !r.cancelled {
                        removed += 1;
                    }
                    r.cancelled = true;
                    r.snooze_until = None;
                } else {
                    // 保留记录：脱离模板成为独立记录，后续不会再被模板逻辑影响。
                    r.template_id = None;
                }
            }
        }

        // 被删掉的记录不再留在右下角提醒里；保留下来的记录继续提醒。
        self.popups
            .retain(|p| self.doc.record(p.record_id).is_some_and(|r| !r.cancelled));

        let reason = if remove_records {
            "删除提醒模板及其日期记录"
        } else {
            "删除提醒模板"
        };
        self.save_and_sync(reason);
        self.set_status(
            if remove_records {
                format!("已删除模板，并删除 {removed} 条日期记录")
            } else if affected.is_empty() {
                "已删除提醒模板".into()
            } else {
                format!(
                    "已删除模板，{affected_cnt} 条日期记录保留为独立记录",
                    affected_cnt = affected.len()
                )
            },
            true,
        );
    }

    fn save_edit(&mut self) {
        let Some(state) = self.edit.take() else {
            return;
        };
        let Draft {
            title,
            tag,
            note,
            time,
            repeat,
            remind,
        } = match state.build() {
            Ok(v) => v,
            Err(msg) => {
                self.edit = Some(EditState {
                    error: Some(msg),
                    ..state
                });
                return;
            }
        };

        // 保存时顺带把标签记入字典，下次下拉里就能直接选。
        self.doc.remember_tag(&tag);

        match state.target {
            Some(EditTarget::Record(record_id)) => {
                if let Some(r) = self.doc.records.iter_mut().find(|x| x.id == record_id) {
                    r.date = state.date;
                    r.title = title;
                    r.tag = tag;
                    r.note = note;
                    r.time = time;
                    r.remind = remind;
                    r.customized = true;
                    // 注意：**不要**在这里清 `last_fired`。
                    // 清空会让这条记录立刻重响一次并与其它记录错开相位，
                    // 表现为「同频率的提醒不再同时出现」。
                    // 窗口整体挪动（改日期）的情况由 `check_fire` 里的
                    // `last_fired >= start` 判断自然处理。
                    r.snooze_until = None;
                }
                self.save_and_sync("修改日期记录");
            }
            Some(EditTarget::Template(template_id)) => {
                let today = Local::now().date_naive();
                if let Some(t) = self.doc.templates.iter_mut().find(|x| x.id == template_id) {
                    t.title = title;
                    t.tag = tag;
                    t.note = note;
                    t.time = time;
                    t.repeat = repeat;
                    t.remind = remind;
                }
                self.sync_template_future_records(template_id, today);
                self.save_and_sync("修改提醒模板");
            }
            None if state.record_only => {
                self.doc.records.push(DayRecord {
                    id: Uuid::new_v4(),
                    template_id: None,
                    date: state.date,
                    title,
                    tag,
                    note,
                    time,
                    remind,
                    done: false,
                    done_at: None,
                    cancelled: false,
                    customized: true,
                    last_fired: None,
                    snooze_until: None,
                    created_at: Local::now(),
                });
                self.save_and_sync("新建当天记录");
            }
            None => {
                let mut t = Plan::new(title, note, time);
                t.tag = tag;
                t.repeat = repeat;
                t.remind = remind;
                let template_id = t.id;
                self.doc.templates.push(t);
                if self
                    .doc
                    .template(template_id)
                    .is_some_and(|t| t.repeat.occurs_on(state.date))
                {
                    if let Some(t) = self.doc.template(template_id) {
                        self.doc
                            .records
                            .push(DayRecord::from_template(t, state.date));
                    }
                }
                self.save_and_sync("新建提醒模板");
            }
        }
    }

    fn record_progress(&self, date: NaiveDate) -> (usize, usize) {
        let indices = self.doc.record_indices_for(date);
        let total = indices.len();
        let done = indices
            .iter()
            .filter(|&&i| self.doc.records[i].done)
            .count();
        (done, total)
    }

    fn enabled_template_count(&self) -> usize {
        self.doc.templates.iter().filter(|t| t.enabled).count()
    }
}

fn git_ready(settings: &Settings) -> bool {
    settings.kind == Kind::Git && settings.git.is_complete()
}

impl eframe::App for PlanApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.frames_drawn = self.frames_drawn.saturating_add(1);
        // 托盘**不在首帧创建**：`TrayIconBuilder::build()` 要和 Windows 通知区交互
        // （实测约 20ms），放在首帧会把这一帧拖到 30ms 以上 —— 而 eframe 对「不可见窗口」
        // 每 100ms 补画一次，首帧越慢，窗口在内容画好之前被显示/隐藏的次数就越多
        // （用户看到的就是启动时白窗闪两三下）。
        // 推迟一帧后，首帧只剩纯绘制（约 8ms），窗口第一次出现就是画好的界面。
        if self.frames_drawn > 1 {
            self.ensure_tray();
        }
        // 窗口已真正创建后再把标题栏设为浅色（见 force_light_titlebar 说明）。
        // 只在前几帧写：库的深色默认值是启动阶段设的，之后不会反复改回来。
        if (2..=6).contains(&self.frames_drawn) {
            force_light_titlebar();
        }
        self.handle_tray_events(ctx);
        self.poll_sync();

        // 关闭按钮 → 隐藏到托盘；只有托盘「退出」才允许真正关闭。
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.hide_main_window(ctx);
        }

        // 另一个进程双击了 exe → 把窗口调出来
        if SHOW_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.show_main_window(ctx);
            ctx.request_repaint();
        }

        self.tick(ctx);

        ui_sidebar(self, ui);
        match self.page {
            WorkspacePage::Calendar => ui_calendar_workspace(self, ui),
            WorkspacePage::Templates => ui_template_workspace(self, ui),
        }
        ui_record_view_dialog(self, ctx);
        ui_day_popup(self, ctx);
        ui_edit_dialog(self, ctx);
        ui_delete_template_dialog(self, ctx);
        ui_record_delete_dialog(self, ctx);
        ui_settings(self, ctx);
        ui_popups(self, ctx);

        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

// ============================== 通用小控件 ==============================

/// 主按钮（强调色）。
fn button_primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(text.to_owned())
            .strong()
            .color(Color32::WHITE),
    )
    .fill(theme::ACCENT)
    .stroke(egui::Stroke::NONE)
    .corner_radius(egui::CornerRadius::same(9))
    .min_size(egui::vec2(0.0, 34.0))
}

/// 次要按钮（透明底）。
fn button_ghost(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(theme::TEXT_WEAK))
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .corner_radius(egui::CornerRadius::same(9))
        .min_size(egui::vec2(0.0, 32.0))
}

/// 小号文字按钮。
fn button_mini(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(text.to_owned())
            .size(11.0)
            .color(theme::TEXT_WEAK),
    )
    .fill(Color32::TRANSPARENT)
    .stroke(egui::Stroke::NONE)
    .corner_radius(egui::CornerRadius::same(7))
    .min_size(egui::vec2(0.0, 26.0))
}

/// 深色提醒窗里的图标按钮：圆圈包住一个 ×，只画图标不带文字。
///
/// 用 painter 自绘而不是渲染 `✕` 字符：深色底上线条颜色可控，
/// 也不怕字体缺字显示成方框。
fn icon_button(ui: &mut egui::Ui, icon: CloseIcon, tip: &str) -> bool {
    let size = egui::vec2(26.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let color = if resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgb(150, 160, 184)
    };
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let c = rect.center();
    // 外圈：悬停时填充淡底，常态只有描边。
    let ring_r = 8.5;
    if resp.hovered() {
        painter.circle_filled(c, ring_r + 1.5, Color32::from_rgb(56, 66, 98));
    }
    painter.circle_stroke(c, ring_r, egui::Stroke::new(1.4_f32, color));
    // 圈内的 ×：略小于圈半径，留出呼吸空间。
    let r = 3.8;
    let stroke = egui::Stroke::new(1.5_f32, color);
    match icon {
        CloseIcon::Single => {
            painter.line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
            painter.line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
        }
        CloseIcon::All => {
            // 「全部」：圈内画两个并排的小 ×。
            let rr = 2.6;
            for dx in [-3.0_f32, 3.0] {
                let cc = c + egui::vec2(dx, 0.0);
                painter.line_segment([cc + egui::vec2(-rr, -rr), cc + egui::vec2(rr, rr)], stroke);
                painter.line_segment([cc + egui::vec2(rr, -rr), cc + egui::vec2(-rr, rr)], stroke);
            }
        }
    }
    resp.on_hover_text(tip).clicked()
}

/// 关闭图标的两种形态。
#[derive(Clone, Copy)]
enum CloseIcon {
    /// 关闭单条。
    Single,
    /// 关闭全部。
    All,
}

// ============================== 输入框 ==============================

/// 输入框的统一外观：浅底 + 细边框 + 8px 圆角 + 内边距。
///
/// egui 0.34 的 `TextEdit::frame()` 接收的是 `Frame` 而不是 `bool`，
/// 所以「有没有外框」改由这里传入的 Frame 决定：想要外框就传
/// `input_frame()`，嵌在自己画好边框的容器里（比如标签下拉）就传
/// `egui::Frame::NONE`。
fn input_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(Color32::from_rgb(248, 249, 252))
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 7))
}

/// 聚焦时在输入框外面补一圈强调色描边（egui 的自定义 Frame 不会自己加）。
fn paint_focus_ring(ui: &egui::Ui, resp: &egui::Response) {
    if resp.has_focus() {
        ui.painter().rect_stroke(
            resp.rect.expand(1.0),
            egui::CornerRadius::same(9),
            egui::Stroke::new(1.6_f32, theme::ACCENT),
            egui::StrokeKind::Outside,
        );
    }
}

/// 带自定义宽度和占位提示的单行输入框。
fn text_input_sized(
    ui: &mut egui::Ui,
    text: &mut String,
    width: f32,
    hint: &str,
) -> egui::Response {
    let resp = ui.add(
        egui::TextEdit::singleline(text)
            .frame(input_frame())
            .desired_width(width)
            .hint_text(hint),
    );
    paint_focus_ring(ui, &resp);
    resp
}

/// 多行输入框（统一外观 + 聚焦高亮）。
fn text_input_multiline(ui: &mut egui::Ui, text: &mut String, rows: usize) -> egui::Response {
    let resp = ui.add(
        egui::TextEdit::multiline(text)
            .frame(input_frame())
            .desired_width(f32::INFINITY)
            .desired_rows(rows),
    );
    paint_focus_ring(ui, &resp);
    resp
}

// ============================== 日期 / 时间选择器 ==============================

/// 日期选择器的触发按钮：显示当前日期，点击展开月历浮层。
fn date_picker(
    ui: &mut egui::Ui,
    date: &mut NaiveDate,
    open: &mut bool,
    cal_month: &mut NaiveDate,
) -> egui::Response {
    let today = Local::now().date_naive();
    // 整块都可点：图标、文字、箭头任何位置点下去都能展开，
    // 否则只有文字那几十像素有反应，很容易点空。
    let inner = egui::Frame::NONE
        .fill(Color32::from_rgb(248, 249, 252))
        .stroke(egui::Stroke::new(
            1.0_f32,
            if *open { theme::ACCENT } else { theme::BORDER },
        ))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("📅").size(12.0));
                ui.label(
                    RichText::new(format!(
                        "{}  周{}",
                        date.format("%Y年%m月%d日"),
                        weekday_cn(*date)
                    ))
                    .size(12.5)
                    .color(theme::TEXT),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(if *open { "▴" } else { "▾" })
                        .size(10.0)
                        .color(theme::TEXT_WEAK),
                );
            });
        });
    let rect = inner.response.rect;
    // 用 Frame 自己的 response 转成可点击：这样图标、文字、箭头整块都能点中。
    let resp = inner.response.interact(egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *open = !*open;
        if *open {
            // 打开时把月历翻到当前选中日期所在月。
            *cal_month = *date;
        }
    }

    if *open {
        let anchor = rect.left_bottom() + egui::vec2(0.0, 4.0);
        let area = egui::Area::new(egui::Id::new("date_picker_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(252.0);
                        month_grid(ui, date, cal_month, today, open);
                    });
            });
        // 点浮层和按钮之外 → 收起。
        if ui.input(|i| i.pointer.any_click()) {
            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                if !area.response.rect.contains(p) && !rect.contains(p) {
                    *open = false;
                }
            }
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            *open = false;
        }
    }
    resp
}

/// 月历网格：上个月 / 月份标题 / 下个月 + 7 列日期格。
fn month_grid(
    ui: &mut egui::Ui,
    date: &mut NaiveDate,
    cal_month: &mut NaiveDate,
    today: NaiveDate,
    open: &mut bool,
) {
    let first = first_of_month(*cal_month);
    let last = last_of_month(*cal_month);
    let lead = first.weekday().num_days_from_monday() as i64;
    let cells = (lead + (last - first).num_days() + 1) as usize;
    let grid_start = first - ChronoDuration::days(lead);

    ui.horizontal(|ui| {
        if ui.add(button_mini("‹")).clicked() {
            *cal_month = shift_month(*cal_month, -1);
        }
        ui.add_space(2.0);
        ui.label(
            RichText::new(format!("{} 年 {} 月", cal_month.year(), cal_month.month()))
                .size(12.5)
                .strong()
                .color(theme::TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(button_mini("›")).clicked() {
                *cal_month = shift_month(*cal_month, 1);
            }
            if *cal_month != today && ui.add(button_mini("今天")).clicked() {
                *cal_month = today;
                *date = today;
                *open = false;
            }
        });
    });
    ui.add_space(6.0);

    // 用真实的 Button + Grid 排日期格：自绘 `ui.interact` 在浮层里收不到点击，
    // 而 Button 走的是标准交互路径，行为稳定（时间选择器同款做法）。
    let mut picked: Option<NaiveDate> = None;
    egui::Grid::new("date_pick_grid")
        .num_columns(7)
        .spacing([3.0, 3.0])
        .show(ui, |ui| {
            for name in ["一", "二", "三", "四", "五", "六", "日"] {
                ui.allocate_ui_with_layout(
                    egui::vec2(30.0, 16.0),
                    Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.label(
                            RichText::new(name)
                                .size(10.5)
                                .color(theme::TEXT_WEAK),
                        );
                    },
                );
            }
            ui.end_row();

            for idx in 0..cells {
                let d = grid_start + ChronoDuration::days(idx as i64);
                let in_month = d.month() == cal_month.month() && d.year() == cal_month.year();
                let selected = d == *date;
                let is_today = d == today;

                let fg = if selected {
                    Color32::WHITE
                } else if !in_month {
                    Color32::from_rgb(183, 189, 203)
                } else if is_today {
                    theme::ACCENT
                } else {
                    theme::TEXT
                };
                let fill = if selected {
                    theme::ACCENT
                } else {
                    Color32::TRANSPARENT
                };
                let stroke = if is_today && !selected {
                    egui::Stroke::new(1.2_f32, theme::ACCENT)
                } else {
                    egui::Stroke::NONE
                };
                let btn = egui::Button::new(
                    RichText::new(d.day().to_string()).size(12.0).color(fg),
                )
                .fill(fill)
                .stroke(stroke)
                .corner_radius(egui::CornerRadius::same(6))
                .min_size(egui::vec2(30.0, 26.0));
                if ui.add(btn).clicked() {
                    picked = Some(d);
                }
                if idx % 7 == 6 {
                    ui.end_row();
                }
            }
        });

    if let Some(d) = picked {
        *date = d;
        *cal_month = d;
        *open = false;
    }
}

/// 时间选择器：显示 HH:MM，点击展开「时 + 分」两列快选。
fn time_picker(ui: &mut egui::Ui, hour: &mut u32, minute: &mut u32, open: &mut bool) -> egui::Response {
    // 整块都可点，理由同 date_picker。
    let inner = egui::Frame::NONE
        .fill(Color32::from_rgb(248, 249, 252))
        .stroke(egui::Stroke::new(
            1.0_f32,
            if *open { theme::ACCENT } else { theme::BORDER },
        ))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🕐").size(12.0));
                ui.label(
                    RichText::new(format!("{:02}:{:02}", hour, minute))
                        .size(12.5)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(if *open { "▴" } else { "▾" })
                        .size(10.0)
                        .color(theme::TEXT_WEAK),
                );
            });
        });
    let rect = inner.response.rect;
    // 用 Frame 自己的 response 转成可点击：这样图标、文字、箭头整块都能点中。
    let resp = inner.response.interact(egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *open = !*open;
    }

    if *open {
        let anchor = rect.left_bottom() + egui::vec2(0.0, 4.0);
        let area = egui::Area::new(egui::Id::new("time_picker_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(196.0);
                        ui.horizontal(|ui| {
                            // 左列：小时（0-23，两列排布省高度）
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("时")
                                        .size(10.5)
                                        .color(theme::TEXT_WEAK),
                                );
                                ui.add_space(2.0);
                                egui::Grid::new("hour_grid")
                                    .num_columns(4)
                                    .spacing([3.0, 3.0])
                                    .show(ui, |ui| {
                                        for h in 0..24u32 {
                                            let sel = *hour == h;
                                            let b = egui::Button::new(
                                                RichText::new(format!("{h:02}"))
                                                    .size(11.0)
                                                    .color(if sel {
                                                        Color32::WHITE
                                                    } else {
                                                        theme::TEXT
                                                    }),
                                            )
                                            .fill(if sel {
                                                theme::ACCENT
                                            } else {
                                                Color32::TRANSPARENT
                                            })
                                            .stroke(egui::Stroke::NONE)
                                            .corner_radius(egui::CornerRadius::same(6))
                                            .min_size(egui::vec2(24.0, 22.0));
                                            if ui.add(b).clicked() {
                                                *hour = h;
                                            }
                                            if (h + 1) % 4 == 0 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                            });
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("分")
                                        .size(10.5)
                                        .color(theme::TEXT_WEAK),
                                );
                                ui.add_space(2.0);
                                egui::Grid::new("minute_grid")
                                    .num_columns(4)
                                    .spacing([3.0, 3.0])
                                    .show(ui, |ui| {
                                        for m in [0u32, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55] {
                                            let sel = *minute == m;
                                            let b = egui::Button::new(
                                                RichText::new(format!("{m:02}"))
                                                    .size(11.0)
                                                    .color(if sel {
                                                        Color32::WHITE
                                                    } else {
                                                        theme::TEXT
                                                    }),
                                            )
                                            .fill(if sel {
                                                theme::ACCENT
                                            } else {
                                                Color32::TRANSPARENT
                                            })
                                            .stroke(egui::Stroke::NONE)
                                            .corner_radius(egui::CornerRadius::same(6))
                                            .min_size(egui::vec2(24.0, 22.0));
                                            if ui.add(b).clicked() {
                                                *minute = m;
                                            }
                                            if (m / 5 + 1) % 4 == 0 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                            });
                        });
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.add(button_mini("现在")).clicked() {
                                let now = Local::now();
                                *hour = now.hour();
                                *minute = now.minute();
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.add(button_mini("完成")).clicked() {
                                    *open = false;
                                }
                            });
                        });
                    });
            });
        if ui.input(|i| i.pointer.any_click()) {
            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                if !area.response.rect.contains(p) && !rect.contains(p) {
                    *open = false;
                }
            }
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            *open = false;
        }
    }
    resp
}

/// 自绘圆形勾选框：未完成=空心圆环，已完成=实心绿圆 + 白勾。
fn check_circle(ui: &mut egui::Ui, checked: bool, size: f32) -> bool {    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let center = rect.center();
    let r = size * 0.5;
    if checked {
        // 已完成使用高对比实心绿圆，避免浅色底 + 白勾在白卡片上看不清。
        ui.painter().circle_filled(center, r, theme::OK);
        let p = [
            egui::pos2(center.x - r * 0.46, center.y + r * 0.02),
            egui::pos2(center.x - r * 0.12, center.y + r * 0.34),
            egui::pos2(center.x + r * 0.48, center.y - r * 0.34),
        ];
        ui.painter().add(egui::Shape::line(
            p.to_vec(),
            egui::Stroke::new(2.4_f32, Color32::WHITE),
        ));
    } else {
        let stroke_color = if resp.hovered() {
            theme::ACCENT
        } else {
            Color32::from_rgb(105, 113, 132)
        };
        ui.painter()
            .circle_stroke(center, r - 1.0, egui::Stroke::new(2.0_f32, stroke_color));
        if resp.hovered() {
            ui.painter()
                .circle_filled(center, r - 2.5, theme::ACCENT_SOFT);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// 今天不发生的计划用静态占位，明确表示这里不可勾选。

/// 与可用宽度同宽的细进度条。

/// 小圆角标签。
fn badge(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(fg));
        });
}

/// 开关胶囊。
fn switch(ui: &mut egui::Ui, on: bool, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(34.0, 19.0), egui::Sense::click());
    let track = if on {
        theme::ACCENT_DIM
    } else {
        Color32::from_rgb(52, 58, 70)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(10), track);
    let cx = if on {
        rect.right() - 9.5
    } else {
        rect.left() + 9.5
    };
    ui.painter()
        .circle_filled(egui::pos2(cx, rect.center().y), 7.0, Color32::WHITE);
    resp.on_hover_text(tip).clicked()
}

// ============================== 界面 ==============================

fn ui_sidebar(app: &mut PlanApp, root: &mut egui::Ui) {
    egui::Panel::left("workspace_sidebar")
        .exact_size(220.0)
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(theme::SIDEBAR)
                .inner_margin(egui::Margin::symmetric(16, 20)),
        )
        .show_inside(root, |ui| {
            ui.horizontal(|ui| {
                brand_mark(ui, 38.0);
                ui.add_space(2.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(APP_NAME)
                            .size(20.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.label(
                        RichText::new(APP_SUBTITLE)
                            .size(11.5)
                            .color(Color32::from_rgb(157, 168, 196)),
                    );
                });
            });
            ui.add_space(28.0);
            ui.label(
                RichText::new("日程")
                    .size(10.5)
                    .strong()
                    .color(Color32::from_rgb(116, 128, 158)),
            );
            ui.add_space(6.0);
            if nav_item(ui, "▦", "日期记录", app.page == WorkspacePage::Calendar).clicked() {
                app.page = WorkspacePage::Calendar;
            }
            ui.add_space(18.0);
            ui.label(
                RichText::new("自动化")
                    .size(10.5)
                    .strong()
                    .color(Color32::from_rgb(116, 128, 158)),
            );
            ui.add_space(6.0);
            if nav_item(ui, "⟳", "提醒模板", app.page == WorkspacePage::Templates).clicked() {
                app.page = WorkspacePage::Templates;
            }
            if nav_item(ui, "＋", "新建模板", false).clicked() {
                app.open_edit_state(EditState::new_template());
            }
            ui.add_space(18.0);
            ui.label(
                RichText::new("工作台")
                    .size(10.5)
                    .strong()
                    .color(Color32::from_rgb(116, 128, 158)),
            );
            ui.add_space(6.0);
            if nav_item(ui, "⚙", "设置", false).clicked() {
                open_settings(app);
            }

            ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                let (mode, detail) = match app.settings.kind {
                    Kind::Local => ("本地存储", "数据仅保存在这台电脑"),
                    Kind::Git => ("Git 同步", "改动后自动推送到仓库"),
                };
                ui.label(
                    RichText::new("v0.3 · 计划与提醒")
                        .size(10.0)
                        .color(Color32::from_rgb(92, 105, 133)),
                );
                ui.add_space(10.0);
                egui::Frame::NONE
                    .fill(theme::SIDEBAR_HOVER)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(11, 9))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (dot, _) =
                                ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                            ui.painter().circle_filled(
                                dot.center(),
                                3.5,
                                if app.syncing { theme::WARN } else { theme::OK },
                            );
                            ui.label(
                                RichText::new(if app.syncing {
                                    "正在同步"
                                } else if app.push_queue.is_some() {
                                    "待推送"
                                } else {
                                    mode
                                })
                                .size(11.5)
                                .strong()
                                .color(Color32::from_rgb(219, 225, 242)),
                            );
                        });
                        ui.label(
                            RichText::new(if app.push_queue.is_some() && !app.syncing {
                                format!("改动已暂存，{}后同步", remaining_str(app))
                            } else {
                                detail.to_string()
                            })
                            .size(9.5)
                            .color(Color32::from_rgb(126, 139, 168)),
                        );
                    });
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("设置与存储")
                                .size(12.0)
                                .color(Color32::from_rgb(178, 188, 211)),
                        )
                        .fill(theme::SIDEBAR_HOVER)
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(48, 58, 87)))
                        .corner_radius(egui::CornerRadius::same(10))
                        .min_size(egui::vec2(ui.available_width(), 36.0)),
                    )
                    .clicked()
                {
                    open_settings(app);
                }
                ui.add_space(8.0);
            });
        });
}

fn ui_calendar_workspace(app: &mut PlanApp, root: &mut egui::Ui) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(26, 22)),
        )
        .show_inside(root, |ui| {
            let today = Local::now().date_naive();
            let (done, total) = app.record_progress(app.selected_date);
            let templates = app.enabled_template_count();
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("日程记录")
                            .size(24.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new("模板自动铺到具体日期，每一天都可以单独调整")
                            .size(12.0)
                            .color(theme::TEXT_WEAK),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_ghost("设置")).clicked() {
                        open_settings(app);
                    }
                    ui.add_space(4.0);
                    // 手动同步：立即执行一轮「先 pull 再 push」，并清掉延迟队列
                    // （用户主动同步说明现在就要最新状态，排队中的消息并入这次）。
                    if ui
                        .add_enabled(
                            !app.syncing,
                            button_ghost(if app.syncing { "同步中…" } else { "立即同步" }),
                        )
                        .clicked()
                    {
                        app.push_queue = None;
                        app.manual_sync();
                    }
                    ui.add_space(4.0);
                    // 「今日记录」：始终针对今天，不跟随上面选中的日期。
                    if ui.add(button_primary("＋  今日记录")).clicked() {
                        app.open_edit_state(EditState::new_record(Local::now().date_naive()));
                    }
                    ui.add_space(4.0);
                    if ui.add(button_ghost("管理模板")).clicked() {
                        app.page = WorkspacePage::Templates;
                    }
                });
            });
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                // 三张卡按可用宽度均分。horizontal 会为每个相邻控件自动插入
                // item_spacing（共 2 个间隙），再加上 Frame 左右各 14px 的内边距，
                // 三项都要扣掉，否则第三张卡会被挤出可视区。
                let gap = ui.spacing().item_spacing.x;
                let margin_x = 14.0 * 2.0;
                let card_w = ((ui.available_width() - gap * 2.0) / 3.0 - margin_x).max(72.0);
                metric_card(
                    ui,
                    card_w,
                    "当日待办",
                    &(total.saturating_sub(done)).to_string(),
                    theme::ACCENT,
                );
                metric_card(
                    ui,
                    card_w,
                    "当日已完成",
                    &format!("{done}/{total}"),
                    theme::ACCENT,
                );
                metric_card(ui, card_w, "启用模板", &templates.to_string(), theme::OK);
            });
            ui.add_space(12.0);
            ui_status_line(app, ui);
            ui.add_space(9.0);
            ui_date_navigation(app, today, ui);
            ui.add_space(8.0);
            ui_date_view_tabs(app, ui);
            ui.add_space(7.0);

            let (start, end) = visible_range(app.date_view, app.selected_date);
            app.materialize_range(start, end, "生成选中日期记录", true);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.date_view == DateView::Calendar {
                        // 提示行放在月历上方：月历本身很高，放在下面会被挤出可视区。
                        ui_calendar_hint(app, ui, today);
                        ui.add_space(8.0);
                        ui_month_grid(app, ui, today);
                    } else {
                        let mut cursor = start;
                        while cursor <= end {
                            let n = app.doc.record_indices_for(cursor).len();
                            if app.date_view == DateView::Day || n > 0 || cursor == today {
                                ui_day_group(app, ui, cursor, today);
                            }
                            cursor += ChronoDuration::days(1);
                        }
                    }
                });
        });
}

/// 日历下方的一行说明 + 当天概览。
///
/// 当天明细已经搬进「双击格子」弹出的窗口，这里只留一条可点的入口，
/// 免得月历被下面一长串卡片顶得看不见。
fn ui_calendar_hint(app: &mut PlanApp, ui: &mut egui::Ui, today: NaiveDate) {
    let date = app.selected_date;
    let (done, total) = app.record_progress(date);
    let mut open = false;
    egui::Frame::NONE
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{}  周{}",
                        date.format("%m月%d日"),
                        weekday_cn(date)
                    ))
                    .size(13.0)
                    .strong()
                    .color(if date == today {
                        theme::ACCENT
                    } else {
                        theme::TEXT
                    }),
                );
                if date == today {
                    badge(ui, "今天", theme::ACCENT, theme::ACCENT_SOFT);
                }
                badge(
                    ui,
                    &format!("{done}/{total}"),
                    if total > 0 && done == total {
                        theme::OK
                    } else {
                        theme::TEXT_WEAK
                    },
                    if total > 0 && done == total {
                        theme::OK_DIM
                    } else {
                        theme::CARD_HOVER
                    },
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new("双击日历格子可打开当天记录")
                        .size(11.0)
                        .color(theme::TEXT_WEAK),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("打开当天记录")).clicked() {
                        open = true;
                    }
                });
            });
        });
    if open {
        app.day_popup = Some(date);
    }
}

/// 双击日历格子弹出的「当天记录」窗口：内容与日期页一致，只是独立成窗。
fn ui_day_popup(app: &mut PlanApp, ctx: &egui::Context) {
    let Some(date) = app.day_popup else {
        app.day_popup_rect = None;
        return;
    };
    let today = Local::now().date_naive();
    let mut open = true;
    let mut close = false;
    // 点窗口外面的空白处就收起，符合弹窗的直觉；点窗口内部不算。
    let outside_clicked = ctx.input(|i| {
        i.pointer.any_click()
            && app
                .day_popup_rect
                .is_some_and(|r| !r.contains(i.pointer.interact_pos().unwrap_or_default()))
    });
    let window = egui::Window::new("当天记录")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::BG)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_width(468.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{}  周{}",
                        date.format("%Y年%m月%d日"),
                        weekday_cn(date)
                    ))
                    .size(16.0)
                    .strong()
                    .color(theme::TEXT),
                );
                if date == today {
                    badge(ui, "今天", theme::ACCENT, theme::ACCENT_SOFT);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(400.0)
                // 垂直方向按内容收缩：只有记录多到超过 400 才出现滚动条，
                // 否则窗口刚好包住卡片，不留大片空白。
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui_day_group(app, ui, date, today);
                });
        });
    // 记下本帧的窗口范围，下一帧据此判断点击是否落在窗口外。
    app.day_popup_rect = window.map(|w| w.response.rect);
    if close || !open || outside_clicked {
        app.day_popup = None;
        app.day_popup_rect = None;
    }
}

/// 当前视图需要铺开并显示的日期范围。
fn visible_range(view: DateView, anchor: NaiveDate) -> (NaiveDate, NaiveDate) {
    match view {
        DateView::Day => (anchor, anchor),
        DateView::Week => {
            let start = anchor - ChronoDuration::days(anchor.weekday().num_days_from_monday() as i64);
            (start, start + ChronoDuration::days(6))
        }
        DateView::Month | DateView::Calendar => (first_of_month(anchor), last_of_month(anchor)),
    }
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

fn last_of_month(d: NaiveDate) -> NaiveDate {
    let first = first_of_month(d);
    let next = shift_month(first, 1);
    next.pred_opt().unwrap_or(first)
}

/// 按自然月平移，日期超出目标月天数时收敛到当月最后一天。
fn shift_month(d: NaiveDate, delta: i32) -> NaiveDate {
    let total = d.year() * 12 + (d.month() as i32 - 1) + delta;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) as u32 + 1;
    let last = NaiveDate::from_ymd_opt(year, month + 1, 1)
        .and_then(|n| n.pred_opt())
        .map(|n| n.day())
        .unwrap_or(28);
    NaiveDate::from_ymd_opt(year, month, d.day().min(last)).unwrap_or(d)
}

/// 日历格子里的内容摘要。
struct CalDay {
    total: usize,
    done: usize,
    entries: Vec<(String, bool)>,
    cancelled: bool,
}

fn collect_cal_day(app: &PlanApp, date: NaiveDate) -> CalDay {
    let entries: Vec<(String, bool)> = app
        .doc
        .record_indices_for(date)
        .into_iter()
        .map(|i| {
            let r = &app.doc.records[i];
            (r.title.clone(), r.done)
        })
        .collect();
    let total = entries.len();
    let done = entries.iter().filter(|(_, d)| *d).count();
    let cancelled = app
        .doc
        .records
        .iter()
        .any(|r| r.date == date && r.cancelled);
    CalDay {
        total,
        done,
        entries,
        cancelled,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn legend_item(ui: &mut egui::Ui, color: Color32, text: &str) {
    let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(dot.center(), 3.5, color);
    ui.label(RichText::new(text).size(10.5).color(theme::TEXT_WEAK));
}

/// 月历网格：每格显示当天记录数与标题，一眼看出哪些日期有计划提醒。
fn ui_month_grid(app: &mut PlanApp, ui: &mut egui::Ui, today: NaiveDate) {
    let selected = app.selected_date;
    let first = first_of_month(selected);
    let last = last_of_month(selected);
    let lead = first.weekday().num_days_from_monday() as i64;
    let cells = (lead + (last - first).num_days() + 1) as usize;
    let rows = cells.div_ceil(7);
    let grid_start = first - ChronoDuration::days(lead);

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} 年 {} 月", selected.year(), selected.month()))
                .size(16.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.add_space(8.0);
        legend_item(ui, theme::ACCENT, "有计划");
        legend_item(ui, theme::OK, "已完成");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // right_to_left 里先加的靠右，所以按视觉顺序倒着加。
            if ui.add(button_mini("下个月 ›")).clicked() {
                app.selected_date = shift_month(selected, 1);
            }
            if ui.add(button_mini("‹ 上个月")).clicked() {
                app.selected_date = shift_month(selected, -1);
            }
            if ui.add(button_mini("回到今天")).clicked() {
                app.selected_date = today;
            }
        });
    });
    ui.add_space(6.0);

    // 先把整月的数据摘出来，避免绘制时反复借用 app。
    let mut days: Vec<CalDay> = Vec::with_capacity(cells);
    let mut dates: Vec<NaiveDate> = Vec::with_capacity(cells);
    for idx in 0..cells {
        let date = grid_start + ChronoDuration::days(idx as i64);
        dates.push(date);
        days.push(collect_cal_day(app, date));
    }

    let width = ui.available_width();
    let head_h = 22.0;
    let cell_h = 78.0;
    let (area, _) = ui.allocate_exact_size(
        egui::vec2(width, head_h + rows as f32 * cell_h),
        egui::Sense::hover(),
    );
    let cell_w = width / 7.0;
    let painter = ui.painter();

    for (i, name) in ["一", "二", "三", "四", "五", "六", "日"].iter().enumerate() {
        painter.text(
            egui::pos2(area.left() + cell_w * (i as f32 + 0.5), area.top()),
            egui::Align2::CENTER_TOP,
            name,
            egui::FontId::proportional(11.0),
            theme::TEXT_WEAK,
        );
    }

    let max_chars = (((cell_w - 18.0) / 11.0).max(2.0)) as usize;
    let mut clicked: Option<NaiveDate> = None;
    let mut opened: Option<NaiveDate> = None;

    for idx in 0..cells {
        let date = dates[idx];
        let data = &days[idx];
        let row = idx / 7;
        let col = idx % 7;
        let cell = egui::Rect::from_min_size(
            egui::pos2(
                area.left() + cell_w * col as f32,
                area.top() + head_h + cell_h * row as f32,
            ),
            egui::vec2(cell_w, cell_h),
        )
        .shrink2(egui::vec2(2.0, 2.0));

        let in_month = date.month() == selected.month() && date.year() == selected.year();
        let is_selected = date == selected;
        let is_today = date == today;

        let resp = ui.interact(cell, ui.id().with(("cal-cell", date)), egui::Sense::click());
        let bg = if is_selected {
            theme::ACCENT_SOFT
        } else if resp.hovered() {
            theme::CARD_HOVER
        } else {
            theme::CARD
        };
        let stroke = if is_today {
            egui::Stroke::new(1.5_f32, theme::ACCENT)
        } else {
            egui::Stroke::new(1.0_f32, theme::BORDER)
        };
        painter.rect(
            cell,
            egui::CornerRadius::same(9),
            bg,
            stroke,
            egui::StrokeKind::Inside,
        );

        let day_fg = if !in_month {
            Color32::from_rgb(183, 189, 203)
        } else if is_today || is_selected {
            theme::ACCENT
        } else {
            theme::TEXT
        };
        painter.text(
            cell.left_top() + egui::vec2(9.0, 5.0),
            egui::Align2::LEFT_TOP,
            date.day().to_string(),
            egui::FontId::proportional(13.0),
            day_fg,
        );

        if in_month && data.total > 0 {
            let color = if data.done == data.total {
                theme::OK
            } else {
                theme::ACCENT
            };
            painter.circle_filled(
                egui::pos2(cell.right() - 10.0, cell.top() + 11.0),
                3.5,
                color,
            );
        }

        if in_month && !data.entries.is_empty() {
            let clip = cell.shrink2(egui::vec2(6.0, 4.0));
            let cp = painter.with_clip_rect(clip);
            let mut y = cell.top() + 24.0;
            for (title, done) in data.entries.iter().take(3) {
                let color = if *done { theme::TEXT_WEAK } else { theme::TEXT };
                cp.text(
                    egui::pos2(cell.left() + 9.0, y),
                    egui::Align2::LEFT_TOP,
                    truncate_chars(title, max_chars),
                    egui::FontId::proportional(10.5),
                    color,
                );
                y += 14.0;
            }
            if data.entries.len() > 3 {
                cp.text(
                    egui::pos2(cell.left() + 9.0, y),
                    egui::Align2::LEFT_TOP,
                    format!("+{} 条", data.entries.len() - 3),
                    egui::FontId::proportional(10.0),
                    theme::TEXT_WEAK,
                );
            }
        } else if in_month && data.cancelled {
            painter.text(
                egui::pos2(cell.left() + 9.0, cell.top() + 26.0),
                egui::Align2::LEFT_TOP,
                "已删除",
                egui::FontId::proportional(10.0),
                Color32::from_rgb(190, 195, 208),
            );
        }

        let tip = if data.total == 0 {
            format!(
                "{}  无记录\n单击选中，双击打开当天记录",
                date.format("%Y年%m月%d日")
            )
        } else {
            let mut t = format!(
                "{}  已完成 {}/{}",
                date.format("%Y年%m月%d日"),
                data.done,
                data.total
            );
            for (title, done) in data.entries.iter().take(6) {
                t.push_str(&format!("\n{} {}", if *done { "✔" } else { "○" }, title));
            }
            t.push_str("\n双击打开当天记录");
            t
        };
        let resp = resp.on_hover_text(tip);
        // 单击只选中日期；双击才把当天记录弹成独立窗口。
        if resp.double_clicked() {
            opened = Some(date);
        } else if resp.clicked() {
            clicked = Some(date);
        }
    }

    if let Some(d) = clicked {
        app.selected_date = d;
    }
    if let Some(d) = opened {
        app.selected_date = d;
        app.day_popup = Some(d);
    }
}

fn ui_date_navigation(app: &mut PlanApp, today: NaiveDate, ui: &mut egui::Ui) {
    egui::Frame::NONE
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // 日历按月平移，其它视图按固定天数步进。
                let is_calendar = app.date_view == DateView::Calendar;
                let step = match app.date_view {
                    DateView::Day => ChronoDuration::days(1),
                    DateView::Week => ChronoDuration::days(7),
                    DateView::Month => ChronoDuration::days(30),
                    DateView::Calendar => ChronoDuration::days(0),
                };
                if ui.add(button_mini("‹ 上一段")).clicked() {
                    app.selected_date = if is_calendar {
                        shift_month(app.selected_date, -1)
                    } else {
                        app.selected_date - step
                    };
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "{}  周{}",
                        app.selected_date.format("%Y年%m月%d日"),
                        weekday_cn(app.selected_date)
                    ))
                    .size(15.0)
                    .strong()
                    .color(theme::TEXT),
                );
                ui.add_space(4.0);
                if ui.add(button_mini("下一段 ›")).clicked() {
                    app.selected_date = if is_calendar {
                        shift_month(app.selected_date, 1)
                    } else {
                        app.selected_date + step
                    };
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if app.selected_date != today && ui.add(button_mini("回到今天")).clicked() {
                        app.selected_date = today;
                    }
                });
            });
        });
}

fn ui_date_view_tabs(app: &mut PlanApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("显示").size(11.5).color(theme::TEXT_WEAK));
        for view in DateView::ALL {
            let active = app.date_view == view;
            let button = egui::Button::new(RichText::new(view.label()).size(12.5).strong().color(
                if active {
                    Color32::WHITE
                } else {
                    theme::TEXT_WEAK
                },
            ))
            .fill(if active {
                theme::ACCENT
            } else {
                Color32::TRANSPARENT
            })
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(7))
            .min_size(egui::vec2(46.0, 27.0));
            if ui.add(button).clicked() {
                app.date_view = view;
            }
        }
    });
}

fn ui_day_group(app: &mut PlanApp, ui: &mut egui::Ui, date: NaiveDate, today: NaiveDate) {
    let indices = app.doc.record_indices_for(date);
    let (done, total) = app.record_progress(date);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{}  周{}",
                date.format("%m月%d日"),
                weekday_cn(date)
            ))
            .size(14.0)
            .strong()
            .color(if date == today {
                theme::ACCENT
            } else {
                theme::TEXT
            }),
        );
        if date == today {
            badge(ui, "今天", theme::ACCENT, theme::ACCENT_SOFT);
        }
        if total > 0 {
            badge(
                ui,
                &format!("{done}/{total}"),
                if done == total {
                    theme::OK
                } else {
                    theme::TEXT_WEAK
                },
                if done == total {
                    theme::OK_DIM
                } else {
                    theme::CARD_HOVER
                },
            );
        }
        // 添加按钮贴着日期标签排在同一行左侧，不再被推到窗口最右边。
        ui.add_space(4.0);
        if ui.add(button_mini("＋ 添加")).clicked() {
            app.selected_date = date;
            app.open_edit_state(EditState::new_record(date));
        }
    });
    ui.add_space(5.0);

    let mut to_done = None;
    let mut to_edit = None;
    let mut to_cancel = None;
    let mut to_view = None;
    // 已有任何对话框开着时，不再响应卡片双击，避免连点堆出多个窗口。
    let allow_open = app.edit.is_none()
        && app.view_record.is_none()
        && app.delete_prompt.is_none()
        && app.record_delete_prompt.is_none()
        && !app.show_settings;
    let available = ui.available_width();
    let gap = 9.0;
    // 卡片收窄到 216：三行内容（标题 / 标签+时间+提醒 / 操作）都放得下，
    // 同一行能多排一张卡，整体更紧凑。
    let card_w = 216.0;
    let columns = (((available + gap) / (card_w + gap)).floor() as usize).max(1);

    for row in indices.chunks(columns) {
        ui.horizontal_top(|ui| {
            for &index in row {
                let r = &app.doc.records[index];
                ui.allocate_ui_with_layout(
                    egui::vec2(card_w, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(card_w);
                        record_card(
                            ui,
                            r,
                            false,
                            allow_open,
                            &mut to_done,
                            &mut to_edit,
                            &mut to_cancel,
                            &mut to_view,
                        );
                    },
                );
                ui.add_space(gap);
            }
        });
        ui.add_space(2.0);
    }

    if let Some((id, done)) = to_done {
        app.set_done(id, done);
    }
    if let Some(id) = to_view {
        app.open_record_view(id);
    }
    if let Some(id) = to_edit {
        app.open_edit_record(id);
    }
    if let Some(id) = to_cancel {
        // 删除是不可逆操作（记录会被打成墓碑），先弹确认框。
        if let Some(r) = app.doc.record(id) {
            app.open_record_delete(RecordDeletePrompt {
                record_id: id,
                title: r.title.clone(),
                date: r.date,
                time: r.time,
            });
        }
    }
    ui.add_space(8.0);
}

/// 记录卡片的「来源」类型：决定底色与左侧色条。
///
/// 注意：完成状态**不**参与这里的分型，否则一勾完成所有卡片就变成同一种颜色，
/// 反而丢了「模板生成 / 单独记录 / 已修改」的区分。完成用绿边框 + 删除线 + 绿勾表达。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardKind {
    /// 模板铺出来的记录（受模板控制）
    Generated,
    /// 独立记录（手工添加 / 从模板脱离）
    Standalone,
    /// 用户改过这一条，不跟随模板
    Customized,
}

impl CardKind {
    fn of(r: &DayRecord) -> Self {
        if r.customized {
            Self::Customized
        } else if r.template_id.is_some() {
            Self::Generated
        } else {
            Self::Standalone
        }
    }

    /// 卡片底色：三个来源各一色，与是否完成无关。
    fn fill(self) -> Color32 {
        match self {
            Self::Generated => Color32::from_rgb(238, 242, 255),
            Self::Standalone => theme::CARD,
            Self::Customized => Color32::from_rgb(255, 246, 230),
        }
    }

    /// 边框色：比底色深一档。
    fn border(self) -> Color32 {
        match self {
            Self::Generated => Color32::from_rgb(206, 214, 245),
            Self::Standalone => theme::BORDER,
            Self::Customized => Color32::from_rgb(238, 213, 172),
        }
    }

    /// 左侧色条：与来源同色的浓一档版本；单独记录不画条（留白本身也是区分）。
    fn stripe(self) -> Option<Color32> {
        match self {
            Self::Generated => Some(Color32::from_rgb(120, 137, 224)),
            Self::Customized => Some(theme::WARN),
            Self::Standalone => None,
        }
    }

    /// 悬停说明，替代原来的文字徽标。
    fn hint(self) -> &'static str {
        match self {
            Self::Generated => "模板生成：跟随模板规则",
            Self::Standalone => "单独记录：不属于任何模板",
            Self::Customized => "已单独修改：不跟随模板更新",
        }
    }
}
fn record_card(
    ui: &mut egui::Ui,
    r: &DayRecord,
    expanded: bool,
    allow_open: bool,
    to_done: &mut Option<(Uuid, bool)>,
    to_edit: &mut Option<Uuid>,
    to_cancel: &mut Option<Uuid>,
    to_view: &mut Option<Uuid>,
) {
    let _ = expanded;
    let kind = CardKind::of(r);

    // 卡片内部可交互控件（勾选框 / 标题 / 编辑 / 删除）的矩形。
    // 双击判定要排除这些区域，否则会抢走按钮的点击。
    let mut hit_rects: Vec<egui::Rect> = Vec::new();

    // 完成状态不改变底色和边框（否则来源分类全丢），而是在卡片右侧铺一个大对勾水印。
    let inner = egui::Frame::NONE
        .fill(kind.fill())
        .stroke(egui::Stroke::new(1.0_f32, kind.border()))
        .corner_radius(egui::CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // ── 第 1 行：勾选 + 标题（点标题看详情）+ 右上角铅笔 ──
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if r.customized {
                        ui.label(RichText::new("✏️").size(11.0).color(theme::WARN));
                        ui.add_space(1.0);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        // 记录勾选框即将占用的区域（不改布局），供后面排除双击。
                        let before = ui.cursor().min;
                        if check_circle(ui, r.done, 20.0) {
                            *to_done = Some((r.id, !r.done));
                        }
                        hit_rects.push(egui::Rect::from_min_size(
                            before,
                            egui::vec2(20.0, 20.0),
                        ));
                        ui.add_space(7.0);
                        let title = RichText::new(&r.title).size(14.5).strong();
                        let title = if r.done {
                            title.strikethrough().color(theme::TEXT_WEAK)
                        } else {
                            title.color(theme::TEXT)
                        };
                        let resp = ui.add(
                            egui::Label::new(title)
                                .truncate()
                                .halign(Align::LEFT)
                                .sense(egui::Sense::click()),
                        );
                        hit_rects.push(resp.rect);
                        // 关键：点标题是「看详情」，双击卡片是「编辑」。
                        // `button_clicked()` 在双击/三击时**也返回 true**，所以这里必须
                        // 排除双击，否则一次双击会同时弹出「查看记录」和「编辑」两个窗，
                        // 用户看到的就是「偶尔弹两个编辑框」。
                        if resp.clicked() && !resp.double_clicked() {
                            *to_view = Some(r.id);
                        }
                        let resp = resp.on_hover_text("点击查看详情");
                        let _ = resp;
                    });
                });
            });

            // ── 第 2 行：标签 + 时间 + 提醒方式，压成一行小字 ──
            // 时间先占位（right_to_left 靠右），左侧标签再吃剩余宽度，长标签不会挤掉时间。
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // 提醒窗口：非默认值才写，用最短的写法，悬停看详情。
                    let hint = r.remind.hint_short();
                    if !hint.is_empty() {
                        ui.label(
                            RichText::new(hint)
                                .size(10.0)
                                .color(theme::ACCENT_DIM),
                        )
                        .on_hover_text(r.remind.describe());
                    }
                    // 逾期标记：窗口已过但还没完成。
                    if r.is_overdue(&Local::now()) {
                        ui.label(
                            RichText::new("逾期")
                                .size(10.0)
                                .strong()
                                .color(theme::DANGER),
                        );
                    }
                    ui.label(
                        RichText::new(model::fmt_time(r.time))
                            .size(11.0)
                            .color(theme::TEXT_WEAK),
                    );
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        if !r.tag.trim().is_empty() {
                            tag_badge_light(ui, r.tag.trim());
                        }
                    });
                });
            });

            // ── 第 3 行：完成时间 + 编辑 / 删除 ──
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                // 左侧显示完成时间（已完成才有）。
                if let Some(done_at) = r.done_at {
                    ui.label(
                        RichText::new(format!("✓ {}", done_at.format("%m-%d %H:%M")))
                            .size(10.5)
                            .color(theme::OK),
                    )
                    .on_hover_text(format!(
                        "完成于 {}",
                        done_at.format("%Y年%m月%d日 %H:%M")
                    ));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let del = ui.add(button_mini("删除"));
                    hit_rects.push(del.rect);
                    if del.clicked() {
                        *to_cancel = Some(r.id);
                    }
                    let edit = ui.add(button_mini("编辑"));
                    hit_rects.push(edit.rect);
                    if edit.clicked() {
                        *to_edit = Some(r.id);
                    }
                });
            });
        });

    let rect = inner.response.rect;

    // 双击卡片空白处 → 打开编辑框。
    //
    // 关键：这里**不能**用 `ui.interact(rect, ...)` 再注册一层交互区。
    // egui 里后注册的交互区优先拿到指针事件，会把上面「编辑/删除/勾选」
    // 的点击全部抢走，表现就是按钮点不动。
    // 改为纯位置判断：只有当双击落点不在任何内部控件上时才触发。
    //
    // `interact_pos()` 是全局的「当前指针位置」，所以同一帧里所有卡片都会拿
    // 它比一遍 `rect.contains()`。正常情况下卡片互不重叠，只有一张命中；
    // 但配合下面的 `allow_open` 再兜一层，确保一次双击最多只开一个窗。
    let dbl_on_card = ui.input(|i| {
        i.pointer.button_double_clicked(egui::PointerButton::Primary)
            && i.pointer
                .interact_pos()
                .is_some_and(|p| rect.contains(p) && !hit_rects.iter().any(|h| h.contains(p)))
    });
    // `allow_open` 由调用方根据「当前有没有别的对话框开着」决定：
    // 已经有弹窗时不再响应双击，避免连点堆出多个窗口。
    if dbl_on_card && allow_open {
        *to_edit = Some(r.id);
    }

    // 左侧色条：贴着卡片内缘画一条圆角竖条，作为最直观的分类标记。
    if let Some(color) = kind.stripe() {
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 1.0, rect.top() + 9.0),
            egui::vec2(3.0, (rect.height() - 18.0).max(6.0)),
        );
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), color);
    }


    // 状态说明挂在整张卡片上，移开鼠标前的悬停提示即可看到原来的徽标文字。
    let _ = kind.hint();

    ui.add_space(6.0);
}
fn ui_template_workspace(app: &mut PlanApp, root: &mut egui::Ui) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(26, 22)),
        )
        .show_inside(root, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("提醒模板")
                            .size(24.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new("模板只决定哪些日期出现记录；具体日期可单独修改")
                            .size(12.0)
                            .color(theme::TEXT_WEAK),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_ghost("← 返回日程")).clicked() {
                        app.page = WorkspacePage::Calendar;
                    }
                    ui.add_space(4.0);
                    if ui.add(button_primary("＋ 新建模板")).clicked() {
                        app.open_edit_state(EditState::new_template());
                    }
                });
            });
            ui.add_space(14.0);
            ui_status_line(app, ui);
            ui.add_space(10.0);
            if app.doc.templates.is_empty() {
                egui::Frame::NONE
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::symmetric(22, 28))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("还没有提醒模板")
                                    .size(16.0)
                                    .strong()
                                    .color(theme::TEXT),
                            );
                            ui.label(
                                RichText::new(
                                    "例如每周一 09:00 开周会，系统会为每个周一生成一条独立记录",
                                )
                                .size(12.0)
                                .color(theme::TEXT_WEAK),
                            );
                        });
                    });
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut to_toggle = None;
                    let mut to_edit = None;
                    let mut to_delete = None;
                    for template in app.doc.templates.clone() {
                        template_card(ui, &template, &mut to_toggle, &mut to_edit, &mut to_delete);
                    }
                    if let Some((id, on)) = to_toggle {
                        app.toggle_template(id, on);
                    }
                    if let Some(id) = to_edit {
                        if let Some(t) = app.doc.template(id) {
                            app.open_edit_state(EditState::from_template(t));
                        }
                    }
                    if let Some(id) = to_delete {
                        app.ask_delete_template(id);
                    }
                });
        });
}

fn template_card(
    ui: &mut egui::Ui,
    t: &Plan,
    to_toggle: &mut Option<(Uuid, bool)>,
    to_edit: &mut Option<Uuid>,
    to_delete: &mut Option<Uuid>,
) {
    egui::Frame::NONE
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if !t.tag.trim().is_empty() {
                    tag_badge_light(ui, t.tag.trim());
                }
                ui.label(
                    RichText::new(&t.title)
                        .size(15.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new({
                        let hint = t.remind.hint_short();
                        if hint.is_empty() {
                            format!("{}  {}", t.repeat.label(), model::fmt_time(t.time))
                        } else {
                            format!("{}  {}  {}", t.repeat.label(), hint, model::fmt_time(t.time))
                        }
                    })
                    .size(12.0)
                    .color(theme::TEXT_WEAK),
                );
                if !t.enabled {
                    badge(ui, "已停用", theme::TEXT_WEAK, theme::CARD_HOVER);
                } else if let Some(next) = t.next_occurrence(&Local::now()) {
                    badge(
                        ui,
                        &format!("下次 {}", next.format("%m-%d %H:%M")),
                        theme::TEXT_WEAK,
                        theme::CARD_HOVER,
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("删除")).clicked() {
                        *to_delete = Some(t.id);
                    }
                    if ui.add(button_mini("编辑")).clicked() {
                        *to_edit = Some(t.id);
                    }
                    if switch(ui, t.enabled, if t.enabled { "停用" } else { "启用" }) {
                        *to_toggle = Some((t.id, !t.enabled));
                    }
                });
            });
            if !t.note.trim().is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&t.note).size(12.0).color(theme::TEXT_WEAK));
            }
        });
    ui.add_space(7.0);
}

fn open_settings(app: &mut PlanApp) {
    app.open_settings();
    app.cfg_kind = app.settings.kind.clone();
    app.cfg_git = app.settings.git.clone();
    app.cfg_error = None;
    app.cfg_msg = None;
}

fn nav_item(ui: &mut egui::Ui, icon: &str, text: &str, active: bool) -> egui::Response {
    let fill = if active {
        theme::ACCENT
    } else {
        Color32::TRANSPARENT
    };
    let fg = if active {
        Color32::WHITE
    } else {
        Color32::from_rgb(170, 181, 205)
    };
    let resp = egui::Frame::NONE
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(11, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(15.0).color(fg));
                ui.add_space(5.0);
                ui.label(RichText::new(text).size(13.0).strong().color(fg));
            });
        })
        .response;
    let resp = if active {
        resp
    } else {
        let resp = resp.interact(egui::Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(
                resp.rect,
                egui::CornerRadius::same(9),
                Color32::from_white_alpha(12),
            );
        }
        resp
    };
    ui.add_space(3.0);
    resp
}

/// 侧栏品牌标：与程序图标一致——白底日历 + 金色铃铛。
fn brand_mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(11), theme::ACCENT);

    // 日历卡
    let cal = egui::Rect::from_min_max(
        rect.min + egui::vec2(size * 0.15, size * 0.22),
        rect.min + egui::vec2(size * 0.72, size * 0.78),
    );
    painter.rect_filled(cal, egui::CornerRadius::same(3), Color32::WHITE);
    let head = egui::Rect::from_min_max(
        cal.min,
        egui::pos2(cal.right(), cal.top() + cal.height() * 0.30),
    );
    painter.rect_filled(
        head,
        egui::CornerRadius {
            nw: 3,
            ne: 3,
            sw: 0,
            se: 0,
        },
        Color32::from_rgb(56, 64, 170),
    );

    // 日期点阵（三分之二处留白给铃铛）
    let dot = size * 0.062;
    for (cx, cy) in [(0.26, 0.52), (0.43, 0.52), (0.26, 0.68), (0.43, 0.68)] {
        painter.rect_filled(
            egui::Rect::from_center_size(
                rect.min + egui::vec2(size * cx, size * cy),
                egui::vec2(dot, dot),
            ),
            egui::CornerRadius::same(1),
            Color32::from_rgb(150, 158, 220),
        );
    }

    // 铃铛：底衬 + 铃身 + 铃舌
    let bell = Color32::from_rgb(255, 205, 92);
    let center = rect.min + egui::vec2(size * 0.72, size * 0.70);
    painter.circle_filled(center, size * 0.235, theme::ACCENT);
    painter.circle_filled(
        egui::pos2(center.x, center.y - size * 0.045),
        size * 0.135,
        bell,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(center.x - size * 0.135, center.y - size * 0.045),
            egui::pos2(center.x + size * 0.135, center.y + size * 0.055),
        ),
        egui::CornerRadius::ZERO,
        bell,
    );
    painter.circle_filled(
        egui::pos2(center.x, center.y + size * 0.055),
        size * 0.052,
        Color32::from_rgb(222, 162, 36),
    );
}
fn metric_card(ui: &mut egui::Ui, width: f32, label: &str, value: &str, accent: Color32) {
    egui::Frame::NONE
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            // 由调用方按可用宽度均分，窄窗口下不会把第三张卡挤出可视区。
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, accent);
                ui.label(RichText::new(label).size(11.0).color(theme::TEXT_WEAK));
            });
            ui.add_space(2.0);
            ui.label(RichText::new(value).size(21.0).strong().color(theme::TEXT));
        });
}

/// 底部状态行：存储位置 + 最近一次同步结果（几秒后自动淡出）。
fn ui_status_line(app: &mut PlanApp, ui: &mut egui::Ui) {
    // 「立即同步」探测到远端有本地没有的新提交 → 显示确认条，而不是悄悄覆盖。
    let backend = build_backend(&app.settings);
    let mut text = backend.describe();
    let mut color = theme::TEXT_WEAK;
    if let Some((msg, ok, at)) = &app.status {
        if at.elapsed() < Duration::from_secs(12) {
            text = format!("{msg} · {text}");
            color = if *ok { theme::OK } else { theme::DANGER };
        }
    }
    ui.horizontal(|ui| {
        if app.syncing {
            ui.spinner();
        }
        ui.add(egui::Label::new(RichText::new(text).size(11.0).color(color)).truncate());
    });
}

/// 防抖窗口里距同步还有多久（用于侧栏「改动已暂存，X后同步」）。
fn remaining_str(app: &PlanApp) -> String {
    match &app.push_queue {
        Some(entry) => {
            let left = PUSH_DELAY.saturating_sub(entry.queued_at.elapsed());
            format!("{} 秒", left.as_secs().max(1))
        }
        None => "—".to_string(),
    }
}

fn weekday_cn(d: NaiveDate) -> &'static str {
    match d.weekday().num_days_from_monday() {
        0 => "一",
        1 => "二",
        2 => "三",
        3 => "四",
        4 => "五",
        5 => "六",
        _ => "日",
    }
}

/// 今日进度卡片。
fn ui_record_view_dialog(app: &mut PlanApp, ctx: &egui::Context) {
    let Some(id) = app.view_record else {
        return;
    };
    let Some(r) = app.doc.record(id).cloned() else {
        app.view_record = None;
        return;
    };
    let mut open = true;
    let mut close = false;
    let mut edit = false;
    egui::Window::new("查看记录")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .title_bar(false)
        .open(&mut open)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(420.0);
            // 顶栏只留弱化的“记录详情”+ 关闭按钮，标题区让给提醒标题本身。
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("记录详情")
                        .size(11.5)
                        .strong()
                        .color(theme::TEXT_WEAK),
                );
                if !r.tag.trim().is_empty() {
                    ui.add_space(4.0);
                    tag_badge_light(ui, r.tag.trim());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(&r.title)
                    .size(19.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);
            egui::Grid::new("record_view_grid")
                .num_columns(2)
                .spacing([14.0, 9.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("日期").color(theme::TEXT_WEAK));
                    ui.label(r.date.format("%Y年%m月%d日").to_string());
                    ui.end_row();
                    ui.label(RichText::new("时间").color(theme::TEXT_WEAK));
                    ui.label(model::fmt_time(r.time));
                    ui.end_row();
                    if !r.remind.hint_short().is_empty() {
                        ui.label(RichText::new("提醒").color(theme::TEXT_WEAK));
                        ui.label(r.remind.describe());
                        ui.end_row();
                    }
                    ui.label(RichText::new("状态").color(theme::TEXT_WEAK));
                    if r.done {
                        let when = r
                            .done_at
                            .map(|d| format!("  完成于 {}", d.format("%Y年%m月%d日 %H:%M")))
                            .unwrap_or_default();
                        ui.label(format!("已完成{when}"));
                    } else if r.is_overdue(&Local::now()) {
                        ui.label(RichText::new("已逾期").color(theme::DANGER));
                    } else {
                        ui.label("未完成");
                    }
                    ui.end_row();
                    ui.label(RichText::new("来源").color(theme::TEXT_WEAK));
                    ui.label(if r.template_id.is_some() {
                        "模板生成"
                    } else {
                        "单独记录"
                    });
                    ui.end_row();
                    if !r.tag.trim().is_empty() {
                        ui.label(RichText::new("标签").color(theme::TEXT_WEAK));
                        ui.label(r.tag.trim());
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            ui.label(
                RichText::new("备注")
                    .size(11.5)
                    .strong()
                    .color(theme::TEXT_WEAK),
            );
            egui::Frame::NONE
                .fill(theme::CARD_HOVER)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(if r.note.trim().is_empty() {
                        "没有备注"
                    } else {
                        &r.note
                    });
                });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.add(button_primary("编辑")).clicked() {
                    edit = true;
                }
                if ui.add(button_ghost("关闭")).clicked() {
                    close = true;
                }
            });
        });
    if edit {
        app.view_record = None;
        app.open_edit_record(r.id);
    } else if close || !open {
        app.view_record = None;
    }
}
// ============================== 删除模板确认 ==============================

fn ui_delete_template_dialog(app: &mut PlanApp, ctx: &egui::Context) {
    let Some(prompt) = app.delete_prompt.as_mut() else {
        return;
    };
    let mut open = true;
    let mut confirm = false;
    let mut cancel = false;

    egui::Window::new("删除提醒模板")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .title_bar(false)
        .open(&mut open)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("删除提醒模板")
                        .size(18.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        cancel = true;
                    }
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(10.0);

            ui.label(
                RichText::new(&prompt.title)
                    .size(15.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("模板本身一定会删除，之后不再为新日期生成记录。")
                    .size(12.0)
                    .color(theme::TEXT_WEAK),
            );
            ui.add_space(10.0);

            if prompt.record_count == 0 {
                ui.label(
                    RichText::new("这个模板目前没有生成任何日期记录。")
                        .size(12.0)
                        .color(theme::TEXT_WEAK),
                );
            } else {
                let checked = prompt.record_count;
                ui.checkbox(
                    &mut prompt.remove_records,
                    format!("同时删除该模板生成的 {checked} 条日期记录"),
                )
                .on_hover_text("勾选后这些日期上的提醒会被清空；不勾选则保留为独立记录，可继续查看和勾完成。");
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if prompt.remove_records {
                        "这些日期上将不再显示任何记录。"
                    } else {
                        "记录会保留下来，但不再属于任何模板（显示为「单独记录」）。"
                    })
                    .size(11.5)
                    .color(if prompt.remove_records {
                        theme::DANGER
                    } else {
                        theme::TEXT_WEAK
                    }),
                );
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let label = if prompt.remove_records && prompt.record_count > 0 {
                    "删除模板和记录"
                } else {
                    "删除模板"
                };
                let btn = if prompt.remove_records && prompt.record_count > 0 {
                    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                        .fill(theme::DANGER)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(egui::CornerRadius::same(9))
                        .min_size(egui::vec2(0.0, 34.0))
                } else {
                    button_primary(label)
                };
                if ui.add(btn).clicked() {
                    confirm = true;
                }
                if ui.add(button_ghost("取消")).clicked() {
                    cancel = true;
                }
            });
        });

    if confirm {
        let (id, remove) = (prompt.template_id, prompt.remove_records);
        app.delete_prompt = None;
        app.delete_template(id, remove);
    } else if cancel || !open {
        app.delete_prompt = None;
    }
}

/// 删除单条记录前的确认框。
///
/// 删除会把记录打成墓碑（`cancelled = true`），界面上就再也看不到了，
/// 所以必须先让用户核对标题 + 日期时间，避免误点。
fn ui_record_delete_dialog(app: &mut PlanApp, ctx: &egui::Context) {
    let Some(prompt) = app.record_delete_prompt.as_ref() else {
        return;
    };
    let mut open = true;
    let mut confirm = false;
    let mut cancel = false;
    let title = prompt.title.clone();
    let record_id = prompt.record_id;
    let when = format!(
        "{}  周{}  {}",
        prompt.date.format("%Y年%m月%d日"),
        weekday_cn(prompt.date),
        model::fmt_time(prompt.time)
    );

    egui::Window::new("确认删除记录")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .title_bar(false)
        .open(&mut open)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(400.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("确认删除")
                        .size(18.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        cancel = true;
                    }
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(10.0);

            // 用卡片样式把「要删的是哪一条」摆出来，避免删错。
            egui::Frame::NONE
                .fill(theme::CARD_HOVER)
                .corner_radius(egui::CornerRadius::same(9))
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(&title)
                            .size(15.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        RichText::new(when)
                            .size(11.5)
                            .color(theme::TEXT_WEAK),
                    );
                });

            ui.add_space(10.0);
            ui.label(
                RichText::new("删除后这条记录就不再显示，也无法撤销。")
                    .size(12.0)
                    .color(theme::DANGER),
            );

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let del = egui::Button::new(
                    RichText::new("确认删除").strong().color(Color32::WHITE),
                )
                .fill(theme::DANGER)
                .stroke(egui::Stroke::NONE)
                .corner_radius(egui::CornerRadius::same(9))
                .min_size(egui::vec2(0.0, 34.0));
                if ui.add(del).clicked() {
                    confirm = true;
                }
                if ui.add(button_ghost("取消")).clicked() {
                    cancel = true;
                }
            });
        });

    if confirm {
        app.record_delete_prompt = None;
        app.cancel_record(record_id);
    } else if cancel || !open {
        app.record_delete_prompt = None;
    }
}

// ============================== 标签下拉（可选可输） ==============================

/// 一个既能下拉选择、又能直接输入新建的标签组件。
///
/// 不能用标准 `ComboBox`：它的弹层在焦点移走时会自动关闭，里面的输入框根本没法用。
/// 这里用 `Area` 自绘浮层，并通过 `tag_open` 显式控制开关，点浮层外部才收起。
///
/// 返回 `Some(标签)` 表示用户新建了一个字典里还没有的标签，调用方负责写入字典。
fn tag_combo(
    ui: &mut egui::Ui,
    tag: &mut String,
    open: &mut bool,
    known: &[String],
    drop_tag: &mut Option<String>,
) -> Option<String> {
    let mut created: Option<String> = None;
    // 记录进入本函数时的展开状态，用来区分“刚点开”和“本来开着”。
    let was_open = *open;
    const WIDTH: f32 = 240.0;

    // 整个组件画在一个 Frame 里，视觉上是一个控件：左边可打字，右边小箭头展开候选。
    let mut text_resp = None;
    let mut arrow_resp = None;
    let input_id = ui.id().with("tag_combo_input");
    egui::Frame::NONE
        .fill(theme::CARD_HOVER)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let text = ui.add(
                    egui::TextEdit::singleline(tag)
                        .id(input_id)
                        .hint_text("选择或输入标签")
                        .desired_width(WIDTH - 46.0)
                        // 外层 Frame 已经画了边框，这里用透明 Frame 把输入框自身的外框去掉。
                        .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(4, 4))),
                );
                let arrow = ui.add(
                    egui::Button::new(
                        RichText::new(if *open { "▴" } else { "▾" })
                            .size(11.0)
                            .color(theme::TEXT_WEAK),
                    )
                    .fill(Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(6))
                    .min_size(egui::vec2(20.0, 20.0)),
                );
                text_resp = Some(text);
                arrow_resp = Some(arrow);
            });
        });
    let text_resp = text_resp.expect("TextEdit 一定已创建");
    let arrow_resp = arrow_resp.expect("箭头按钮一定已创建");
    let rect = text_resp.rect.union(arrow_resp.rect);
    let anchor_probe = rect.left_bottom() + egui::vec2(0.0, 2.0);

    // 点进输入框即展开候选。
    if text_resp.clicked() || text_resp.gained_focus() {
        *open = true;
    }
    if arrow_resp.clicked() {
        *open = !*open;
        if *open {
            ui.memory_mut(|m| m.request_focus(input_id));
        }
    }
    // 输入法组字时按 Esc 是「取消候选」，不能顺手把下拉也关了。
    let ime_composing = ui.input(|i| {
        i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Ime(egui::ImeEvent::Preedit { text: t, .. }) if !t.is_empty()
            )
        })
    });
    if !ime_composing && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        *open = false;
    }

    // 回车：字典里没有就新建，已有就选中。
    //
    // 关键：拼音候选没选定时按的回车是「上屏」，不能当成提交。
    // egui 0.34 已在 winit 层把这类回车过滤掉（VK_PROCESSKEY），
    // 这里再兜一层：本帧还在输入法组字状态就完全不响应回车。
    let submitted =
        !ime_composing && text_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if submitted {
        let typed = tag.trim().to_string();
        if !typed.is_empty() && !known.iter().any(|t| t == &typed) {
            created = Some(typed.clone());
            tag.clone_from(&typed);
        }
        *open = false;
    }

    // ---- 浮层（候选列表） ----
    if *open {
        let anchor = anchor_probe;
        let typed = tag.trim().to_lowercase();
        let matches: Vec<&String> = known
            .iter()
            .filter(|t| typed.is_empty() || t.to_lowercase().contains(&typed))
            .collect();

        let area = egui::Area::new(egui::Id::new("tag_combo_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(6, 6))
                    .show(ui, |ui| {
                        ui.set_width(WIDTH - 8.0);
                        ui.spacing_mut().item_spacing.y = 2.0;

                        // 输入了字典里没有的词 → 顶部给出“新建”入口
                        let can_create = !typed.is_empty()
                            && !known.iter().any(|t| t.to_lowercase() == typed);
                        if can_create {
                            let label = format!("＋ 新建标签「{}」", tag.trim());
                            if ui
                                .add_sized(
                                    [ui.available_width(), 26.0],
                                    egui::Button::new(
                                        RichText::new(label)
                                            .size(12.0)
                                            .strong()
                                            .color(theme::ACCENT),
                                    )
                                    .fill(theme::ACCENT_SOFT)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(egui::CornerRadius::same(7)),
                                )
                                .clicked()
                            {
                                let fresh = tag.trim().to_string();
                                created = Some(fresh.clone());
                                tag.clone_from(&fresh);
                                *open = false;
                            }
                        }

                        if matches.is_empty() && !can_create {
                            ui.label(
                                RichText::new("还没有标签，直接输入即可新建")
                                    .size(11.0)
                                    .color(theme::TEXT_WEAK),
                            );
                        }

                        let mut picked: Option<String> = None;
                        for name in &matches {
                            let selected = tag.trim() == name.as_str();
                            if ui
                                .add_sized(
                                    [ui.available_width(), 24.0],
                                    egui::Button::new(
                                        RichText::new(name.as_str()).size(12.0).color(
                                            if selected {
                                                theme::ACCENT
                                            } else {
                                                theme::TEXT
                                            },
                                        ),
                                    )
                                    .fill(if selected {
                                        theme::ACCENT_SOFT
                                    } else {
                                        Color32::TRANSPARENT
                                    })
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(egui::CornerRadius::same(6)),
                                )
                                .clicked()
                            {
                                picked = Some((*name).clone());
                            }
                        }
                        if let Some(name) = picked {
                            *tag = name;
                            *open = false;
                        }

                        if matches.len() > 1 || (!matches.is_empty() && can_create) {
                            ui.separator();
                        }
                        ui.horizontal(|ui| {
                            if !tag.trim().is_empty() && ui.add(button_mini("清空")).clicked() {
                                tag.clear();
                                *open = false;
                            }
                            let removable = !tag.trim().is_empty()
                                && known.iter().any(|t| t == tag.trim());
                            if removable && ui.add(button_mini("从字典删除")).clicked() {
                                *drop_tag = Some(tag.trim().to_string());
                            }
                            ui.label(
                                RichText::new("回车新建")
                                    .size(10.0)
                                    .color(theme::TEXT_WEAK),
                            );
                        });
                    });
            });

        // 点在浮层和输入框之外 → 收起。
        // 注意：本条只在“这一帧之前就已经展开”时生效，否则刚点开就会被自己关掉。
        if was_open && ui.input(|i| i.pointer.any_click()) {
            let pointer = ui.input(|i| i.pointer.interact_pos());
            if let Some(p) = pointer {
                let inside_popup = area.response.rect.contains(p);
                let inside_input = rect.contains(p);
                if !inside_popup && !inside_input {
                    *open = false;
                }
            }
        }
    }

    created
}

// ============================== 编辑对话框 ==============================

fn ui_edit_dialog(app: &mut PlanApp, ctx: &egui::Context) {
    if app.edit.is_none() {
        return;
    }
    let mut open = true;
    let mut save = false;
    let mut cancel = false;
    // 先取出候选标签，避免后面同时借用 app.edit 与 app.doc。
    let used_tags = app.doc.all_tags();
    let mut new_tag: Option<String> = None;
    let mut dropped_tag: Option<String> = None;
    let state = app.edit.as_mut().unwrap();

    egui::Window::new("计划与提醒")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .title_bar(false)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(18)),
        )
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_width(470.0);
            // 内容（模板 + 提醒窗口展开）可能很长，小窗口下会把标题顶出屏幕。
            // 做法：标题和底部按钮固定，只有中间表单滚动；滚动区高度按窗口剩余空间算。
            let footer_h = 52.0;
            let scroll_h = (ctx.content_rect().height() - 220.0 - footer_h).clamp(180.0, 460.0);
            ui.horizontal(|ui| {
                // 标题只保留「新建 / 编辑」两个字，后面不再挂解释性小字。
                ui.label(
                    RichText::new(if state.target.is_some() {
                        "编辑"
                    } else {
                        "新建"
                    })
                    .size(18.0)
                    .strong()
                    .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        cancel = true;
                    }
                });
            });
            ui.add_space(2.0);
            ui.separator();
            ui.add_space(6.0);

            // 表单本体放进滚动区：内容再长也只滚动中间这段，
            // 标题栏和底部按钮始终留在可视区。
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(scroll_h)
                .show(ui, |ui| {
            if state.is_template() {
                ui.label(
                    RichText::new(
                        "模板会按规则生成独立日期记录；修改模板不会覆盖已经单独改过的日期",
                    )
                    .size(11.5)
                    .color(theme::TEXT_WEAK),
                );
                ui.add_space(6.0);
            }
            edit_field_label(ui, "标题");
            text_input_sized(
                ui,
                &mut state.title,
                f32::INFINITY,
                "例如：周报、交房租、年度体检",
            );
            ui.add_space(8.0);

            // 标签：同输入框既下拉选择，也可直接打字新建（回车确认）。
            edit_field_label(ui, "标签");
            if let Some(created) = tag_combo(
                ui,
                &mut state.tag,
                &mut state.tag_open,
                &used_tags,
                &mut dropped_tag,
            ) {
                new_tag = Some(created);
            }
            if state.tag.trim().is_empty() {
                ui.label(
                    RichText::new("不选标签也能保存，提醒窗会显示「提醒」")
                        .size(10.5)
                        .color(theme::TEXT_WEAK),
                );
            }
            ui.add_space(8.0);

            // 日期 + 时间并成一行两列：两者都是「什么时候响」，放一起更直观，
            // 也省掉一整行高度。
            let time_label = "计划时间";
            let time_hint: Option<&str> = None;
            if !state.is_template() {
                // 记录：左列日期，右列时间。
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        edit_field_label(ui, "日期");
                        date_picker(
                            ui,
                            &mut state.date,
                            &mut state.date_open,
                            &mut state.cal_month,
                        );
                    });
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        edit_field_label(ui, time_label);
                        time_picker(
                            ui,
                            &mut state.hour,
                            &mut state.minute,
                            &mut state.time_open,
                        );
                    });
                });
            } else {
                // 模板没有固定日期，只显示时间。
                edit_field_label(ui, time_label);
                time_picker(
                    ui,
                    &mut state.hour,
                    &mut state.minute,
                    &mut state.time_open,
                );
            }
            if let Some(hint) = time_hint {
                ui.label(RichText::new(hint).size(10.5).color(theme::TEXT_WEAK));
            }
            ui.add_space(8.0);

            // 提醒窗口：提前 / 延后 / 间隔 三个正交字段。
            edit_field_label(ui, "提醒窗口");
            egui::Frame::NONE
                .fill(theme::CARD_HOVER)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());

                    // 提前：天数 + 忽略（0 天）两态。
                    //   忽略（0 天）= 不提前，但**从计划当天 0 点**就开始判断
                    //   N 天        = 从「计划日往前 N 天」的 0 点开始
                    // 统一规则：起点永远是那一天的 00:00，不取计划时分。
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("提前").size(12.0).color(theme::TEXT_WEAK));
                        if ui
                            .add(grace_chip("忽略", state.adv_days == 0))
                            .on_hover_text("不提前，从计划当天 0 点开始提醒")
                            .clicked()
                        {
                            state.adv_days = 0;
                        }
                        let mut days = state.adv_days;
                        if ui
                            .add(
                                egui::DragValue::new(&mut days)
                                    .range(0..=365)
                                    .speed(0.2)
                                    .suffix(" 天"),
                            )
                            .changed()
                        {
                            state.adv_days = days;
                        }
                        ui.label(
                            RichText::new(if days == 0 {
                                "当天 0 点开始"
                            } else {
                                "天前的 0 点开始"
                            })
                            .size(12.0)
                            .color(theme::TEXT_WEAK),
                        );
                    });
                    ui.add_space(4.0);

                    // 延后：三种状态
                    //   不限（None）  = 一直催到完成
                    //   忽略（Some(0)）= 计划时间一过就停，不再提醒
                    //   自定义 N 天   = 计划时间过后再催 N 天
                    // 「忽略」单独做成按钮，省得手输 0。
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("延后").size(12.0).color(theme::TEXT_WEAK),
                        );
                        let unlimited = state.grace_days.is_none();
                        let ignored = state.grace_days == Some(0);
                        if ui
                            .add(grace_chip("不限", unlimited))
                            .on_hover_text("一直提醒到标记完成")
                            .clicked()
                        {
                            state.grace_days = None;
                        }
                        if ui
                            .add(grace_chip("忽略", ignored))
                            .on_hover_text("计划时间过后就停止提醒")
                            .clicked()
                        {
                            state.grace_days = Some(0);
                        }

                        match state.grace_days {
                            None => {
                                ui.label(
                                    RichText::new("一直提醒到标记完成")
                                        .size(12.0)
                                        .color(theme::TEXT_WEAK),
                                );
                            }
                            Some(g) => {
                                let mut days = g;
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut days)
                                            .range(0..=365)
                                            .speed(0.2)
                                            .suffix(" 天"),
                                    )
                                    .changed()
                                {
                                    state.grace_days = Some(days);
                                }
                                ui.label(
                                    RichText::new(if days == 0 {
                                        "（到点即停）"
                                    } else {
                                        "后停止提醒"
                                    })
                                    .size(12.0)
                                    .color(theme::TEXT_WEAK),
                                );
                            }
                        }
                    });
                    ui.add_space(4.0);

                    // 间隔
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("每").size(12.0).color(theme::TEXT_WEAK),
                        );
                        ui.add(
                            egui::DragValue::new(&mut state.every_minutes)
                                .range(1..=1440)
                                .speed(1)
                                .suffix(" 分钟"),
                        );
                        ui.label(
                            RichText::new("提醒一次")
                                .size(12.0)
                                .color(theme::TEXT_WEAK),
                        );
                    });
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("常用：").size(10.5).color(theme::TEXT_WEAK),
                        );
                        for m in [10u32, 15, 30, 60, 120, 1440] {
                            if ui.add(button_mini(&model::fmt_minutes(m))).clicked() {
                                state.every_minutes = m;
                            }
                        }
                    });

                    // 窗口总次数提示：提前天数 × 间隔会产生大量提醒，明确告知。
                    let total = window_fire_count(&state);
                    if total > 48 {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!(
                                "窗口内约 {total} 次提醒，数量较多，建议缩短提前/延后天数或加大间隔"
                            ))
                            .size(10.5)
                            .color(theme::WARN),
                        );
                    } else {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("窗口内约 {total} 次提醒"))
                                .size(10.5)
                                .color(theme::TEXT_WEAK),
                        );
                    }
                });
            ui.add_space(8.0);
            ui.add_space(8.0);

            if state.is_template() {
                edit_field_label(ui, "生成规则");
                egui::Frame::NONE
                    .fill(theme::CARD_HOVER)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ComboBox::from_id_salt("repeat_kind")
                            .selected_text(repeat_kind_label(state.repeat_kind))
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.repeat_kind,
                                    RepeatKind::Daily,
                                    "每天",
                                );
                                ui.selectable_value(
                                    &mut state.repeat_kind,
                                    RepeatKind::Weekly,
                                    "每周",
                                );
                                ui.selectable_value(
                                    &mut state.repeat_kind,
                                    RepeatKind::Monthly,
                                    "每月",
                                );
                                ui.selectable_value(
                                    &mut state.repeat_kind,
                                    RepeatKind::Yearly,
                                    "每年",
                                );
                            });
                        ui.add_space(6.0);
                        match state.repeat_kind {
                            RepeatKind::Daily => {
                                ui.label(
                                    RichText::new("每天生成一条日期记录")
                                        .size(12.5)
                                        .color(theme::TEXT_WEAK),
                                );
                            }
                            RepeatKind::Weekly => {
                                ui.horizontal(|ui| {
                                    for (idx, name) in ["一", "二", "三", "四", "五", "六", "日"]
                                        .iter()
                                        .enumerate()
                                    {
                                        let selected = state.weekdays[idx];
                                        let button = egui::Button::new(
                                            RichText::new(*name).size(12.5).strong().color(
                                                if selected {
                                                    Color32::WHITE
                                                } else {
                                                    theme::TEXT_WEAK
                                                },
                                            ),
                                        )
                                        .fill(if selected { theme::ACCENT } else { theme::CARD })
                                        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                                        .corner_radius(egui::CornerRadius::same(7))
                                        .min_size(egui::vec2(38.0, 28.0));
                                        if ui.add(button).clicked() {
                                            state.weekdays[idx] = !state.weekdays[idx];
                                        }
                                    }
                                });
                            }
                            RepeatKind::Monthly => {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("每月").size(12.5).color(theme::TEXT_WEAK),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut state.month_day)
                                            .range(1..=31)
                                            .speed(1)
                                            .suffix(" 日"),
                                    );
                                    ui.label(
                                        RichText::new("小月自动按月底")
                                            .size(11.5)
                                            .color(theme::TEXT_WEAK),
                                    );
                                });
                            }
                            RepeatKind::Yearly => {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("每年").size(12.5).color(theme::TEXT_WEAK),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut state.year_month)
                                            .range(1..=12)
                                            .speed(1)
                                            .suffix(" 月"),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut state.year_day)
                                            .range(1..=31)
                                            .speed(1)
                                            .suffix(" 日"),
                                    );
                                    ui.label(
                                        RichText::new("2 月 29 日平年按 28 日")
                                            .size(11.5)
                                            .color(theme::TEXT_WEAK),
                                    );
                                });
                            }
                        }
                    });
                ui.add_space(8.0);
            }

            edit_field_label(ui, "备注");
            let note_hint = if state.is_template() {
                "会复制到新生成的日期记录；已有记录可单独修改"
            } else {
                "只属于这一天的备注"
            };
            text_input_multiline(ui, &mut state.note, 3).on_hover_text(note_hint);

            if let Some(err) = &state.error {
                ui.add_space(4.0);
                ui.colored_label(theme::DANGER, err);
            }
                });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(button_primary("保存")).clicked() {
                    save = true;
                }
                ui.add_space(4.0);
                if ui.add(button_ghost("取消")).clicked() {
                    cancel = true;
                }
            });
        });

    // 字典变更先落地（不影响本次保存），再走保存流程。
    if let Some(tag) = new_tag {
        if app.doc.remember_tag(&tag) {
            app.save_and_sync("新增标签");
        }
    }
    if let Some(tag) = dropped_tag {
        app.doc.forget_tag(&tag);
        app.save_and_sync("删除标签");
    }

    if save {
        app.save_edit();
    } else if !open || cancel {
        app.edit = None;
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    /// 「延后」的三态：不限 / 忽略(0) / 自定义，都要能正确落到 RemindWindow。
    #[test]
    fn grace_states_map_to_remind_window() {
        let mut state = EditState::new_record(date(2026, 11, 5));

        // 新建默认「不限」
        assert!(state.grace_days.is_none());
        assert_eq!(state.remind().unwrap().grace_days, None);

        // 忽略 → Some(0)，窗口在计划时刻结束
        state.grace_days = Some(0);
        assert_eq!(state.remind().unwrap().grace_days, Some(0));
        let w = state.remind().unwrap();
        let occ = date(2026, 11, 5)
            .and_time(time(9, 0))
            .and_local_timezone(Local)
            .single()
            .unwrap();
        let (_, end) = w.window(occ).unwrap();
        assert_eq!(end, Some(occ), "忽略=窗口在计划时刻结束");

        // 自定义天数
        state.grace_days = Some(3);
        let w = state.remind().unwrap();
        let (_, end) = w.window(occ).unwrap();
        assert_eq!(end, Some(occ + ChronoDuration::days(3)));

        // 次数估算：忽略时至少 1 次（进窗口首响）
        state.grace_days = Some(0);
        state.adv_days = 0;
        assert_eq!(window_fire_count(&state), 1);
    }

    /// 弹窗定位：必须落在工作区**右下角**，且与缩放比无关。
    ///
    /// 回归场景：主窗口缓存了过期的 `pixels_per_point`（1.5），
    /// 而实际桌面是 96 DPI（1.0）。当时把 1920px 宽的工作区除以 1.5，
    /// 得到 1280 points，卡片 x 落在 842px —— 水平方向 44%，也就是
    /// 用户看到的「跑到屏幕中间」。
    #[test]
    fn toast_layout_sticks_to_bottom_right() {
        const CARD_W: f32 = 420.0;
        const MARGIN: f32 = 18.0;
        const GAP: f32 = 10.0;

        // 1920×1032 工作区（1080 屏减任务栏），三种真实缩放比。
        let work = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1032.0));
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let h = 128.0;
            let boxes = toast_layout_px(work, scale, CARD_W, MARGIN, GAP, &[h]);
            let p = boxes[0];

            let card_w_px = CARD_W * scale;
            let margin_px = MARGIN * scale;
            // 右边贴边：右边界正好是 屏宽 - 边距。
            assert!(
                (p.x + card_w_px - (work.max.x - margin_px)).abs() < 0.01,
                "scale={scale}: 右边未贴齐, x={} w={}",
                p.x,
                card_w_px
            );
            // 底边贴边。
            assert!(
                (p.y + h * scale - (work.max.y - margin_px)).abs() < 0.01,
                "scale={scale}: 底边未贴齐, y={}",
                p.y
            );
            // 关键回归点：不管缩放比多少，卡片都必须落在屏幕右半边，
            // 绝不能漂到中间去。
            assert!(
                p.x > work.max.x * 0.5,
                "scale={scale}: 卡片跑到屏幕中间了, x={} / {}",
                p.x,
                work.max.x
            );
        }
    }

    /// 多条提醒要向上堆叠，且不越出工作区上沿。
    #[test]
    fn toast_layout_stacks_upward_and_clamps() {
        const CARD_W: f32 = 420.0;
        const MARGIN: f32 = 18.0;
        const GAP: f32 = 10.0;
        let work = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1032.0));

        // 三条正常高度：依次向上，且互不重叠。
        let heights = [128.0_f32, 106.0, 106.0];
        let boxes = toast_layout_px(work, 1.0, CARD_W, MARGIN, GAP, &heights);
        for w in boxes.windows(2) {
            assert!(w[1].y < w[0].y, "后一条应在上面");
        }
        // 相邻两条之间正好隔 GAP。
        let gap_actual = boxes[0].y - (boxes[1].y + heights[1]);
        assert!((gap_actual - GAP).abs() < 0.01, "间距应为 {GAP}");

        // 超多卡片：不能跑到工作区上沿之外。
        let many: Vec<f32> = vec![200.0; 20];
        let boxes = toast_layout_px(work, 1.0, CARD_W, MARGIN, GAP, &many);
        for b in &boxes {
            assert!(
                b.y >= work.min.y + MARGIN - 0.01,
                "卡片顶出上沿: y={}",
                b.y
            );
        }
    }

    /// 造一条独立记录（测试用）。
    fn mk_record(d: NaiveDate, title: &str, t: NaiveTime) -> DayRecord {
        DayRecord {
            id: Uuid::new_v4(),
            template_id: None,
            date: d,
            title: title.into(),
            tag: "标签".into(),
            note: String::new(),
            time: t,
            remind: RemindWindow::default(),
            done: false,
            done_at: None,
            cancelled: false,
            customized: true,
            last_fired: None,
            snooze_until: None,
            created_at: Local::now(),
        }
    }

    /// 卡片双击必须保证「同一时刻只有一个对话框」。
    ///
    /// 回归场景：标题单击走 `resp.clicked()`，而 egui 的 `clicked()` 在双击时
    /// **也返回 true**，于是一次双击同时开了「查看记录」和「编辑」两个窗，
    /// 用户看到的就是「偶尔弹两个编辑框」。
    #[test]
    fn dialog_open_is_mutually_exclusive() {
        let mut app = base_app();
        // 造两条记录，分别当作「编辑」和「查看」的目标。
        let d = date(2026, 11, 2);
        app.doc
            .records
            .push(mk_record(d, "第一条", time(9, 0)));
        app.doc
            .records
            .push(mk_record(d, "第二条", time(10, 0)));
        let a = app.doc.records[0].id;
        let b = app.doc.records[1].id;

        // 先开「查看」，再开「编辑」：查看必须被关掉。
        app.open_record_view(a);
        assert!(app.view_record.is_some());
        app.open_edit_record(b);
        assert!(app.edit.is_some(), "编辑应打开");
        assert!(app.view_record.is_none(), "查看应被自动关闭，不能并存");

        // 反过来：先编辑，再查看，编辑必须被关掉。
        app.open_edit_record(a);
        assert!(app.edit.is_some());
        app.open_record_view(b);
        assert!(app.view_record.is_some());
        assert!(app.edit.is_none(), "编辑应被自动关闭，不能并存");

        // 删除确认框同样互斥。
        app.open_record_delete(RecordDeletePrompt {
            record_id: a,
            title: "第一条".into(),
            date: d,
            time: time(9, 0),
        });
        assert!(app.record_delete_prompt.is_some());
        assert!(app.view_record.is_none(), "查看应被删除框关掉");

        // 连点同一张卡片多次 → 依然只有一个编辑框（内容取最后一次）。
        app.open_edit_record(a);
        app.open_edit_record(a);
        app.open_edit_record(a);
        assert!(app.edit.is_some());
        // 再点另一张 → 换成另一张，不会堆出两个。
        app.open_edit_record(b);
        let st = app.edit.as_ref().unwrap();
        match st.target {
            Some(EditTarget::Record(id)) => assert_eq!(id, b, "应切换到最后点的那条"),
            _ => panic!("编辑目标应为记录"),
        }
    }

    fn base_app() -> PlanApp {
        let (sync_tx, sync_rx) = channel();
        PlanApp {
            settings: Settings::default(),
            doc: Doc::default(),
            page: WorkspacePage::Calendar,
            selected_date: date(2026, 11, 2),
            date_view: DateView::Day,
            view_record: None,
            edit: None,
            delete_prompt: None,
            record_delete_prompt: None,
            day_popup: None,
            day_popup_rect: None,
            show_settings: false,
            cfg_kind: Kind::Local,
            cfg_git: GitConfig::default(),
            cfg_error: None,
            cfg_msg: None,
            popups: Vec::new(),
            popup_heights: std::collections::HashMap::new(),
            last_tick: Instant::now(),
            frames_drawn: 0,
            status: None,
            syncing: false,
            push_queue: None,
            sync_tx,
            sync_rx,
            tray: None,
            tray_autostart_item: None,
            autolaunch: None,
            tray_handlers_ready: false,
            quitting: false,
        }
    }

    #[test]
    fn materialize_creates_independent_dated_records() {
        let mut app = base_app();
        let mut t = Plan::new("晨会".into(), "同步进度".into(), time(9, 0));
        t.repeat = RepeatRule::Weekly {
            weekdays: [true, false, false, false, false, false, false],
        };
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 16), "test", true);
        let dates: Vec<NaiveDate> = app
            .doc
            .records
            .iter()
            .filter(|r| !r.cancelled)
            .map(|r| r.date)
            .collect();
        assert_eq!(
            dates,
            vec![date(2026, 11, 2), date(2026, 11, 9), date(2026, 11, 16)]
        );
        let ids: std::collections::HashSet<_> = app.doc.records.iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), app.doc.records.len());
    }

    #[test]
    fn editing_one_day_does_not_change_other_days_or_template() {
        let mut app = base_app();
        let mut t = Plan::new("喝水".into(), String::new(), time(9, 0));
        t.repeat = RepeatRule::Daily;
        let template_id = t.id;
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 3), "test", true);
        let target = app.doc.records[0].id;
        let mut state = EditState::from_record(&app.doc.records[0]);
        state.title = "只改今天".into();
        state.note = "今天多喝一杯".into();
        state.hour = 10;
        state.target = Some(EditTarget::Record(target));
        app.edit = Some(state);
        app.save_edit();

        let a = app.doc.record(target).unwrap();
        let b = app.doc.records.iter().find(|r| r.id != target).unwrap();
        assert_eq!(a.title, "只改今天");
        assert!(a.customized);
        assert_eq!(b.title, "喝水");
        assert_eq!(app.doc.template(template_id).unwrap().title, "喝水");
    }

    #[test]
    fn standalone_record_does_not_create_template() {
        let mut app = base_app();
        let mut state = EditState::new_record(date(2026, 11, 5));
        state.title = "临时事项".into();
        state.note = "只在这一天".into();
        state.hour = 14;
        app.edit = Some(state);
        app.save_edit();
        assert!(app.doc.templates.is_empty());
        assert_eq!(app.doc.records.len(), 1);
        assert_eq!(app.doc.records[0].date, date(2026, 11, 5));
        assert_eq!(app.doc.records[0].title, "临时事项");
        assert!(app.doc.records[0].template_id.is_none());
    }

    #[test]
    fn deleted_generated_day_stays_deleted() {
        let mut app = base_app();
        let mut t = Plan::new("日报".into(), String::new(), time(18, 0));
        t.repeat = RepeatRule::Daily;
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 2), "test", true);
        let record_id = app.doc.records[0].id;
        app.cancel_record(record_id);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 2), "test", true);
        assert_eq!(app.doc.records.len(), 1);
        assert!(app.doc.record(record_id).unwrap().cancelled);
        assert!(app.doc.record_indices_for(date(2026, 11, 2)).is_empty());
    }

    #[test]
    fn template_sync_keeps_customized_future_record() {
        let mut app = base_app();
        let mut t = Plan::new("旧标题".into(), String::new(), time(9, 0));
        t.repeat = RepeatRule::Daily;
        let template_id = t.id;
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 3), "test", true);
        app.doc.records[0].customized = true;
        app.doc.records[0].title = "定制标题".into();
        app.doc.templates[0].title = "新标题".into();
        app.sync_template_future_records(template_id, date(2026, 11, 2));
        assert_eq!(app.doc.records[0].title, "定制标题");
        assert_eq!(app.doc.records[1].title, "新标题");
    }

    #[test]
    fn month_bounds_and_shift() {
        assert_eq!(first_of_month(date(2026, 11, 18)), date(2026, 11, 1));
        assert_eq!(last_of_month(date(2026, 11, 18)), date(2026, 11, 30));
        assert_eq!(last_of_month(date(2026, 2, 10)), date(2026, 2, 28));
        assert_eq!(last_of_month(date(2028, 2, 10)), date(2028, 2, 29));
        // 跨年平移
        assert_eq!(shift_month(date(2026, 12, 15), 1), date(2027, 1, 15));
        assert_eq!(shift_month(date(2026, 1, 15), -1), date(2025, 12, 15));
        // 31 日进入小月时收敛到当月末
        assert_eq!(shift_month(date(2026, 10, 31), 1), date(2026, 11, 30));
    }

    #[test]
    fn calendar_view_days_follow_month_grid() {
        let (start, end) = visible_range(DateView::Calendar, date(2026, 11, 18));
        assert_eq!(start, date(2026, 11, 1));
        assert_eq!(end, date(2026, 11, 30));
        // 月视图范围与日历一致
        assert_eq!(
            visible_range(DateView::Month, date(2026, 11, 18)),
            (start, end)
        );
        // 2026-11-01 是周日，周一开头的网格前面要留 6 个空位
        assert_eq!(start.weekday().num_days_from_monday(), 6);
    }

    /// 准备一个「每天一条」的模板，并铺出 11/02~11/04 三天记录。
    fn app_with_template() -> (PlanApp, Uuid) {
        let mut app = base_app();
        let mut t = Plan::new("打卡".into(), String::new(), time(9, 0));
        t.repeat = RepeatRule::Daily;
        let template_id = t.id;
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 4), "test", true);
        (app, template_id)
    }

    #[test]
    fn delete_template_keeps_records_as_standalone_by_default() {
        let (mut app, template_id) = app_with_template();
        app.ask_delete_template(template_id);
        let prompt = app.delete_prompt.as_ref().expect("应弹出确认");
        assert_eq!(prompt.record_count, 3);
        assert!(!prompt.remove_records, "默认不连带删除记录");

        app.delete_prompt = None;
        app.delete_template(template_id, false);

        assert!(app.doc.templates.is_empty());
        assert_eq!(app.doc.records.len(), 3);
        // 记录还在，但已脱离模板
        assert!(app.doc.records.iter().all(|r| !r.cancelled));
        assert!(app.doc.records.iter().all(|r| r.template_id.is_none()));
        assert_eq!(app.doc.record_indices_for(date(2026, 11, 2)).len(), 1);
    }

    #[test]
    fn delete_template_with_records_clears_generated_days() {
        let (mut app, template_id) = app_with_template();
        app.delete_template(template_id, true);

        assert!(app.doc.templates.is_empty());
        assert_eq!(app.doc.records.len(), 3, "记录以墓碑保留，避免被补回");
        assert!(app.doc.records.iter().all(|r| r.cancelled));
        for d in [2, 3, 4] {
            assert!(app
                .doc
                .record_indices_for(date(2026, 11, d))
                .is_empty());
        }
    }

    #[test]
    fn delete_template_leaves_other_templates_records_alone() {
        let (mut app, template_id) = app_with_template();
        let mut other = Plan::new("别的".into(), String::new(), time(10, 0));
        other.repeat = RepeatRule::Daily;
        let other_id = other.id;
        app.doc.templates.push(other);
        app.materialize_range(date(2026, 11, 2), date(2026, 11, 4), "test", true);

        app.delete_template(template_id, true);

        let kept: Vec<_> = app
            .doc
            .records
            .iter()
            .filter(|r| !r.cancelled)
            .collect();
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|r| r.template_id == Some(other_id)));
    }

    #[test]
    fn tag_flows_from_template_to_records() {
        let mut app = base_app();
        let mut t = Plan::new("交房租".into(), String::new(), time(9, 0));
        t.tag = "每月固定".into();
        t.repeat = RepeatRule::Monthly { day: 1 };
        let template_id = t.id;
        app.doc.templates.push(t);
        app.materialize_range(date(2026, 11, 1), date(2026, 12, 1), "test", true);

        // 生成时带走标签
        assert!(app.doc.records.iter().all(|r| r.tag == "每月固定"));

        // 改模板标签 → 同步到未定制的未来记录
        app.doc.templates[0].tag = "纪念日提醒".into();
        app.sync_template_future_records(template_id, date(2026, 11, 2));
        assert_eq!(app.doc.records[1].tag, "纪念日提醒");
        assert_eq!(app.doc.records[0].tag, "每月固定", "历史记录不被改写");
    }

    #[test]
    fn standalone_record_keeps_its_own_tag() {
        let mut app = base_app();
        let mut state = EditState::new_record(date(2026, 11, 5));
        state.title = "妈妈生日".into();
        state.tag = "生日提醒".into();
        state.hour = 8;
        app.edit = Some(state);
        app.save_edit();

        assert!(app.doc.templates.is_empty());
        assert_eq!(app.doc.records[0].tag, "生日提醒");
        assert_eq!(app.doc.all_tags(), vec!["生日提醒".to_string()]);
    }

    #[test]
    fn tag_is_trimmed_and_length_limited() {
        let mut state = EditState::new_template();
        state.title = "测试".into();
        state.tag = "  每日计划  ".into();
        assert_eq!(state.build().unwrap().tag, "每日计划");

        state.tag = "一二三四五六七八九十一二三".into(); // 13 个字
        assert!(state.build().is_err(), "超过 12 字应报错");

        state.tag = "   ".into();
        assert_eq!(state.build().unwrap().tag, "", "全空白视为无标签");
    }

    #[test]
    fn all_tags_are_deduped_and_sorted() {
        let mut app = base_app();
        let mut a = Plan::new("A".into(), String::new(), time(9, 0));
        a.tag = "每日计划".into();
        let mut b = Plan::new("B".into(), String::new(), time(9, 0));
        b.tag = "生日提醒".into();
        let mut c = Plan::new("C".into(), String::new(), time(9, 0));
        c.tag = "每日计划".into(); // 重复
        let d = Plan::new("D".into(), String::new(), time(9, 0)); // 无标签
        app.doc.templates.extend([a, b, c, d]);

        assert_eq!(
            app.doc.all_tags(),
            vec!["每日计划".to_string(), "生日提醒".to_string()]
        );
    }

    #[test]
    fn tag_dictionary_outlives_records() {
        let mut app = base_app();
        app.doc.remember_tag("纪念日提醒");
        assert!(app.doc.all_tags().contains(&"纪念日提醒".to_string()));

        // 记录全删掉，标签仍在字典里
        let mut state = EditState::new_record(date(2026, 11, 5));
        state.title = "结婚纪念日".into();
        state.tag = "纪念日提醒".into();
        app.edit = Some(state);
        app.save_edit();
        let id = app.doc.records[0].id;
        app.cancel_record(id);
        assert!(app.doc.record_indices_for(date(2026, 11, 5)).is_empty());
        assert_eq!(app.doc.all_tags(), vec!["纪念日提醒".to_string()]);

        // 从字典移除后，下拉里就没有了
        app.doc.forget_tag("纪念日提醒");
        assert!(app.doc.all_tags().is_empty());
    }

    #[test]
    fn remember_tag_ignores_blank_and_duplicates() {
        let mut doc = Doc::default();
        assert!(doc.remember_tag("每日计划"));
        assert!(!doc.remember_tag("每日计划"), "重复不新增");
        assert!(!doc.remember_tag("   "), "空白不新增");
        assert_eq!(doc.tags, vec!["每日计划".to_string()]);
    }

    #[test]
    fn saving_a_record_registers_its_tag() {
        let mut app = base_app();
        let mut state = EditState::new_record(date(2026, 11, 6));
        state.title = "妈妈生日".into();
        state.tag = "生日提醒".into();
        app.edit = Some(state);
        app.save_edit();
        assert_eq!(app.doc.tags, vec!["生日提醒".to_string()]);
    }
}
// ============================== 设置对话框 ==============================
fn ui_settings(app: &mut PlanApp, ctx: &egui::Context) {
    if !app.show_settings {
        return;
    }
    let mut open = app.show_settings;
    let mut close = false;
    let mut apply = false;

    egui::Window::new("设置")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .title_bar(false)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(18)),
        )
        .title_bar(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_min_width(470.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("设置").size(18.0).strong().color(theme::TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(button_mini("✕")).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(2.0);
            ui.separator();
            ui.add_space(8.0);

            // ---- 存储方式 ----
            ui.label(
                RichText::new("存储方式")
                    .size(13.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut app.cfg_kind, Kind::Local, "  仅本地 JSON  ");
                ui.selectable_value(&mut app.cfg_kind, Kind::Git, "  Git 仓库同步  ");
            });
            ui.add_space(2.0);
            let hint = match app.cfg_kind {
                Kind::Local => format!(
                    "存到 {}",
                    store::app_dir().join(store::DOC_DIR_NAME).display()
                ),
                Kind::Git => {
                    if store::git_available() {
                        "启动时拉取，改动后自动推送".to_string()
                    } else {
                        "⚠ 未检测到 git 命令，Git 同步不可用".to_string()
                    }
                }
            };
            ui.label(RichText::new(hint).small().color(theme::TEXT_WEAK));

            // ---- Git 配置 ----
            if app.cfg_kind == Kind::Git {
                ui.add_space(10.0);
                ui.label(
                    RichText::new("Git 仓库")
                        .size(13.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(2.0);
                egui::Grid::new("git_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("仓库地址").color(theme::TEXT_WEAK));
                        text_input_sized(
                            ui,
                            &mut app.cfg_git.url,
                            320.0,
                            "https://example.com/me/daily.git",
                        );
                        ui.end_row();

                        ui.label(RichText::new("用户名").color(theme::TEXT_WEAK));
                        text_input_sized(
                            ui,
                            &mut app.cfg_git.username,
                            320.0,
                            "可选；用访问令牌时留空即可",
                        );
                        ui.end_row();

                        ui.label(RichText::new("密码 / 访问令牌").color(theme::TEXT_WEAK));
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut app.cfg_git.password)
                                .frame(input_frame())
                                .desired_width(320.0)
                                .password(true)
                                .hint_text("可只填访问令牌（GitHub/Gitee 的 PAT）"),
                        );
                        paint_focus_ring(ui, &resp);
                        ui.end_row();

                        ui.label(RichText::new("分支").color(theme::TEXT_WEAK));
                        text_input_sized(ui, &mut app.cfg_git.branch, 120.0, "");
                        ui.end_row();

                        ui.label(RichText::new("数据文件").color(theme::TEXT_WEAK));
                        text_input_sized(ui, &mut app.cfg_git.path, 200.0, "");
                        ui.end_row();
                    });
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "提示：访问令牌以明文保存在 settings.json，请勿使用主账号密码。\
                         GitHub / Gitee 只需填「密码 / 访问令牌」，用户名留空即可。",
                    )
                    .small()
                    .color(theme::WARN),
                );
            }

            // ---- 其它 ----
            ui.add_space(12.0);
            ui.label(RichText::new("其它").size(13.0).strong().color(theme::TEXT));
            ui.add_space(2.0);
            let mut sound = app.settings.sound;
            if ui.checkbox(&mut sound, "到点播放提示音").changed() {
                app.settings.sound = sound;
                app.save_settings();
            }
            let mut autostart = app.settings.autostart;
            if ui
                .checkbox(&mut autostart, "开机自启（当前用户，无需管理员）")
                .changed()
            {
                app.set_autostart(autostart);
            }

            if let Some(err) = &app.cfg_error {
                ui.add_space(6.0);
                ui.colored_label(theme::DANGER, err);
            }
            if let Some(msg) = &app.cfg_msg {
                ui.add_space(6.0);
                ui.colored_label(theme::OK, msg);
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.add(button_primary("保存设置")).clicked() {
                    apply = true;
                }
                ui.add_space(4.0);
                if ui.add(button_ghost("关闭")).clicked() {
                    close = true;
                }
            });
            if app.syncing {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("同步中…").small().color(theme::TEXT_WEAK));
                });
            }
        });

    if apply {
        app.apply_settings();
    }
    if close || !open {
        app.show_settings = false;
    }
}

impl PlanApp {
    /// 把设置页的临时值落盘，并按新配置重建后端。
    fn apply_settings(&mut self) {
        let kind_changed = self.settings.kind != self.cfg_kind;
        let git_changed = !git_config_eq(&self.settings.git, &self.cfg_git);

        if self.cfg_kind == Kind::Git && !self.cfg_git.is_complete() {
            self.cfg_error = Some("Git 模式需要填写仓库地址与分支".into());
            return;
        }

        self.settings.kind = self.cfg_kind.clone();
        self.settings.git = self.cfg_git.clone();
        self.save_settings();
        self.cfg_error = None;
        self.cfg_msg = Some("设置已保存".into());

        if kind_changed || git_changed {
            // 改了仓库配置，或刚启用 Git：把数据目录接上新远端并拉一次。
            // prepare 会 git init / 校正 remote / fetch / 把远端内容铺进数据目录；
            // 网络交互**绝不能放在 UI 线程**，丢给后台线程，结果经 poll_sync 回填。
            //
            // 注意方向：**远端 → 本地**。远端确实有数据时才覆盖本地（见 Switched 分支）。
            if self.settings.kind == Kind::Git {
                if !store::git_available() {
                    self.cfg_error = Some("未检测到 git 命令，无法使用 Git 同步".into());
                    return;
                }
                self.syncing = true;
                self.cfg_msg = Some("正在从 Git 拉取…".into());
                let settings = self.settings.clone();
                let tx = self.sync_tx.clone();
                let local_dir = store::app_dir().join(store::DOC_DIR_NAME);
                std::thread::spawn(move || {
                    let git = GitStore::new(local_dir, settings.git.clone());
                    // Ok(true) = 远端有数据并已铺进本地；Ok(false) = 远端还没有数据
                    let res = git
                        .pull()
                        .map_err(|e| e.to_string());
                    let _ = tx.send(SyncMsg::Switched(res));
                });
            } else if let Err(e) = self.persist() {
                self.cfg_error = Some(format!("写入本地文件失败：{e}"));
            } else {
                self.cfg_msg = Some("已关闭 Git 同步".into());
            }
        }
    }
}

fn git_config_eq(a: &GitConfig, b: &GitConfig) -> bool {
    a.url == b.url
        && a.username == b.username
        && a.password == b.password
        && a.branch == b.branch
        && a.path == b.path
}

// ============================== 提醒弹窗 ==============================

/// 提醒窗的内边距（顶部略小于底部，按钮不会压在圆角上）。
const TOAST_PAD_TOP: f32 = 13.0;
const TOAST_PAD_BOTTOM: f32 = 17.0;

/// 标签配色：同一标签名永远得到同一个颜色，不随记录顺序变化。
fn tag_color(tag: &str) -> Color32 {
    const PALETTE: [Color32; 6] = [
        Color32::from_rgb(72, 187, 140),  // 绿
        Color32::from_rgb(120, 160, 255), // 蓝
        Color32::from_rgb(230, 168, 84),  // 琥珀
        Color32::from_rgb(210, 130, 220), // 紫
        Color32::from_rgb(96, 200, 214),  // 青
        Color32::from_rgb(238, 128, 132), // 珊瑚
    ];
    let mut hash: u32 = 2166136261;
    for b in tag.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    PALETTE[(hash % PALETTE.len() as u32) as usize]
}

/// 浅色主题下标签用淡底色 + 深字，深色提醒窗里直接用彩色文字。
fn tag_badge_light(ui: &mut egui::Ui, tag: &str) {
    let color = tag_color(tag);
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            28,
        ))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(tag).size(11.0).strong().color(color));
        });
}

/// 首帧还没量到内容高度时的估算值（无备注 / 有备注）。
///
/// 现在固定有「标签·标题」行 + 日期时间行 + 按钮行，备注会换行，所以估值比原来高。
fn toast_estimate_content_h(has_note: bool) -> f32 {
    if has_note {
        132.0
    } else {
        106.0
    }
}

/// 主屏「工作区」矩形，单位是**物理像素**（已扣掉任务栏）。
///
/// 用 `SystemParametersInfo(SPI_GETWORKAREA)` 取真实可用区域，比
/// `viewport().monitor_size` 可靠：后者在 egui-winit 里是
/// `monitor.size().to_logical(pixels_per_point)`，一旦 `pixels_per_point`
/// 和显示器实际缩放不一致（例如切换过分辨率/缩放、或远程桌面重连），
/// 拿到的数值就是错的。
///
/// 注意这里**只返回物理像素**，不做任何 points 换算：换算必须用弹窗
/// 自己的缩放比来做，详见 `ui_popups()` 里的说明。
#[cfg(windows)]
fn toast_work_area_px() -> Option<egui::Rect> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // 第三个参数是输出缓冲区指针，SPI_GETWORKAREA 会写入 RECT。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let (l, t) = (rect.left as f32, rect.top as f32);
    let (r, b) = (rect.right as f32, rect.bottom as f32);
    if r - l < 400.0 || b - t < 300.0 {
        return None;
    }
    Some(egui::Rect::from_min_max(
        egui::pos2(l, t),
        egui::pos2(r, b),
    ))
}

#[cfg(not(windows))]
fn toast_work_area_px() -> Option<egui::Rect> {
    None
}

/// 主屏**当前真实**的缩放比（1.0 = 96 DPI，1.5 = 144 DPI）。
///
/// 必须问 Windows，不能用 `ctx.pixels_per_point()`：后者是 egui 内部缓存的
/// 值，来自窗口创建时 winit 报告的 scale factor。如果进程启动后显示环境变了
/// （切换分辨率、改缩放、远程桌面重连），主窗口可能收不到
/// `ScaleFactorChanged`，这个缓存就会一直停在旧值。
///
/// 真实场景：进程在 150% 下启动，之后桌面变成 100%，`ctx.pixels_per_point()`
/// 仍是 1.5。用它把 1920 物理像素的工作区折算成 1280 points，弹窗就会落在
/// 屏幕水平方向 44%~66% 的位置 —— 也就是用户看到的「跑到屏幕中间」。
#[cfg(windows)]
fn toast_scale(ctx: &egui::Context) -> f32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() };
    if dpi >= 48 {
        dpi as f32 / 96.0
    } else {
        ctx.pixels_per_point().max(0.5)
    }
}

#[cfg(not(windows))]
fn toast_scale(ctx: &egui::Context) -> f32 {
    ctx.pixels_per_point().max(0.5)
}

/// 提醒弹窗的「右下角堆叠」位置计算，单位统一为**物理像素**。
///
/// 抽成纯函数是为了能写单元测试：定位这类 bug 一旦回归，
/// 用户看到的现象是「到点了没提醒」（窗口飞到屏幕外）或「弹窗跑到中间」，
/// 都很难靠肉眼复现。
///
/// * `work_px`  —— 主屏工作区（物理像素，已扣任务栏）
/// * `scale`    —— 该显示器真实缩放比（1.0 = 96 DPI）
/// * `card_w`   —— 卡片逻辑宽度
/// * `sizes`    —— 每条提醒的卡片逻辑高度，按**叠放顺序**给（末尾那条在最下面）
///
/// 返回每张卡片的左上角**物理像素**坐标，顺序与 `sizes` 一致。
fn toast_layout_px(
    work_px: egui::Rect,
    scale: f32,
    card_w: f32,
    margin: f32,
    gap: f32,
    sizes: &[f32],
) -> Vec<egui::Pos2> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    // 逻辑尺寸 → 物理像素。
    let card_w_px = card_w * scale;
    let margin_px = margin * scale;
    let gap_px = gap * scale;
    let area = work_px;
    // 右下角贴边，逐条向上叠。
    let left = area.max.x - card_w_px - margin_px;
    let mut bottom = area.max.y - margin_px;
    let mut out = Vec::with_capacity(sizes.len());
    for h in sizes {
        let h_px = h * scale;
        let mut y = bottom - h_px;
        // 顶到屏幕上沿就不再上移，避免超出工作区。
        y = y.max(area.min.y + margin_px);
        out.push(egui::pos2(left, y));
        bottom = y - gap_px;
    }
    out
}

fn ui_popups(app: &mut PlanApp, ctx: &egui::Context) {
    if app.popups.is_empty() {
        app.popup_heights.clear();
        return;
    }

    const CARD_W: f32 = 420.0;
    const MARGIN: f32 = 18.0;
    const GAP: f32 = 10.0;

    // ── 坐标系说明（这段是踩坑记录，改动前务必读完）────────────────────────
    //
    // 提醒弹窗是**独立的 OS 窗口**，不是主窗口里的控件，所以它的位置要走
    // `ViewportBuilder::with_position`（单位：egui points）。
    //
    // egui-winit 把 points 转成物理像素时，用的是**那个窗口自己的**
    // `pixels_per_point = zoom_factor × native_scale`。问题在于
    // `ctx.pixels_per_point()` 是 egui 内部缓存的值，来自窗口创建时 winit
    // 报告的 scale factor；进程启动后如果显示环境变了（改缩放比例、
    // 切换分辨率、远程桌面重连），主窗口可能根本收不到
    // `ScaleFactorChanged`，这个缓存就一直停在旧值上。
    //
    // 真实复现：进程在 150% 下启动 → 远程桌面把桌面变成 100%（96 DPI）。
    // 此时 `ctx.pixels_per_point()` 仍是 1.5，而新弹窗窗口的 scale 是 1.0。
    // 把 1920 物理像素的工作区除以 1.5 得到 1280 points，算出
    // x = 1280 - 420 - 18 = 842，再被弹窗按 1.0 转回物理像素 → 842 px，
    // 而屏幕有 1920 px 宽 —— 弹窗就落在水平 44% 的位置，也就是
    // 用户看到的「跑到屏幕中间」。
    //
    // 所以：**缩放比必须现问 Windows（GetDpiForSystem），不能用
    // ctx.pixels_per_point()**。见 `toast_scale()`。
    let scale = toast_scale(ctx);

    // 可用屏幕区域（= 屏幕减去任务栏），单位 points，供 `with_position` 用。
    let work_area = toast_work_area_px().unwrap_or_else(|| {
        // 非 Windows 或取不到时的兜底：用 viewport 的物理像素尺寸。
        let ppp = ctx.pixels_per_point().max(0.5);
        let size = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or(egui::vec2(1920.0, 1080.0));
        // monitor_size 在不同 egui 版本里单位不一致，这里按「物理像素」处理。
        let px = if size.x > 3000.0 { size / ppp } else { size * ppp };
        egui::Rect::from_min_size(egui::Pos2::ZERO, px)
    });
    let mut actions: Vec<PopupAction> = Vec::new();
    let mut measured: Vec<(Uuid, f32)> = Vec::new();
    let total_popups = app.popups.len();

    // 先把所有卡片的高度算出来，统一交给 `toast_layout_px()` 求位置。
    // 顺序：`app.popups` 末尾那条在最下面，所以反着叠放。
    let heights: Vec<f32> = (0..app.popups.len())
        .rev()
        .map(|idx| {
            let popup = &app.popups[idx];
            let has_note = !popup.note.trim().is_empty();
            // 这里存的是「内容块」高度，与窗口尺寸无关，所以不会出现「测高→改窗口→再测高」的抖动。
            let content_h = app
                .popup_heights
                .get(&popup.record_id)
                .copied()
                .unwrap_or_else(|| toast_estimate_content_h(has_note));
            content_h + TOAST_PAD_TOP + TOAST_PAD_BOTTOM
        })
        .collect();
    let boxes_px = toast_layout_px(work_area, scale, CARD_W, MARGIN, GAP, &heights);

    for (slot, idx) in (0..app.popups.len()).rev().enumerate() {
        let popup = &app.popups[idx];
        let h = heights[slot];
        // 目标位置始终是**物理像素**，建窗和逐帧纠正都从这里换算。
        let pos_px = boxes_px[slot];
        let pos = pos_px / scale;

        let viewport_id = egui::ViewportId::from_hash_of(("record-toast", popup.record_id));
        let builder = egui::ViewportBuilder::default()
            .with_title("")
            .with_inner_size([CARD_W, h])
            .with_position(pos)
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top()
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_taskbar(false)
            .with_active(false);

        ctx.show_viewport_immediate(viewport_id, builder, |vui, _| {
            let vctx = vui.ctx().clone();
            // 自我校正：用**弹窗这个窗口自己的**缩放比把物理坐标换算回 points。
            // 万一 `toast_scale()` 与弹窗窗口实际的 scale 有偏差（例如主窗口
            // 的缩放缓存过期），这里每帧都会把它纠回右下角，不会再漂到屏幕中间。
            let child_ppp = vctx.pixels_per_point().max(0.5);
            vctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos_px / child_ppp));
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::NONE
                        .fill(theme::TOAST_BG)
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(57, 66, 94)))
                        .corner_radius(12)
                        .inner_margin(egui::Margin {
                            left: 16,
                            right: 16,
                            top: TOAST_PAD_TOP as i8,
                            bottom: TOAST_PAD_BOTTOM as i8,
                        }),
                )
                .show_inside(vui, |ui| {
                    let body = ui.scope(|ui| {
                        // 关掉默认 8px 行距，行高改由下面的 add_space 精确控制。
                        ui.spacing_mut().item_spacing.y = 0.0;

                        // ── 第 1 行：标签 + 标题（同一行）+ 关闭 ──
                        // 标签用彩色圆点 + 彩色小字，标题紧随其后用白字，
                        // 这样「标签 - 标题」一眼读完，不用分两行占高度。
                        let tag = popup.tag.trim();
                        let tag_color = if tag.is_empty() {
                            Color32::from_rgb(150, 160, 184)
                        } else {
                            tag_color(tag)
                        };
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (dot, _) =
                                ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot.center(), 3.5, tag_color);
                            ui.label(
                                RichText::new(if tag.is_empty() { "提醒" } else { tag })
                                    .size(11.5)
                                    .strong()
                                    .color(tag_color),
                            );
                            // 标签和标题之间用一个弱化的小横杠分隔。
                            ui.label(
                                RichText::new("·")
                                    .size(11.5)
                                    .color(Color32::from_rgb(110, 122, 158)),
                            );
                            // 标题：不截断，长标题换行显示全。
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&popup.title)
                                        .strong()
                                        .size(15.0)
                                        .color(Color32::WHITE),
                                )
                                .wrap(),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                // 单个关闭。
                                if icon_button(ui, CloseIcon::Single, "关闭这一条") {
                                    actions.push(PopupAction::Dismiss(popup.record_id));
                                }
                                // 批量关闭：只在有多个弹窗时出现，避免单条时误点。
                                if total_popups > 1
                                    && icon_button(
                                        ui,
                                        CloseIcon::All,
                                        &format!("关闭全部 {total_popups} 条提醒"),
                                    )
                                {
                                    actions.push(PopupAction::DismissAll);
                                }
                            });
                        });

                        // ── 第 2 行：计划日期时间 ──
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "📅 {}  周{}  {}",
                                popup.date.format("%Y年%m月%d日"),
                                weekday_cn(popup.date),
                                model::fmt_time(popup.time)
                            ))
                            .size(11.5)
                            .color(Color32::from_rgb(150, 165, 200)),
                        );

                        // ── 第 3 行：备注，完整换行显示，不截断 ──
                        if !popup.note.trim().is_empty() {
                            ui.add_space(5.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&popup.note)
                                        .size(12.0)
                                        .color(Color32::from_rgb(190, 200, 220)),
                                )
                                .wrap(),
                            );
                        }

                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            // 深色通知窗里的主操作：实心绿底 + 白字，浅绿底配白字几乎不可读。
                            let done_btn = egui::Button::new(
                                RichText::new("✓  完成").strong().color(Color32::WHITE),
                            )
                            .fill(theme::OK)
                            .stroke(egui::Stroke::new(1.0_f32, theme::OK))
                            .corner_radius(egui::CornerRadius::same(8))
                            .min_size(egui::vec2(88.0, 32.0));
                            if ui.add(done_btn).clicked() {
                                actions.push(PopupAction::Complete(popup.record_id));
                            }
                            // 次要操作也做成实心按钮：浅一档的底色 + 亮描边 + 白字，
                            // 在深色窗里既看得见又点得准（原来只是灰字，像说明文字）。
                            let know_btn = egui::Button::new(
                                RichText::new("知道了").strong().color(Color32::WHITE),
                            )
                            .fill(theme::TOAST_BTN)
                            .stroke(egui::Stroke::new(1.0_f32, theme::TOAST_BTN_STROKE))
                            .corner_radius(egui::CornerRadius::same(8))
                            .min_size(egui::vec2(88.0, 32.0));
                            if ui.add(know_btn).clicked() {
                                actions.push(PopupAction::Dismiss(popup.record_id));
                            }
                        });
                    });
                    // 内容块实际占用高度：下一帧据此把窗口收到刚好包住内容 + 内边距。
                    measured.push((popup.record_id, body.response.rect.height()));
                });
        });
    }

    app.popup_heights = measured.into_iter().collect();

    for action in actions {
        match action {
            PopupAction::Dismiss(id) => app.popups.retain(|p| p.record_id != id),
            PopupAction::Complete(id) => app.set_done(id, true),
            PopupAction::DismissAll => {
                app.popups.clear();
                app.popup_heights.clear();
            }
        }
    }
    ctx.request_repaint();
}









