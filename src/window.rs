use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId, WaitForSingleObject};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::compact_layout::{self, BadgeHit, ColorKey, DrawCmd, FontKey, Metrics, Scene, TileSize};
use crate::compact_view::{self, CompactViewModel};
use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{
    AppUsageData, ProviderStatus, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_WEEK_SECONDS,
};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL, TIMER_UPDATE_CHECK, WM_APP_TRAY,
    WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::settings::{
    self, default_provider_order, SettingsFile, POLL_15_MIN, POLL_1_HOUR, POLL_1_MIN, POLL_5_MIN,
};
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
#[derive(Clone, Debug, Default)]
struct WidgetUsageWindow {
    percent: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct ProviderWidgetData {
    windows: Vec<WidgetUsageWindow>,
}

struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    is_high_contrast: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    claude_widget: ProviderWidgetData,
    codex_widget: ProviderWidgetData,
    antigravity_widget: ProviderWidgetData,
    compact_vm: CompactViewModel,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    provider_order: Vec<tray_icon::TrayIconKind>,
    pending_provider_order: Option<Vec<tray_icon::TrayIconKind>>,
    pending_provider_order_samples: u8,
    last_observed_tray_order: Option<Vec<tray_icon::TrayIconKind>>,

    data: Option<AppUsageData>,
    /// True while `data` came from the persisted snapshot of a previous run
    /// (shown immediately at startup); cleared by the first successful poll.
    data_is_cached: bool,
    /// The error of the last completely failed poll (every enabled provider
    /// failed), for the detail popup's per-provider status badges.
    last_error: Option<poller::PollError>,

    poll_interval_ms: u32,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    last_success_unix: Option<u64>,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,
    details_hwnd: Option<HWND>,
    floating_hwnd: Option<HWND>,
    floating_visible: bool,
    detailed_tray_icons: bool,
    floating_x: Option<i32>,
    floating_y: Option<i32>,
    widget_tooltip_hwnd: Option<SendHwnd>,

    taskbar_index: usize,
    tray_offset: i32,
    preferred_taskbar_index: usize,
    preferred_tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_client_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const RATE_LIMIT_MIN_RETRY_MS: u32 = POLL_5_MIN;
const RATE_LIMIT_MAX_RETRY_MS: u32 = POLL_1_HOUR;

const IDM_REFRESH_NOW: u16 = 1;
// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_TOGGLE_FLOATING: u16 = 32;
const IDM_RESET_FLOATING_POSITION: u16 = 34;
const IDM_DETAILED_TRAY_ICONS: u16 = 35;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_LANG_RUSSIAN: u16 = 49;
const IDM_LANG_PORTUGUESE_BRAZIL: u16 = 50;
const IDM_LANG_SIMPLIFIED_CHINESE: u16 = 51;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_MODEL_ANTIGRAVITY: u16 = 62;
const IDM_NOTIFY_SESSION_RESET: u16 = 80;
const IDM_NOTIFY_WEEKLY_RESET: u16 = 81;

const WM_DPICHANGED_MSG: u32 = 0x02E0;
/// WM_MOUSELEAVE (winuser.h), kept local to avoid pulling a control-specific
/// constant into the custom tooltip implementation.
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
const WIDGET_TOOLTIP_DELAY_MS: u32 = 650;
const WIDGET_TOOLTIP_MIN_WIDTH: i32 = 180;
const WIDGET_TOOLTIP_MAX_WIDTH: i32 = 320;
const WIDGET_TOOLTIP_EDGE_GAP: i32 = 7;
const TIMER_WIDGET_TOOLTIP: usize = 14;
/// Timer on the broadcast helper window that coalesces setting/display
/// broadcast bursts into one refresh (trailing-edge debounce).
const TIMER_BROADCAST_DEBOUNCE: usize = 10;
const BROADCAST_DEBOUNCE_MS: u32 = 250;
const TIMER_TRAY_ORDER: usize = 11;
const TRAY_ORDER_SAMPLE_MS: u32 = 1_000;
const TIMER_TRAY_ORDER_CONFIRM: usize = 13;
const TRAY_ORDER_CONFIRM_MS: u32 = 120;
const TRAY_ORDER_STABLE_SAMPLES: u8 = 2;
const TRAY_ORDER_EVENT_THROTTLE_MS: u128 = 100;
/// The detail popup owns this timer. It only refreshes locally formatted
/// countdown text; provider requests continue to follow the configured poll
/// interval on the main window.
const TIMER_DETAIL_REFRESH: usize = 12;
const DETAIL_REFRESH_MS: u32 = 1_000;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
/// Thread message (msg.hwnd == null) handled directly in the message loop:
/// recreate/re-attach the widget window after it was destroyed externally.
const WM_APP_REVIVE: u32 = WM_APP + 4;
/// Thread message posted by the revival background thread once the taskbar
/// set is stable and the UI thread should recreate/re-attach the widget.
const WM_APP_REVIVE_READY: u32 = WM_APP + 5;
/// Stable process-level request for the UI thread to perform a deliberate
/// shutdown, even if the embedded main window was replaced during revival.
const WM_APP_REQUEST_QUIT: u32 = WM_APP + 6;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

/// WM_WTSSESSION_CHANGE and the wparam values we care about (winuser.h).
const WM_WTSSESSION_CHANGE_MSG: u32 = 0x02B1;
const WTS_CONSOLE_CONNECT: usize = 1;
const WTS_CONSOLE_DISCONNECT: usize = 2;
const WTS_REMOTE_CONNECT: usize = 3;
const WTS_REMOTE_DISCONNECT: usize = 4;
const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

/// How often the watchdog thread polls for an explorer.exe restart (which
/// recreates the taskbar and wipes our tray-icon registration).
const TASKBAR_WATCH_INTERVAL_SECS: u64 = 2;

/// Revival tuning: how often/patiently to retry widget-window creation before
/// giving up. Taskbar availability itself is retried by shell events and the
/// watchdog without ever exposing the widget as a desktop popup.
const REVIVE_CREATE_ATTEMPTS: u32 = 12;
const REVIVE_CREATE_RETRY_DELAY: Duration = Duration::from_secs(5);
static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Set when the user picks Exit: WM_DESTROY then means a deliberate quit,
/// anything else means explorer destroyed our embedded window and we revive.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set while a revival is running so the watchdog does not interfere.
static REVIVING: AtomicBool = AtomicBool::new(false);

/// Failed window-creation attempts of the current revival; reset when a
/// revival starts or completes. Once it reaches REVIVE_CREATE_ATTEMPTS the
/// revival gives up and falls back to a process relaunch.
static REVIVE_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Unix time when the in-flight revival began; 0 when none. The watchdog
/// uses it as a backstop: if a revival's READY signal is ever lost (see
/// post_revive_ready), REVIVING would otherwise stay true forever and
/// permanently disable revival detection.
static REVIVING_SINCE: AtomicU64 = AtomicU64::new(0);

/// A revival older than this is considered stuck and its in-flight flag is
/// force-cleared so detection re-arms. Legitimate revivals stay well under:
/// the stability wait caps at 120s and 12 create retries add ~60s.
const REVIVE_STUCK_RESET_SECS: u64 = 600;

/// The broadcast helper window handle once created (0 = none). Revival
/// signals are posted here rather than as thread messages: modal message
/// loops (context menu, message boxes) pump-and-discard NULL-hwnd thread
/// messages, while window messages are dispatched correctly.
static BROADCAST_HELPER_HWND: AtomicIsize = AtomicIsize::new(0);

/// Registered shell message sent when Explorer recreates the taskbar.
/// Kept on the process-level helper so recovery still works after the
/// embedded widget window has been destroyed with its old parent.
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

/// The hidden process-level helper receives WTS notifications even when
/// Explorer destroys the embedded widget, so this can remain set for the full
/// lock/disconnect interval without stopping provider polling.
static SESSION_INACTIVE: AtomicBool = AtomicBool::new(false);

struct PollCoordinator {
    in_flight: AtomicBool,
    pending: AtomicBool,
    force_claude_refresh: AtomicBool,
    generation: AtomicU64,
}

impl PollCoordinator {
    const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            force_claude_refresh: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    /// Register a refresh request. The caller that changes `in_flight` from
    /// false to true owns starting the single worker; every other caller is
    /// collapsed into the worker's one pending follow-up pass.
    fn request(&self, force_claude_refresh: bool) -> bool {
        if force_claude_refresh {
            self.force_claude_refresh.store(true, Ordering::Release);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending.store(true, Ordering::Release);
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn begin_pass(&self) -> (u64, bool) {
        self.pending.store(false, Ordering::Release);
        (
            self.generation.load(Ordering::Acquire),
            self.force_claude_refresh.swap(false, Ordering::AcqRel),
        )
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    #[cfg(test)]
    fn invalidate_pending(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending.store(false, Ordering::Release);
        self.force_claude_refresh.store(false, Ordering::Release);
    }

    /// Return true when this worker should immediately perform the one
    /// coalesced follow-up pass. The second check closes the race where a
    /// request arrives between the first pending check and releasing ownership.
    fn finish_pass(&self) -> bool {
        if self.pending.load(Ordering::Acquire) {
            return true;
        }

        self.in_flight.store(false, Ordering::Release);
        if !self.pending.load(Ordering::Acquire) {
            return false;
        }

        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

static POLL_COORDINATOR: PollCoordinator = PollCoordinator::new();

fn watchdog_needs_taskbar_recovery(
    widget_exists: bool,
    binding_valid: bool,
    taskbar_available: bool,
) -> bool {
    taskbar_available && (!widget_exists || !binding_valid)
}

/// UI thread id, so the watchdog can reach the message loop once the window
/// (the usual PostMessage target) no longer exists.
static UI_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// The Win32 window class; also part of the app's identity (kept distinct
/// from the original CodeZeno app so both can run side by side).
const WINDOW_CLASS_NAME: &str = "AIUsageMonitor";
const DETAIL_WINDOW_CLASS_NAME: &str = "AIUsageMonitorDetails";
const FLOATING_WINDOW_CLASS_NAME: &str = "AIUsageMonitorFloating";
const WIDGET_TOOLTIP_WINDOW_CLASS_NAME: &str = "AIUsageMonitorWidgetTooltip";
/// Hidden top-level helper window. Two jobs the embedded widget cannot do
/// itself: receive broadcast messages (WM_SETTINGCHANGE / WM_DISPLAYCHANGE
/// are only sent to top-level windows, and the widget is a WS_CHILD of the
/// taskbar in its normal mode - without this a dark/light theme switch was
/// not reflected until the next poll), and be findable by class name so a
/// second launched instance can ask us to show the detail popup.
const BROADCAST_WINDOW_CLASS_NAME: &str = "AIUsageMonitorBroadcast";
const DETAIL_POPUP_WIDTH: i32 = 408;
/// Title area above the first provider group.
const DETAIL_HEADER_H: i32 = 52;
/// Provider identity line: icon chip + name + compact state label.
const DETAIL_GROUP_HEADER_H: i32 = 44;
/// One quota window: label/bar/percent line plus the reset line below it.
const DETAIL_WINDOW_ROW_H: i32 = 48;
const DETAIL_PRIMARY_LINE_H: i32 = 18;
const DETAIL_GROUP_GAP: i32 = 10;
const DETAIL_CARD_MARGIN: i32 = 18;
const DETAIL_GROUP_PAD_V: i32 = 6;
const DETAIL_LOGO_CHIP_SIZE: i32 = 28;
const DETAIL_BAR_GAP: i32 = 3;
const DETAIL_CONTENT_BOTTOM_PAD: i32 = 12;
const DETAIL_FOOTER_H: i32 = 42;
/// Content height when no provider rows exist yet (waiting message).
const DETAIL_EMPTY_H: i32 = 40;
/// A popup dismissed this recently is treated as "the user clicked the tray
/// icon to close it": the click that caused the focus loss also arrives as an
/// open request, and re-opening would make the popup flicker instead of
/// toggling.
const DETAIL_REOPEN_SUPPRESS_MS: u128 = 300;
// Keep enough room for the DWM shadow without making the surface look detached
// from the display edge. This is scaled with the monitor DPI.
const FLOATING_MARGIN: i32 = 8;
const FLOATING_DRAG_THRESHOLD: i32 = 3;
const FLOATING_CONTENT_LEFT_MARGIN: i32 = 8;
static FLOATING_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
static FLOATING_MOVING: AtomicBool = AtomicBool::new(false);

struct FloatingDragState {
    tracking: bool,
    moved: bool,
    start_cursor_x: i32,
    start_cursor_y: i32,
    start_window_x: i32,
    start_window_y: i32,
}

static FLOATING_DRAG_STATE: Mutex<FloatingDragState> = Mutex::new(FloatingDragState {
    tracking: false,
    moved: false,
    start_cursor_x: 0,
    start_cursor_y: 0,
    start_window_x: 0,
    start_window_y: 0,
});

fn session_is_unstable() -> bool {
    SESSION_INACTIVE.load(Ordering::Acquire)
}

thread_local! {
    /// DPI for the window currently being laid out or painted on the UI
    /// thread. Every HWND entry point installs its own value, so one window
    /// moving between monitors cannot change another window's scale.
    static ACTIVE_WINDOW_DPI: Cell<u32> = const { Cell::new(96) };
}

fn normalize_dpi(dpi: u32) -> u32 {
    if dpi == 0 {
        96
    } else {
        dpi
    }
}

fn scale_px_for_dpi(px: i32, dpi: u32) -> i32 {
    let dpi = normalize_dpi(dpi);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Scale a base pixel value (designed at 96 DPI) for the active HWND.
fn sc(px: i32) -> i32 {
    ACTIVE_WINDOW_DPI.with(|dpi| scale_px_for_dpi(px, dpi.get()))
}

fn active_window_dpi() -> u32 {
    ACTIVE_WINDOW_DPI.with(|dpi| normalize_dpi(dpi.get()))
}

struct DpiScope {
    previous: u32,
}

impl DpiScope {
    fn new(dpi: u32) -> Self {
        let dpi = normalize_dpi(dpi);
        let previous = ACTIVE_WINDOW_DPI.with(|active| {
            let previous = active.get();
            active.set(dpi);
            previous
        });
        Self { previous }
    }

    fn for_window(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        Self::new(dpi)
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        ACTIVE_WINDOW_DPI.with(|active| active.set(self.previous));
    }
}

fn set_default_dpi(dpi: u32) {
    ACTIVE_WINDOW_DPI.with(|active| active.set(normalize_dpi(dpi)));
}

fn dpi_from_wparam(wparam: WPARAM) -> u32 {
    normalize_dpi((wparam.0 & 0xFFFF) as u32)
}

fn suggested_dpi_rect(lparam: LPARAM) -> Option<RECT> {
    if lparam.0 == 0 {
        return None;
    }
    Some(unsafe { *(lparam.0 as *const RECT) })
}

unsafe fn apply_suggested_dpi_rect(hwnd: HWND, lparam: LPARAM, context: &str) {
    let Some(rect) = suggested_dpi_rect(lparam) else {
        diagnose::log(format!(
            "{context}: WM_DPICHANGED had no suggested rectangle"
        ));
        return;
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        diagnose::log(format!(
            "{context}: ignored invalid DPI rectangle ({}, {}, {}, {})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
        return;
    }
    if let Err(error) = SetWindowPos(
        hwnd,
        HWND::default(),
        rect.left,
        rect.top,
        width,
        height,
        SWP_NOACTIVATE | SWP_NOZORDER,
    ) {
        diagnose::log_error(&format!("{context}: unable to apply DPI rectangle"), error);
    }
}

/// Spacing below which two relaunches are treated as a storm (e.g. explorer.exe
/// crash-looping); when detected we back off instead of spawning in a tight loop.
const RELAUNCH_THROTTLE_SECS: u64 = 10;
const RELAUNCH_BACKOFF_SECS: u64 = 30;
/// Environment flag set on a relaunched child so it waits for the previous
/// instance's single-instance mutex instead of exiting immediately.
const ENV_RELAUNCH: &str = "AIUM_RELAUNCH";
/// Unix timestamp (seconds) of the relaunch that spawned this process, passed to
/// the child so it can detect a relaunch storm.
const ENV_LAST_RELAUNCH_UNIX: &str = "AIUM_LAST_RELAUNCH_UNIX";

/// Relaunch the widget as a fresh process. Last-resort recovery only: normal
/// recovery from explorer restarts and RDP session switches happens in-process
/// via `revive_after_destroy` (which keeps state and needs no process handoff).
/// This path remains for when the UI thread is unreachable or window creation
/// keeps failing. The child is flagged via `ENV_RELAUNCH` so it waits for this
/// instance's single-instance mutex to be released before taking over (see the
/// guard in `run`).
fn relaunch_self() {
    // Back off if we are relaunching very soon after the relaunch that spawned
    // us: that signals the shell is crash-looping, not a one-off restart.
    let now = now_unix_secs();
    let last = std::env::var(ENV_LAST_RELAUNCH_UNIX)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if last != 0 && now.saturating_sub(last) < RELAUNCH_THROTTLE_SECS {
        diagnose::log("relaunch storm detected; backing off before relaunching");
        std::thread::sleep(Duration::from_secs(RELAUNCH_BACKOFF_SECS));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            diagnose::log_error("watchdog: unable to resolve current executable", error);
            return;
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(exe)
        .args(&args)
        .env(ENV_RELAUNCH, "1")
        .env(ENV_LAST_RELAUNCH_UNIX, now.to_string())
        .spawn()
    {
        Ok(_) => {
            diagnose::log("watchdog: relaunched fresh instance, exiting old one");
            std::process::exit(0);
        }
        Err(error) => {
            diagnose::log_error("watchdog: unable to spawn relaunched instance", error);
        }
    }
}

/// Detect taskbar changes the message-based paths might miss and trigger
/// recovery. The primary recovery is in-process revival on the UI thread
/// (WM_APP_REVIVE: the message loop outlives the window); this thread is the
/// safety net that notices a changed taskbar while the app is idle, and falls
/// back to a full process relaunch only if the UI thread cannot be reached.
fn spawn_taskbar_watchdog() {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
        // Hold off while a revival is already running or the session is in
        // the middle of an RDP switch / lock screen.
        if REVIVING.load(Ordering::SeqCst) || session_is_unstable() {
            // Backstop: if a revival's READY signal was lost (message loop
            // torn down at the wrong moment, or any other one-off), REVIVING
            // would pin true and permanently disable this watchdog. After a
            // generous timeout, re-arm detection.
            let since = REVIVING_SINCE.load(Ordering::SeqCst);
            if REVIVING.load(Ordering::SeqCst)
                && since != 0
                && now_unix_secs().saturating_sub(since) > REVIVE_STUCK_RESET_SECS
            {
                diagnose::log("watchdog: revival stuck past its deadline; re-arming detection");
                clear_reviving();
            }
            continue;
        }
        let stored = {
            let state = lock_state();
            state.as_ref().map(|s| (s.hwnd.to_hwnd(), s.taskbar_hwnd))
        };
        let Some((hwnd, old)) = stored else {
            continue;
        };
        let taskbars = native_interop::find_taskbars();
        let widget_exists = unsafe { IsWindow(hwnd).as_bool() };
        let binding_valid = widget_exists
            && old.is_some_and(|taskbar| native_interop::is_embedded_in_taskbar(hwnd, taskbar));
        if watchdog_needs_taskbar_recovery(widget_exists, binding_valid, !taskbars.is_empty()) {
            let widget_missing = !widget_exists;
            if widget_missing {
                diagnose::log(format!(
                    "watchdog: widget hwnd missing hwnd={:?} -> requesting revival",
                    hwnd
                ));
            }
            if let Some(taskbar) = taskbars.first() {
                if let Some(old) = old {
                    diagnose::log(format!(
                        "watchdog: taskbar changed old={:?} new={:?} -> requesting revival",
                        old.0, taskbar.hwnd.0
                    ));
                } else {
                    diagnose::log(format!(
                        "watchdog: taskbar returned while widget hidden new={:?} -> requesting revival",
                        taskbar.hwnd.0
                    ));
                }
            }
            // Ask the UI thread to revive in-process (it also covers the case
            // where the window survived and only needs re-attaching). Only if
            // the message cannot be delivered fall back to a full relaunch.
            let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
            let posted = thread_id != 0
                && unsafe {
                    PostThreadMessageW(thread_id, WM_APP_REVIVE, WPARAM(0), LPARAM(0)).is_ok()
                };
            if posted {
                // Give the UI thread one watchdog period to run the immediate
                // in-process re-attachment before re-checking.
                std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
            } else {
                diagnose::log("watchdog: UI thread unreachable -> relaunching");
                relaunch_self();
            }
        }
    });
}

/// Recreate the widget window itself (class is already registered). Only used
/// by revival; the startup path in `run` keeps its own creation code.
unsafe fn recreate_widget_window() -> Option<HWND> {
    let hinstance = match GetModuleHandleW(PCWSTR::null()) {
        Ok(handle) => handle,
        Err(error) => {
            diagnose::log_error("revival: GetModuleHandleW failed", error);
            return None;
        }
    };
    let (title_text, model_count) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.language.strings().window_title,
                active_model_count(s.show_claude_code, s.show_codex, s.show_antigravity),
            ),
            None => return None,
        }
    };
    let class_name = native_interop::wide_str(WINDOW_CLASS_NAME);
    let title = native_interop::wide_str(title_text);
    match CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(class_name.as_ptr()),
        PCWSTR::from_raw(title.as_ptr()),
        WS_POPUP,
        0,
        0,
        total_widget_width_for(model_count),
        sc(WIDGET_HEIGHT),
        HWND::default(),
        HMENU::default(),
        hinstance,
        None,
    ) {
        Ok(hwnd) => Some(hwnd),
        Err(error) => {
            diagnose::log_error("revival: CreateWindowExW failed", error);
            None
        }
    }
}

/// First stage of revival: mark a revival as in flight and immediately ask
/// the UI thread to try the current taskbar. Shell readiness is event-driven
/// (TaskbarCreated/display/session broadcasts plus the watchdog), so delaying
/// the first attempt only makes RDP and Explorer recovery visibly slower.
fn revive_request() {
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        return;
    }
    if session_is_unstable() {
        diagnose::log("revival deferred while session is locked or disconnected");
        return;
    }
    if REVIVING.swap(true, Ordering::SeqCst) {
        return; // another revival is already in flight
    }
    REVIVING_SINCE.store(now_unix_secs(), Ordering::SeqCst);
    REVIVE_ATTEMPTS.store(0, Ordering::SeqCst);
    diagnose::log("revival: begin (immediate taskbar re-attach attempt)");
    post_revive_ready();
}

fn clear_reviving() {
    REVIVING.store(false, Ordering::SeqCst);
    REVIVING_SINCE.store(0, Ordering::SeqCst);
}

fn post_revive_ready() {
    // Prefer the broadcast helper window as the target: a NULL-hwnd thread
    // message retrieved by a modal message loop (context menu, message box)
    // is silently discarded by DispatchMessageW, which would strand
    // REVIVING=true forever; window messages survive modal loops.
    let helper = BROADCAST_HELPER_HWND.load(Ordering::SeqCst);
    if helper != 0 {
        let helper = HWND(helper as *mut _);
        let posted = unsafe {
            IsWindow(helper).as_bool()
                && PostMessageW(helper, WM_APP_REVIVE_READY, WPARAM(0), LPARAM(0)).is_ok()
        };
        if posted {
            return;
        }
    }
    // Fallback: thread message straight to the message loop.
    let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
    let posted = thread_id != 0
        && unsafe {
            PostThreadMessageW(thread_id, WM_APP_REVIVE_READY, WPARAM(0), LPARAM(0)).is_ok()
        };
    if !posted {
        // The UI thread is unreachable; clear the in-flight flag so the
        // watchdog re-detects the problem and can fall back to a relaunch.
        clear_reviving();
        diagnose::log("revival: unable to reach the UI thread with the ready signal");
    }
}

/// Ask the UI thread to perform the deliberate-quit cleanup without relying
/// on the current embedded window handle, which revival may replace while an
/// update is downloading. The hidden broadcast helper normally lives for the
/// whole process; the thread queue is the fallback if that window is gone.
fn request_process_quit() {
    let helper = BROADCAST_HELPER_HWND.load(Ordering::SeqCst);
    if helper != 0 {
        let helper = HWND(helper as *mut _);
        let posted = unsafe {
            IsWindow(helper).as_bool()
                && PostMessageW(helper, WM_APP_REQUEST_QUIT, WPARAM(0), LPARAM(0)).is_ok()
        };
        if posted {
            return;
        }
    }

    let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
    let posted = thread_id != 0
        && unsafe {
            PostThreadMessageW(thread_id, WM_APP_REQUEST_QUIT, WPARAM(0), LPARAM(0)).is_ok()
        };
    if posted {
        return;
    }

    // The helper has already been launched and is waiting for this PID. If the
    // UI thread cannot be reached at all, process termination is the only way
    // to avoid stranding the helper until its timeout.
    diagnose::log("update quit request could not reach the UI thread; exiting directly");
    std::process::exit(0);
}

/// Second stage of revival, on the UI thread with no long waits: bring the
/// widget back after Explorer destroyed our window (or moved the taskbar out
/// from under us). The taskbar widget is never shown as a desktop popup: when
/// the shell is unavailable it stays hidden until a later shell event or the
/// watchdog can verify a successful re-attachment.
unsafe fn revive_execute() {
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        clear_reviving();
        return;
    }

    let (existing_hwnd, preferred_taskbar_index, widget_visible) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd.to_hwnd(),
                s.preferred_taskbar_index,
                s.widget_visible,
            ),
            None => {
                clear_reviving();
                return;
            }
        }
    };

    let hwnd = if IsWindow(existing_hwnd).as_bool() {
        diagnose::log("revival: window still alive; re-attaching only");
        existing_hwnd
    } else {
        match recreate_widget_window() {
            Some(hwnd) => {
                diagnose::log(format!("revival: window recreated hwnd={:?}", hwnd));
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.hwnd = SendHwnd::from_hwnd(hwnd);
                    s.embedded = false;
                    s.taskbar_hwnd = None;
                    s.tray_notify_hwnd = None;
                }
                hwnd
            }
            None => {
                let attempt = REVIVE_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt >= REVIVE_CREATE_ATTEMPTS {
                    clear_reviving();
                    diagnose::log("revival: window creation failed repeatedly; relaunching");
                    relaunch_self();
                    // relaunch_self exits the process on success; reaching
                    // here means the spawn failed. Stay alive - the watchdog
                    // retries.
                    return;
                }
                diagnose::log(format!(
                    "revival: window creation attempt {attempt}/{REVIVE_CREATE_ATTEMPTS} failed; retrying in {}s",
                    REVIVE_CREATE_RETRY_DELAY.as_secs()
                ));
                // REVIVING stays true while the delayed retry is pending.
                std::thread::spawn(|| {
                    std::thread::sleep(REVIVE_CREATE_RETRY_DELAY);
                    post_revive_ready();
                });
                return;
            }
        }
    };

    // Prevent a transient desktop flash if SetParent must detach the old
    // taskbar child before the new taskbar is ready.
    let _ = ShowWindow(hwnd, SW_HIDE);
    if !attach_to_taskbar(hwnd, preferred_taskbar_index) {
        diagnose::log("revival: taskbar unavailable; keeping widget hidden");
        if let Err(error) = native_interop::detach_to_popup(hwnd) {
            diagnose::log(format!("revival detach from stale taskbar failed: {error}"));
        }
        let _ = ShowWindow(hwnd, SW_HIDE);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = false;
                s.taskbar_hwnd = None;
                s.tray_notify_hwnd = None;
            }
        }
        clear_reviving();
        return;
    }

    sync_tray_icons(hwnd);
    // Position and render before showing so the revived widget reappears in
    // place with content instead of flashing in and being moved.
    position_at_taskbar();
    render_layered();
    if widget_visible {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    // Provider polling is owned by the process-level broadcast helper and
    // therefore did not stop while this taskbar child was absent.
    schedule_countdown_timer();
    schedule_auto_update_check(hwnd);

    REVIVE_ATTEMPTS.store(0, Ordering::SeqCst);
    clear_reviving();
    diagnose::log("revival: complete");
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
struct DetailPopupState {
    title: String,
    providers: Vec<DetailProviderGroup>,
    status: String,
    version: String,
}

#[derive(Clone)]
struct DetailProviderGroup {
    kind: tray_icon::TrayIconKind,
    name: String,
    /// Compact status shown on the provider header. Tone carries product
    /// meaning independently from the localized copy.
    badge: Option<DetailBadge>,
    rows: Vec<DetailUsageRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailBadgeTone {
    Neutral,
    Degraded,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailBadge {
    text: String,
    tone: DetailBadgeTone,
}

#[derive(Clone)]
struct DetailUsageRow {
    window_label: String,
    /// None while no data exists for this window (shown as "--").
    percent: Option<f64>,
    reset_text: String,
    dividers: i32,
    warn: bool,
}

static DETAIL_STATE: Mutex<Option<DetailPopupState>> = Mutex::new(None);
static DETAIL_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
/// When the popup was last destroyed, for the reopen-as-toggle suppression.
static DETAIL_LAST_DISMISS: Mutex<Option<Instant>> = Mutex::new(None);
/// Which header button the mouse is over: 0 none, 1 refresh, 2 close, 3 move.
static DETAIL_HOVER: AtomicU8 = AtomicU8::new(0);
const DETAIL_HOVER_NONE: u8 = 0;
const DETAIL_HOVER_REFRESH: u8 = 1;
const DETAIL_HOVER_CLOSE: u8 = 2;
const DETAIL_HOVER_MOVE: u8 = 3;
/// The popup starts movable every time it opens. Locking only lasts for this
/// HWND's lifetime; its moved position is deliberately not persisted.
const DETAIL_DEFAULT_MOVEMENT_UNLOCKED: bool = true;
static DETAIL_MOVEMENT_UNLOCKED: AtomicBool = AtomicBool::new(DETAIL_DEFAULT_MOVEMENT_UNLOCKED);

#[derive(Clone, Debug, PartialEq, Eq)]
struct WidgetTooltipRow {
    window_label: String,
    percent_text: String,
    reset_text: String,
    warn: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WidgetTooltipSnapshot {
    kind: tray_icon::TrayIconKind,
    provider_name: String,
    rows: Vec<WidgetTooltipRow>,
}

#[derive(Default)]
struct WidgetTooltipRuntime {
    hover_kind: Option<tray_icon::TrayIconKind>,
    hits: Vec<BadgeHit>,
    snapshot: Option<WidgetTooltipSnapshot>,
}

static WIDGET_TOOLTIP_RUNTIME: Mutex<WidgetTooltipRuntime> = Mutex::new(WidgetTooltipRuntime {
    hover_kind: None,
    hits: Vec::new(),
    snapshot: None,
});
static WIDGET_TOOLTIP_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

fn lock_detail_state() -> MutexGuard<'static, Option<DetailPopupState>> {
    DETAIL_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_widget_tooltip_runtime() -> MutexGuard<'static, WidgetTooltipRuntime> {
    WIDGET_TOOLTIP_RUNTIME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

const USAGE_CACHE_MAX_AGE_SECS: u64 = 48 * 60 * 60;

fn usage_cache_path() -> PathBuf {
    settings::app_data_file("usage-cache.json")
}

/// Snapshot of the last successful poll, persisted so a restart can show the
/// previous numbers immediately instead of "--" until the first poll lands.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheWindow {
    percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_label: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    windows: Vec<UsageCacheWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<UsageCacheWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekly: Option<UsageCacheWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageCacheFile {
    saved_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_code: Option<UsageCacheProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex: Option<UsageCacheProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    antigravity: Option<UsageCacheProvider>,
}

fn usage_window_to_cache(window: &UsageWindow) -> UsageCacheWindow {
    UsageCacheWindow {
        percent: window.percentage,
        resets_unix: window
            .resets_at
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        duration_seconds: window.duration_seconds,
        source_label: window.source_label.clone(),
    }
}

fn usage_window_from_cache(window: &UsageCacheWindow) -> UsageWindow {
    UsageWindow {
        // The file is user-writable: a corrupt-but-parseable value must not
        // panic at startup (SystemTime + Duration panics on overflow) or
        // paint absurd percentages.
        percentage: window.percent.clamp(0.0, 100.0),
        resets_at: window
            .resets_unix
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
        duration_seconds: window.duration_seconds,
        source_label: window.source_label.clone(),
    }
}

fn usage_provider_to_cache(usage: &UsageData, updated_unix: Option<u64>) -> UsageCacheProvider {
    UsageCacheProvider {
        updated_unix,
        windows: usage.windows.iter().map(usage_window_to_cache).collect(),
        session: None,
        weekly: None,
    }
}

fn usage_provider_from_cache(provider: &UsageCacheProvider) -> UsageData {
    if !provider.windows.is_empty() {
        return UsageData::from_windows(
            provider
                .windows
                .iter()
                .map(usage_window_from_cache)
                .collect(),
        );
    }

    // Migrate the v2.0 cache shape. A zero/no-reset legacy section was the old
    // representation of a missing window, so do not recreate that ghost row.
    let mut windows = Vec::new();
    for (legacy, duration_seconds) in [
        (provider.session.as_ref(), FIVE_HOURS_SECONDS),
        (provider.weekly.as_ref(), ONE_WEEK_SECONDS),
    ] {
        if let Some(legacy) = legacy {
            if legacy.percent != 0.0 || legacy.resets_unix.is_some() {
                let mut window = usage_window_from_cache(legacy);
                window.duration_seconds = Some(duration_seconds);
                windows.push(window);
            }
        }
    }
    UsageData::from_windows(windows)
}

fn fresh_cached_provider(
    provider: Option<&UsageCacheProvider>,
    saved_unix: u64,
    now_unix: u64,
) -> Option<(UsageData, u64)> {
    provider.and_then(|provider| {
        let updated_unix = provider.updated_unix.unwrap_or(saved_unix);
        (now_unix.saturating_sub(updated_unix) <= USAGE_CACHE_MAX_AGE_SECS)
            .then(|| (usage_provider_from_cache(provider), updated_unix))
    })
}

fn save_usage_cache(data: &AppUsageData) {
    let file = UsageCacheFile {
        saved_unix: now_unix_secs(),
        claude_code: data
            .claude_code
            .as_ref()
            .map(|usage| usage_provider_to_cache(usage, data.claude_code_updated_unix)),
        codex: data
            .codex
            .as_ref()
            .map(|usage| usage_provider_to_cache(usage, data.codex_updated_unix)),
        antigravity: data
            .antigravity
            .as_ref()
            .map(|usage| usage_provider_to_cache(usage, data.antigravity_updated_unix)),
    };
    let path = usage_cache_path();
    match serde_json::to_string(&file) {
        Ok(json) => {
            if let Err(error) = settings::write_file_atomic(&path, &json) {
                diagnose::log_error(
                    &format!("usage cache write failed path={}", path.display()),
                    error,
                );
            }
        }
        Err(error) => diagnose::log_error("usage cache serialization failed", error),
    }
}

fn load_usage_cache() -> Option<(AppUsageData, u64)> {
    let path = usage_cache_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            diagnose::log_error(
                &format!("usage cache read failed path={}", path.display()),
                error,
            );
            return None;
        }
    };
    let file: UsageCacheFile = match serde_json::from_str(&content) {
        Ok(file) => file,
        Err(error) => {
            diagnose::log_error(
                &format!("usage cache parse failed path={}", path.display()),
                error,
            );
            return None;
        }
    };
    let now = now_unix_secs();
    if now.saturating_sub(file.saved_unix) > USAGE_CACHE_MAX_AGE_SECS {
        return None;
    }
    let claude_code = fresh_cached_provider(file.claude_code.as_ref(), file.saved_unix, now);
    let codex = fresh_cached_provider(file.codex.as_ref(), file.saved_unix, now);
    let antigravity = fresh_cached_provider(file.antigravity.as_ref(), file.saved_unix, now);
    let data = AppUsageData {
        claude_code: claude_code.as_ref().map(|(usage, _)| usage.clone()),
        codex: codex.as_ref().map(|(usage, _)| usage.clone()),
        antigravity: antigravity.as_ref().map(|(usage, _)| usage.clone()),
        claude_code_updated_unix: claude_code.as_ref().map(|(_, updated_unix)| *updated_unix),
        codex_updated_unix: codex.as_ref().map(|(_, updated_unix)| *updated_unix),
        antigravity_updated_unix: antigravity.as_ref().map(|(_, updated_unix)| *updated_unix),
        ..Default::default()
    };
    if data.claude_code.is_none() && data.codex.is_none() && data.antigravity.is_none() {
        return None;
    }
    let last_success_unix = [
        data.claude_code_updated_unix,
        data.codex_updated_unix,
        data.antigravity_updated_unix,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(file.saved_unix);
    Some((data, last_success_unix))
}

fn save_state_settings() {
    let snapshot = {
        let state = lock_state();
        state.as_ref().map(|s| SettingsFile {
            tray_offset: s.preferred_tray_offset,
            taskbar_index: s.preferred_taskbar_index,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            floating_visible: s.floating_visible,
            detailed_tray_icons: s.detailed_tray_icons,
            floating_x: s.floating_x,
            floating_y: s.floating_y,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            show_antigravity: s.show_antigravity,
            provider_order: s.provider_order.clone(),
            notify_session_reset: s.notify_session_reset,
            notify_weekly_reset: s.notify_weekly_reset,
        })
    };
    if let Some(snapshot) = snapshot {
        if let Err(error) = settings::save(&snapshot) {
            diagnose::log_error("settings save failed", error);
        }
    }
}

const TRAY_TOOLTIP_MAX_UTF16: usize = 127;

fn truncate_utf16_with_ellipsis(text: &str, max_units: usize) -> String {
    if text.encode_utf16().count() <= max_units {
        return text.to_string();
    }
    if max_units == 0 {
        return String::new();
    }

    let content_units = max_units.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let units = ch.len_utf16();
        if used + units > content_units {
            break;
        }
        result.push(ch);
        used += units;
    }
    result.push('…');
    result
}

fn tray_tooltip_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut result = String::new();
    for line in lines {
        let separator_units = usize::from(!result.is_empty());
        let used = result.encode_utf16().count();
        let remaining = TRAY_TOOLTIP_MAX_UTF16.saturating_sub(used + separator_units);
        if remaining == 0 {
            break;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&truncate_utf16_with_ellipsis(line, remaining));
        if result.encode_utf16().count() >= TRAY_TOOLTIP_MAX_UTF16 {
            break;
        }
    }
    result
}

fn provider_tooltip_lines<'a>(
    provider_name: &str,
    windows: impl IntoIterator<Item = &'a UsageWindow>,
    strings: Strings,
) -> Vec<String> {
    let mut lines = vec![provider_name.to_string()];
    for window in windows {
        let mut line = format!(
            "{}: {:.0}%",
            usage_window_label(window, strings),
            window.percentage.clamp(0.0, 100.0)
        );
        if let Some(resets_at) = window.resets_at {
            line.push_str(" (");
            line.push_str(&detail_reset_line(resets_at, strings, false));
            line.push(')');
        }
        lines.push(line);
    }
    if lines.len() == 1 {
        lines.push("--".to_string());
    }
    lines
}

fn provider_tooltip(provider_name: &str, usage: Option<&UsageData>, strings: Strings) -> String {
    let windows = usage
        .filter(|usage| !usage.is_empty())
        .map(compact_view::selected_usage_windows)
        .unwrap_or_default();
    let lines = provider_tooltip_lines(provider_name, windows, strings);
    tray_tooltip_from_lines(lines.iter().map(String::as_str))
}

fn widget_tooltip_reset_text(resets_at: SystemTime, strings: Strings) -> String {
    match resets_at.duration_since(SystemTime::now()) {
        Ok(duration) if duration.as_secs() > 0 => strings
            .detail_resets_in
            .replace("{duration}", &detail_duration_text(duration, strings)),
        _ => strings.detail_resets_now.to_string(),
    }
}

fn widget_tooltip_snapshot(kind: tray_icon::TrayIconKind) -> WidgetTooltipSnapshot {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return WidgetTooltipSnapshot {
            kind,
            provider_name: String::new(),
            rows: Vec::new(),
        };
    };
    let strings = s.language.strings();
    let provider_has_values = s
        .compact_vm
        .providers
        .iter()
        .any(|provider| provider.kind == kind && !provider.windows.is_empty());
    let (provider_name, usage) = match kind {
        tray_icon::TrayIconKind::Claude => (
            strings.claude_code_model,
            provider_has_values
                .then(|| s.data.as_ref().and_then(|data| data.claude_code.as_ref()))
                .flatten(),
        ),
        tray_icon::TrayIconKind::Codex => (
            strings.codex_model,
            provider_has_values
                .then(|| s.data.as_ref().and_then(|data| data.codex.as_ref()))
                .flatten(),
        ),
        tray_icon::TrayIconKind::Antigravity => (
            strings.antigravity_model,
            provider_has_values
                .then(|| s.data.as_ref().and_then(|data| data.antigravity.as_ref()))
                .flatten(),
        ),
    };
    let mut rows = usage
        .filter(|usage| !usage.is_empty())
        .into_iter()
        .flat_map(|usage| usage.windows.iter())
        .map(|window| {
            let display_percent = compact_view::display_percent(window.percentage);
            WidgetTooltipRow {
                window_label: usage_window_label(window, strings),
                percent_text: format!("{display_percent}%"),
                reset_text: window
                    .resets_at
                    .map(|resets_at| widget_tooltip_reset_text(resets_at, strings))
                    .unwrap_or_else(|| strings.detail_reset_unavailable.to_string()),
                warn: display_percent >= compact_view::WARN_THRESHOLD_PERCENT,
            }
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        rows.push(WidgetTooltipRow {
            window_label: "--".to_string(),
            percent_text: String::new(),
            reset_text: strings.detail_waiting.to_string(),
            warn: false,
        });
    }

    WidgetTooltipSnapshot {
        kind,
        provider_name: provider_name.to_string(),
        rows,
    }
}

fn app_tooltip_provider_line(
    provider_name: &str,
    usage: Option<&UsageData>,
    strings: Strings,
) -> String {
    let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
        return format!("{provider_name}: --");
    };
    let windows = compact_view::selected_usage_windows(usage)
        .into_iter()
        .map(|window| {
            format!(
                "{} {:.0}%",
                usage_window_label(window, strings),
                window.percentage.clamp(0.0, 100.0)
            )
        })
        .collect::<Vec<_>>();
    format!("{provider_name}: {}", windows.join(" · "))
}

fn provider_tray_icon(
    kind: tray_icon::TrayIconKind,
    provider_name: &str,
    usage: Option<&UsageData>,
    widget: &ProviderWidgetData,
    strings: Strings,
) -> tray_icon::TrayIconData {
    tray_icon::TrayIconData {
        kind,
        percents: widget
            .windows
            .iter()
            .filter_map(|window| window.percent)
            .collect(),
        tooltip: provider_tooltip(provider_name, usage, strings),
    }
}

fn tray_icon_data_from_state() -> (Vec<tray_icon::TrayIconData>, bool, String) {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return (
            Vec::new(),
            true,
            LanguageId::English.strings().window_title.to_string(),
        );
    };
    let strings = s.language.strings();
    let empty = ProviderWidgetData::default();
    let mut icons = Vec::new();
    let mut app_tooltip_lines = vec![strings.window_title.to_string()];
    let data = s.last_poll_ok.then_some(s.data.as_ref()).flatten();
    if s.show_claude_code {
        let usage = data.and_then(|data| data.claude_code.as_ref());
        icons.push(provider_tray_icon(
            tray_icon::TrayIconKind::Claude,
            strings.claude_code_model,
            usage,
            if s.last_poll_ok {
                &s.claude_widget
            } else {
                &empty
            },
            strings,
        ));
        app_tooltip_lines.push(app_tooltip_provider_line(
            strings.claude_code_model,
            usage,
            strings,
        ));
    }
    if s.show_codex {
        let usage = data.and_then(|data| data.codex.as_ref());
        icons.push(provider_tray_icon(
            tray_icon::TrayIconKind::Codex,
            strings.codex_window_title,
            usage,
            if s.last_poll_ok {
                &s.codex_widget
            } else {
                &empty
            },
            strings,
        ));
        app_tooltip_lines.push(app_tooltip_provider_line(
            strings.codex_model,
            usage,
            strings,
        ));
    }
    if s.show_antigravity {
        let usage = data.and_then(|data| data.antigravity.as_ref());
        icons.push(provider_tray_icon(
            tray_icon::TrayIconKind::Antigravity,
            strings.antigravity_window_title,
            usage,
            if s.last_poll_ok {
                &s.antigravity_widget
            } else {
                &empty
            },
            strings,
        ));
        app_tooltip_lines.push(app_tooltip_provider_line(
            strings.antigravity_model,
            usage,
            strings,
        ));
    }
    let app_tooltip = tray_tooltip_from_lines(app_tooltip_lines.iter().map(String::as_str));
    (icons, s.detailed_tray_icons, app_tooltip)
}

fn sync_tray_icons(hwnd: HWND) {
    let (icons, detailed_icons, app_tooltip) = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons, detailed_icons, &app_tooltip);
}

fn enabled_provider_kinds(state: &AppState) -> Vec<tray_icon::TrayIconKind> {
    default_provider_order()
        .into_iter()
        .filter(|kind| match kind {
            tray_icon::TrayIconKind::Claude => state.show_claude_code,
            tray_icon::TrayIconKind::Codex => state.show_codex,
            tray_icon::TrayIconKind::Antigravity => state.show_antigravity,
        })
        .collect()
}

/// Replace only the relative slots occupied by currently visible providers.
/// Hidden providers keep their previous slot so toggling one back on does not
/// arbitrarily move the other providers.
fn merge_visible_provider_order(
    full_order: &[tray_icon::TrayIconKind],
    visible_order: &[tray_icon::TrayIconKind],
) -> Vec<tray_icon::TrayIconKind> {
    let mut visible = visible_order.iter().copied();
    full_order
        .iter()
        .map(|kind| {
            if visible_order.contains(kind) {
                visible.next().unwrap_or(*kind)
            } else {
                *kind
            }
        })
        .collect()
}

fn reset_pending_provider_order() {
    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.pending_provider_order = None;
        s.pending_provider_order_samples = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderOrderObservation {
    Current,
    Pending,
    Apply,
}

fn observe_provider_order_candidate(
    current: &[tray_icon::TrayIconKind],
    candidate: &[tray_icon::TrayIconKind],
    pending: &mut Option<Vec<tray_icon::TrayIconKind>>,
    samples: &mut u8,
) -> ProviderOrderObservation {
    if candidate == current {
        *pending = None;
        *samples = 0;
        return ProviderOrderObservation::Current;
    }

    if pending.as_deref() == Some(candidate) {
        *samples = samples.saturating_add(1);
    } else {
        *pending = Some(candidate.to_vec());
        *samples = 1;
    }

    if *samples >= TRAY_ORDER_STABLE_SAMPLES {
        *pending = None;
        *samples = 0;
        ProviderOrderObservation::Apply
    } else {
        ProviderOrderObservation::Pending
    }
}

/// Sample the public Shell rectangles for this app's active tray icons. A new
/// order must be observed twice before it changes the compact surfaces. The
/// first observation arms a short one-shot confirmation timer, so an actual
/// drag settles in about 120ms while transient Explorer layouts still cannot
/// make the UI flicker.
fn refresh_provider_order_from_tray(hwnd: HWND) -> bool {
    let (taskbar_hwnd, enabled, current_order, detailed_icons) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return false;
        };
        (
            s.taskbar_hwnd,
            enabled_provider_kinds(s),
            s.provider_order.clone(),
            s.detailed_tray_icons,
        )
    };

    if !detailed_icons || enabled.len() <= 1 {
        reset_pending_provider_order();
        return false;
    }
    let Some(taskbar_rect) = taskbar_hwnd.and_then(native_interop::get_taskbar_rect) else {
        reset_pending_provider_order();
        return false;
    };
    let Some(visible_order) = tray_icon::visible_order(hwnd, &enabled, &taskbar_rect) else {
        reset_pending_provider_order();
        return false;
    };
    let candidate = merge_visible_provider_order(&current_order, &visible_order);

    let (applied, confirm) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return false;
        };
        if s.last_observed_tray_order.as_ref() != Some(&visible_order) {
            diagnose::log(format!(
                "tray provider order observed visible={visible_order:?}"
            ));
            s.last_observed_tray_order = Some(visible_order.clone());
        }
        match observe_provider_order_candidate(
            &s.provider_order,
            &candidate,
            &mut s.pending_provider_order,
            &mut s.pending_provider_order_samples,
        ) {
            ProviderOrderObservation::Current => (false, false),
            ProviderOrderObservation::Pending => {
                diagnose::log(format!(
                    "tray provider order candidate visible={visible_order:?} full={candidate:?}"
                ));
                (false, true)
            }
            ProviderOrderObservation::Apply => {
                s.provider_order = candidate.clone();
                (true, false)
            }
        }
    };

    unsafe {
        if confirm {
            if SetTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM, TRAY_ORDER_CONFIRM_MS, None) == 0 {
                diagnose::log("tray provider order confirmation timer failed");
            }
        } else if applied {
            let _ = KillTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM);
        }
    }

    if applied {
        diagnose::log(format!("tray provider order applied full={candidate:?}"));
        position_at_taskbar();
        render_layered();
        refresh_floating_monitor(false);
        // Persist after both visible surfaces have updated; a slow filesystem
        // must never delay the user's drag feedback.
        save_state_settings();
    }
    applied
}

fn toggle_widget_visibility(hwnd: HWND) {
    let (new_visible, embedded) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            (s.widget_visible, s.embedded)
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible && embedded {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else if new_visible {
            let _ = ShowWindow(hwnd, SW_HIDE);
            revive_request();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn attach_to_taskbar(hwnd: HWND, requested_index: usize) -> bool {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        diagnose::log("taskbar not found; taskbar widget remains hidden");
        return false;
    }

    let index = requested_index.min(taskbars.len().saturating_sub(1));
    let taskbar = taskbars[index];
    diagnose::log(format!(
        "taskbar selected index={index} count={} hwnd={:?} rect=({}, {}, {}, {})",
        taskbars.len(),
        taskbar.hwnd,
        taskbar.rect.left,
        taskbar.rect.top,
        taskbar.rect.right,
        taskbar.rect.bottom
    ));

    let old_hook = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| s.win_event_hook.take())
    };
    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook);
    }

    if let Err(error) = native_interop::embed_in_taskbar(hwnd, taskbar.hwnd) {
        diagnose::log(format!(
            "taskbar embedding failed; keeping widget hidden: {error}"
        ));
        if let Err(detach_error) = native_interop::detach_to_popup(hwnd) {
            diagnose::log(format!("detach after embed error failed: {detach_error}"));
        }
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.taskbar_hwnd = None;
            s.tray_notify_hwnd = None;
            s.win_event_hook = None;
            s.embedded = false;
        }
        return false;
    }

    let tray_notify = native_interop::find_child_window(taskbar.hwnd, "TrayNotifyWnd");
    if tray_notify.is_some() {
        diagnose::log("TrayNotifyWnd found");
    } else {
        diagnose::log("TrayNotifyWnd not found");
    }

    let hook = tray_notify.and_then(|tray_hwnd| {
        let thread_id = native_interop::get_window_thread_id(tray_hwnd);
        native_interop::set_tray_event_hook(thread_id, on_tray_location_changed)
    });
    if hook.is_some() {
        diagnose::log("tray event hook installed");
    } else {
        diagnose::log("tray event hook could not be installed");
    }

    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.taskbar_hwnd = Some(taskbar.hwnd);
        s.tray_notify_hwnd = tray_notify;
        s.win_event_hook = hook;
        s.taskbar_index = index;
        s.embedded = true;
    }
    true
}

fn taskbar_at_point(pt: POINT) -> Option<(usize, native_interop::TaskbarWindow)> {
    native_interop::find_taskbars()
        .into_iter()
        .enumerate()
        .find(|(_, taskbar)| {
            pt.x >= taskbar.rect.left
                && pt.x < taskbar.rect.right
                && pt.y >= taskbar.rect.top
                && pt.y < taskbar.rect.bottom
        })
}

fn primary_taskbar_index() -> usize {
    native_interop::find_taskbars()
        .iter()
        .position(|taskbar| unsafe {
            let mut class_name = [0u16; 64];
            let len = GetClassNameW(taskbar.hwnd, &mut class_name);
            len > 0 && String::from_utf16_lossy(&class_name[..len as usize]) == "Shell_TrayWnd"
        })
        .unwrap_or(0)
}

fn tray_left_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT) -> i32 {
    let mut tray_left = taskbar_rect.right;
    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }
    tray_left
}

fn clamp_offset_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT, offset: i32) -> i32 {
    let _dpi_scope = DpiScope::for_window(taskbar_hwnd);
    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
    let max_offset = (tray_left - taskbar_rect.left - total_widget_width()).max(0);
    offset.clamp(0, max_offset)
}

fn offset_for_drop_point(
    taskbar_hwnd: HWND,
    taskbar_rect: RECT,
    pt: POINT,
    drag_start_client_x: i32,
) -> i32 {
    let _dpi_scope = DpiScope::for_window(taskbar_hwnd);
    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
    let desired_left = pt.x - taskbar_rect.left - drag_start_client_x;
    let offset = tray_left - taskbar_rect.left - total_widget_width() - desired_left;
    clamp_offset_for_taskbar(taskbar_hwnd, taskbar_rect, offset)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    if !updater::update_channel_configured() {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        }
        return;
    }
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn approximately(actual: u64, expected: u64) -> bool {
    actual >= expected.saturating_mul(95) / 100 && actual <= expected.saturating_mul(105) / 100
}

fn usage_window_label(window: &UsageWindow, strings: Strings) -> String {
    if let Some(seconds) = window.duration_seconds {
        if approximately(seconds, 5 * 60 * 60) {
            return strings.session_window.to_string();
        }
        if approximately(seconds, 24 * 60 * 60) {
            return "1d".to_string();
        }
        if approximately(seconds, 7 * 24 * 60 * 60) {
            return strings.weekly_window.to_string();
        }
        if approximately(seconds, 30 * 24 * 60 * 60) {
            return "30d".to_string();
        }
        if approximately(seconds, 365 * 24 * 60 * 60) {
            return "365d".to_string();
        }
        if seconds % (24 * 60 * 60) == 0 {
            return format!("{}d", seconds / (24 * 60 * 60));
        }
        if seconds % (60 * 60) == 0 {
            return format!("{}h", seconds / (60 * 60));
        }
        if seconds % 60 == 0 {
            return format!("{}m", seconds / 60);
        }
    }

    window
        .source_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(8).collect())
        .unwrap_or_else(|| strings.quota_window.to_string())
}

fn usage_window_dividers(window: &UsageWindow) -> i32 {
    let Some(seconds) = window.duration_seconds else {
        return 1;
    };
    let units = if seconds <= 24 * 60 * 60 && seconds % (60 * 60) == 0 {
        seconds / (60 * 60)
    } else if seconds % (24 * 60 * 60) == 0 {
        seconds / (24 * 60 * 60)
    } else {
        1
    };
    units.clamp(1, 10) as i32
}

fn placeholder_widget() -> ProviderWidgetData {
    ProviderWidgetData {
        windows: vec![WidgetUsageWindow { percent: None }],
    }
}

fn provider_widget_from_usage(usage: Option<&UsageData>) -> ProviderWidgetData {
    let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
        return placeholder_widget();
    };

    ProviderWidgetData {
        windows: compact_view::selected_usage_windows(usage)
            .into_iter()
            .map(|window| WidgetUsageWindow {
                percent: Some(window.percentage.clamp(0.0, 100.0)),
            })
            .collect(),
    }
}

fn set_widget_placeholders(state: &mut AppState, text: &str) {
    state.claude_widget = placeholder_widget();
    state.codex_widget = placeholder_widget();
    state.antigravity_widget = placeholder_widget();
    let compact_text = if text == "!" { "--" } else { text };
    state.compact_vm = compact_view::placeholder_model(
        compact_text,
        &state.provider_order,
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
    );
    if text == "!" {
        mark_all_compact_providers_error(&mut state.compact_vm);
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let data = state.data.as_ref();
    state.claude_widget =
        provider_widget_from_usage(data.and_then(|data| data.claude_code.as_ref()));
    state.codex_widget = provider_widget_from_usage(data.and_then(|data| data.codex.as_ref()));
    state.antigravity_widget =
        provider_widget_from_usage(data.and_then(|data| data.antigravity.as_ref()));
    state.compact_vm = compact_view::build(
        data,
        strings,
        &state.provider_order,
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
    );
    if state.last_error.is_some() {
        mark_all_compact_providers_error(&mut state.compact_vm);
    }
}

fn mark_all_compact_providers_error(vm: &mut CompactViewModel) {
    for provider in &mut vm.providers {
        provider.attention = compact_view::Attention::Error;
    }
}

fn merge_missing_provider_data(
    previous: Option<&AppUsageData>,
    mut next: AppUsageData,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> AppUsageData {
    if let Some(previous) = previous {
        if show_claude_code && next.claude_code.is_none() {
            next.claude_code = previous.claude_code.clone();
            next.claude_code_updated_unix = previous.claude_code_updated_unix;
        }
        if show_codex && next.codex.is_none() {
            next.codex = previous.codex.clone();
            next.codex_updated_unix = previous.codex_updated_unix;
        }
        if show_antigravity && next.antigravity.is_none() {
            next.antigravity = previous.antigravity.clone();
            next.antigravity_updated_unix = previous.antigravity_updated_unix;
        }
    }
    next
}

fn stamp_provider_updates(data: &mut AppUsageData, updated_unix: u64) {
    if data.claude_code.is_some() && data.claude_code_error.is_none() {
        data.claude_code_updated_unix = Some(updated_unix);
    }
    if data.codex.is_some() && data.codex_error.is_none() {
        data.codex_updated_unix = Some(updated_unix);
    }
    if data.antigravity.is_some() && data.antigravity_error.is_none() {
        data.antigravity_updated_unix = Some(updated_unix);
    }
}

#[derive(Clone)]
struct ResetNotification {
    kind: tray_icon::TrayIconKind,
    title: String,
    body: String,
}

fn collect_reset_notifications(
    previous: Option<&AppUsageData>,
    next: &AppUsageData,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    strings: Strings,
) -> Vec<ResetNotification> {
    if !notify_session_reset && !notify_weekly_reset {
        return Vec::new();
    }
    let Some(previous) = previous else {
        return Vec::new();
    };

    let mut notifications = Vec::new();
    push_provider_reset_notifications(
        &mut notifications,
        tray_icon::TrayIconKind::Claude,
        strings.claude_code_model,
        previous.claude_code.as_ref(),
        next.claude_code.as_ref(),
        notify_session_reset,
        notify_weekly_reset,
        strings,
    );
    push_provider_reset_notifications(
        &mut notifications,
        tray_icon::TrayIconKind::Codex,
        strings.codex_model,
        previous.codex.as_ref(),
        next.codex.as_ref(),
        notify_session_reset,
        notify_weekly_reset,
        strings,
    );
    push_provider_reset_notifications(
        &mut notifications,
        tray_icon::TrayIconKind::Antigravity,
        strings.antigravity_model,
        previous.antigravity.as_ref(),
        next.antigravity.as_ref(),
        notify_session_reset,
        notify_weekly_reset,
        strings,
    );
    notifications
}

// Keeping the provider/reset inputs explicit makes the notification policy
// auditable; wrapping them in a one-use options object would add indirection.
#[allow(clippy::too_many_arguments)]
fn push_provider_reset_notifications(
    notifications: &mut Vec<ResetNotification>,
    kind: tray_icon::TrayIconKind,
    provider_label: &str,
    previous: Option<&UsageData>,
    next: Option<&UsageData>,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    strings: Strings,
) {
    let (Some(previous), Some(next)) = (previous, next) else {
        return;
    };

    for next_window in &next.windows {
        let Some(previous_window) = previous
            .windows
            .iter()
            .find(|previous_window| same_usage_window(previous_window, next_window))
        else {
            continue;
        };
        let enabled = if is_long_usage_window(next_window) {
            notify_weekly_reset
        } else {
            notify_session_reset
        };
        if enabled && reset_window_refreshed(previous_window, next_window) {
            notifications.push(make_reset_notification(
                kind,
                provider_label,
                &usage_window_label(next_window, strings),
                strings,
            ));
        }
    }
}

fn same_usage_window(left: &UsageWindow, right: &UsageWindow) -> bool {
    match (left.duration_seconds, right.duration_seconds) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left.source_label.as_deref() == right.source_label.as_deref(),
        _ => false,
    }
}

fn is_long_usage_window(window: &UsageWindow) -> bool {
    window
        .duration_seconds
        .is_some_and(|seconds| seconds >= 6 * 24 * 60 * 60)
}

fn reset_window_refreshed(previous: &UsageWindow, next: &UsageWindow) -> bool {
    let (Some(previous_reset), Some(next_reset)) = (previous.resets_at, next.resets_at) else {
        return false;
    };

    SystemTime::now().duration_since(previous_reset).is_ok()
        && next_reset != previous_reset
        && next_reset.duration_since(previous_reset).is_ok()
}

fn make_reset_notification(
    kind: tray_icon::TrayIconKind,
    provider_label: &str,
    window_label: &str,
    strings: Strings,
) -> ResetNotification {
    let body = strings
        .reset_notification_body
        .replace("{provider}", provider_label)
        .replace("{window}", window_label);
    ResetNotification {
        kind,
        title: strings.reset_notification_title.to_string(),
        body,
    }
}
fn rate_limit_retry_ms(retry_after_ms: Option<u32>, poll_interval_ms: u32) -> u32 {
    let requested = retry_after_ms.unwrap_or_else(|| poll_interval_ms.max(RATE_LIMIT_MIN_RETRY_MS));
    requested
        .max(poll_interval_ms)
        .clamp(RATE_LIMIT_MIN_RETRY_MS, RATE_LIMIT_MAX_RETRY_MS)
}

fn credential_watch_mode_for_failure(
    error: poller::PollError,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> Option<poller::CredentialWatchMode> {
    if !matches!(
        error,
        poller::PollError::AuthRequired
            | poller::PollError::TokenExpired
            | poller::PollError::NoCredentials
    ) {
        return None;
    }

    let enabled_count = show_claude_code as u8 + show_codex as u8 + show_antigravity as u8;
    if enabled_count > 1 {
        return Some(poller::CredentialWatchMode::AllProviders);
    }
    if show_codex {
        return Some(poller::CredentialWatchMode::Codex);
    }
    if show_antigravity {
        return Some(poller::CredentialWatchMode::Antigravity);
    }
    if show_claude_code && error == poller::PollError::NoCredentials {
        Some(poller::CredentialWatchMode::AllSources)
    } else {
        Some(poller::CredentialWatchMode::ActiveSource)
    }
}
fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Exit the process deliberately from the UI thread.
///
/// Update workers reach this through the process-level `WM_APP_REQUEST_QUIT`
/// channel; menu Exit and a normal `WM_CLOSE` call it directly. Mark the quit
/// before ending the message loop so a concurrent window destruction
/// can never be mistaken for an explorer-triggered teardown and revived.
unsafe fn request_quit(hwnd: HWND) {
    if QUIT_REQUESTED.swap(true, Ordering::SeqCst) {
        return;
    }

    diagnose::log("deliberate quit requested");
    let (hook, detail_hwnd, floating_hwnd) = {
        let mut state = lock_state();
        match state.as_mut() {
            Some(s) => (s.win_event_hook.take(), s.details_hwnd, s.floating_hwnd),
            None => (None, None, None),
        }
    };
    if let Some(hook) = hook {
        native_interop::unhook_win_event(hook);
    }
    if let Some(detail_hwnd) = detail_hwnd {
        let _ = DestroyWindow(detail_hwnd);
    }
    if let Some(floating_hwnd) = floating_hwnd {
        let _ = DestroyWindow(floating_hwnd);
    }

    if hwnd == HWND::default() || !IsWindow(hwnd).as_bool() {
        diagnose::log("deliberate quit: main window unavailable; ending message loop directly");
        PostQuitMessage(0);
        return;
    }
    if let Err(error) = DestroyWindow(hwnd) {
        diagnose::log_error(
            "deliberate quit: failed to destroy main window; ending message loop directly",
            error,
        );
        PostQuitMessage(0);
    }
}

/// Reset every provider's text to the loading placeholder and kick off a
/// poll. Shared by the context-menu Refresh entry and the detail popup's
/// refresh button.
fn trigger_manual_refresh(_hwnd: HWND) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            set_widget_placeholders(s, "...");
            s.force_notify_auth_error = true;
        }
    }
    render_layered();
    refresh_floating_monitor(false);
    request_poll_with(true);
}

fn show_usage_details(_tray_hwnd: HWND) {
    // Clicking the tray icon (or the widget) while the popup is open first
    // dismisses it via focus loss, then delivers this open request. Treat an
    // open that lands right after a dismissal as a toggle-close instead of
    // flickering the popup shut and open again.
    {
        let last_dismiss = DETAIL_LAST_DISMISS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *last_dismiss {
            if at.elapsed().as_millis() < DETAIL_REOPEN_SUPPRESS_MS {
                diagnose::log("detail popup: open request treated as toggle-close");
                return;
            }
        }
    }

    diagnose::log("detail popup: open requested");
    let snapshot = detail_popup_snapshot();
    let title = snapshot.title.clone();

    {
        let mut detail_state = lock_detail_state();
        *detail_state = Some(snapshot.clone());
    }

    let existing = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };

    unsafe {
        if let Some(detail_hwnd) = existing {
            if IsWindow(detail_hwnd).as_bool() {
                let _dpi_scope = DpiScope::for_window(detail_hwnd);
                let (width, height) = detail_popup_size(&snapshot);
                let (x, y) = detail_popup_position(width, height);
                let _ = SetWindowPos(
                    detail_hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    width,
                    height,
                    SWP_SHOWWINDOW,
                );
                let _ = InvalidateRect(detail_hwnd, None, false);
                let _ = SetForegroundWindow(detail_hwnd);
                return;
            }
        }
    }

    if !ensure_detail_window_class() {
        return;
    }

    unsafe {
        // Provisional geometry is replaced with the new HWND's own monitor
        // DPI immediately after creation, before the popup is shown.
        let (width, height) = detail_popup_size(&snapshot);
        let (x, y) = detail_popup_position(width, height);
        DETAIL_MOVEMENT_UNLOCKED.store(DETAIL_DEFAULT_MOVEMENT_UNLOCKED, Ordering::SeqCst);
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("detail popup: GetModuleHandleW failed", error);
                return;
            }
        };
        let class_name = native_interop::wide_str(DETAIL_WINDOW_CLASS_NAME);
        let title_wide = native_interop::wide_str(&title);
        // Deliberately unowned. The main widget is a WS_CHILD embedded in the
        // taskbar, so passing it as owner would make Win32 resolve the owner
        // to its top-level ancestor - explorer's taskbar window. A popup owned
        // by a foreign process's window ties its lifetime and z-order to
        // explorer's whims; an unowned topmost tool window is self-contained
        // (Exit cleans it up explicitly, and WS_EX_TOOLWINDOW keeps it out of
        // the taskbar).
        let detail_hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            WS_POPUP,
            x,
            y,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("detail popup: CreateWindowExW failed", error);
                let mut detail_state = lock_detail_state();
                *detail_state = None;
                return;
            }
        };

        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.details_hwnd = Some(detail_hwnd);
            }
        }

        {
            let _dpi_scope = DpiScope::for_window(detail_hwnd);
            let (width, height) = detail_popup_size(&snapshot);
            let (x, y) = detail_popup_position(width, height);
            if let Err(error) = SetWindowPos(
                detail_hwnd,
                HWND_TOPMOST,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            ) {
                diagnose::log_error("detail popup: DPI-aware initial positioning failed", error);
            }
        }

        diagnose::log(format!("detail popup: created hwnd={:?}", detail_hwnd));
        if SetTimer(detail_hwnd, TIMER_DETAIL_REFRESH, DETAIL_REFRESH_MS, None) == 0 {
            diagnose::log("detail popup: unable to start live countdown timer");
        }
        // Rounded corners on Windows 11, matching native tray flyouts; a no-op
        // (harmless error) on Windows 10.
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            detail_hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        // Fade in like the native tray flyouts (WM_PRINTCLIENT is handled so
        // the blend has real content); fall back to a plain show on failure.
        // AW_ACTIVATE matters: without activation the popup never receives
        // WA_INACTIVE, so click-outside-to-dismiss would silently stop
        // working whenever SetForegroundWindow below is denied.
        if AnimateWindow(detail_hwnd, 120, AW_BLEND | AW_ACTIVATE).is_err() {
            let _ = ShowWindow(detail_hwnd, SW_SHOWNORMAL);
        }
        let _ = UpdateWindow(detail_hwnd);
        let _ = SetForegroundWindow(detail_hwnd);
    }
}

fn detail_popup_snapshot() -> DetailPopupState {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return DetailPopupState {
            title: "Gengchou".to_string(),
            providers: Vec::new(),
            status: LanguageId::English.strings().detail_waiting.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
    };

    let strings = s.language.strings();
    // A completely failed poll (every enabled provider) leaves no per-provider
    // error in `data`; classify from the recorded poll error instead. This
    // intentionally ignores last_poll_ok: a transient failure keeps the stale
    // numbers on display (last_poll_ok stays true), and these badges are then
    // the only visible signal that the data is not refreshing.
    let global_error = s.last_error.map(poller::provider_status);

    let mut providers = Vec::new();
    if s.show_claude_code {
        providers.push(detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            strings.claude_code_model,
            s.data.as_ref().and_then(|data| data.claude_code.as_ref()),
            s.data
                .as_ref()
                .and_then(|data| data.claude_code_error)
                .or(global_error),
            s.data_is_cached,
            strings,
        ));
    }
    if s.show_codex {
        providers.push(detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            strings.codex_model,
            s.data.as_ref().and_then(|data| data.codex.as_ref()),
            s.data
                .as_ref()
                .and_then(|data| data.codex_error)
                .or(global_error),
            s.data_is_cached,
            strings,
        ));
    }
    if s.show_antigravity {
        providers.push(detail_provider_group(
            tray_icon::TrayIconKind::Antigravity,
            strings.antigravity_model,
            s.data.as_ref().and_then(|data| data.antigravity.as_ref()),
            s.data
                .as_ref()
                .and_then(|data| data.antigravity_error)
                .or(global_error),
            s.data_is_cached,
            strings,
        ));
    }

    DetailPopupState {
        title: strings.window_title.to_string(),
        providers,
        status: detail_status_text(s, strings),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn detail_provider_group(
    kind: tray_icon::TrayIconKind,
    name: &str,
    usage: Option<&UsageData>,
    error: Option<ProviderStatus>,
    data_is_cached: bool,
    strings: Strings,
) -> DetailProviderGroup {
    let badge = match error {
        Some(ProviderStatus::AuthRequired) => Some(DetailBadge {
            text: strings.detail_auth_required.to_string(),
            tone: DetailBadgeTone::Critical,
        }),
        Some(ProviderStatus::RateLimited) => Some(DetailBadge {
            text: strings.detail_badge_rate_limited.to_string(),
            tone: DetailBadgeTone::Degraded,
        }),
        Some(ProviderStatus::RequestFailed) => Some(DetailBadge {
            text: strings.detail_badge_error.to_string(),
            tone: DetailBadgeTone::Degraded,
        }),
        None if usage.is_none_or(UsageData::is_empty) => Some(DetailBadge {
            text: strings.detail_badge_loading.to_string(),
            tone: DetailBadgeTone::Neutral,
        }),
        None => None,
    };

    let rows = match usage.filter(|usage| !usage.is_empty()) {
        Some(usage) => usage
            .windows
            .iter()
            .map(|window| {
                detail_usage_row(
                    compact_view::compact_usage_window_label(window, strings),
                    Some(window),
                    error,
                    usage_window_dividers(window),
                    strings,
                )
            })
            .collect(),
        None => vec![detail_usage_row("--".to_string(), None, error, 1, strings)],
    };

    let badge = badge.or_else(|| {
        if data_is_cached {
            Some(DetailBadge {
                text: strings.detail_badge_cached.to_string(),
                tone: DetailBadgeTone::Neutral,
            })
        } else if rows.iter().any(|row| row.warn) {
            Some(DetailBadge {
                text: strings.detail_badge_near_limit.to_string(),
                tone: DetailBadgeTone::Critical,
            })
        } else if rows.iter().all(|row| row.percent == Some(0.0)) {
            Some(DetailBadge {
                text: strings.detail_badge_unused.to_string(),
                tone: DetailBadgeTone::Neutral,
            })
        } else {
            Some(DetailBadge {
                text: strings.detail_badge_normal.to_string(),
                tone: DetailBadgeTone::Neutral,
            })
        }
    });

    DetailProviderGroup {
        kind,
        name: name.to_string(),
        badge,
        rows,
    }
}

fn detail_usage_row(
    window_label: String,
    section: Option<&UsageWindow>,
    error: Option<ProviderStatus>,
    dividers: i32,
    strings: Strings,
) -> DetailUsageRow {
    let percent = section.map(|section| section.percentage.clamp(0.0, 100.0));
    let reset_text = if matches!(error, Some(ProviderStatus::AuthRequired)) {
        strings.detail_sign_in_hint.to_string()
    } else {
        match section {
            None => strings.detail_waiting.to_string(),
            Some(section) => match section.resets_at {
                None => strings.detail_reset_unavailable.to_string(),
                Some(resets_at) => detail_reset_line(resets_at, strings, true),
            },
        }
    };

    DetailUsageRow {
        window_label,
        percent,
        reset_text,
        dividers,
        warn: percent.is_some_and(|percent| {
            compact_view::display_percent(percent) >= compact_view::WARN_THRESHOLD_PERCENT
        }),
    }
}

fn detail_percent_is_display_zero(percent: Option<f64>) -> bool {
    percent.is_some_and(|percent| compact_view::display_percent(percent) == 0)
}

/// "Resets in 2h 13m (21:30)" - relative countdown plus the absolute local
/// time, which is what people actually plan around for longer quota windows.
/// `compact` selects the popup's tightened "… · HH:MM" form; the tray tooltip
/// passes false to keep its parenthesised "… (HH:MM)" wrapping.
fn detail_reset_line(resets_at: SystemTime, strings: Strings, compact: bool) -> String {
    match resets_at.duration_since(SystemTime::now()) {
        Ok(duration) if duration.as_secs() > 0 => {
            let mut text = strings
                .detail_resets_in
                .replace("{duration}", &detail_duration_text(duration, strings));
            if let Some(at) = format_local_time(resets_at, strings) {
                if compact {
                    text.push_str(" · ");
                    text.push_str(&at);
                } else {
                    text.push_str(" (");
                    text.push_str(&at);
                    text.push(')');
                }
            }
            text
        }
        _ => strings.detail_resets_now.to_string(),
    }
}

/// Format a SystemTime as local wall-clock time: "21:30" today, "Wed 21:30"
/// within the next six days, "7/16 21:30" beyond that.
fn format_local_time(t: SystemTime, strings: Strings) -> Option<String> {
    let unix = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    // Unix seconds -> FILETIME (100ns ticks since 1601-01-01).
    let ticks = unix
        .checked_mul(10_000_000)?
        .checked_add(116_444_736_000_000_000)?;
    let filetime = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&filetime, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    let now = unsafe { GetLocalTime() };
    let time = format!("{:02}:{:02}", local.wHour, local.wMinute);
    if local.wYear == now.wYear && local.wMonth == now.wMonth && local.wDay == now.wDay {
        return Some(time);
    }
    if unix.saturating_sub(now_unix_secs()) < 6 * 86_400 {
        let weekday = strings
            .weekdays
            .get(local.wDayOfWeek as usize)
            .copied()
            .unwrap_or("");
        Some(format!("{weekday} {time}"))
    } else {
        Some(format!("{}/{} {time}", local.wMonth, local.wDay))
    }
}

fn detail_status_text(state: &AppState, strings: Strings) -> String {
    if state.auth_error_paused_polling {
        return auth_status_title(state, strings).to_string();
    }
    // Rate limiting shows up either as partial-poll metadata in `data` or,
    // when every provider 429'd at once, as the recorded global error.
    if state.data.as_ref().is_some_and(|data| data.rate_limited)
        || matches!(state.last_error, Some(poller::PollError::RateLimited(_)))
    {
        return strings.detail_rate_limited.to_string();
    }
    let Some(last_success_unix) = state.last_success_unix else {
        return strings.detail_waiting.to_string();
    };

    detail_poll_timing_status(
        last_success_unix,
        state.data_is_cached,
        state.poll_interval_ms,
        strings,
        now_unix_secs(),
    )
}

fn detail_poll_timing_status(
    last_success_unix: u64,
    data_is_cached: bool,
    poll_interval_ms: u32,
    strings: Strings,
    now_unix: u64,
) -> String {
    let elapsed = now_unix.saturating_sub(last_success_unix);
    let updated = strings
        .detail_updated_ago
        .replace("{ago}", &detail_duration_from_secs(elapsed, strings));
    let mut status = if data_is_cached {
        format!("{} · {updated}", strings.detail_stale)
    } else {
        strings.detail_poll_every.replace(
            "{interval}",
            &detail_duration_from_secs((poll_interval_ms / 1000) as u64, strings),
        )
    };

    let interval_secs = (poll_interval_ms / 1000) as u64;
    if !data_is_cached && interval_secs > elapsed {
        status.push_str(" · ");
        status.push_str(&strings.detail_next_in.replace(
            "{next}",
            &detail_duration_from_secs(interval_secs - elapsed, strings),
        ));
    }
    status
}

fn detail_duration_text(duration: Duration, strings: Strings) -> String {
    detail_duration_from_secs(duration.as_secs(), strings)
}

fn detail_duration_from_secs(total_secs: u64, strings: Strings) -> String {
    if total_secs < 60 {
        return format!("{}{}", total_secs, strings.second_suffix);
    }

    let total_minutes = total_secs.div_ceil(60).max(1);
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;
    let joiner = if [
        strings.day_suffix,
        strings.hour_suffix,
        strings.minute_suffix,
    ]
    .iter()
    .any(|suffix| suffix.chars().any(is_east_asian_character))
    {
        ""
    } else {
        " "
    };

    if days > 0 {
        if hours > 0 {
            format!(
                "{}{}{joiner}{}{}",
                days, strings.day_suffix, hours, strings.hour_suffix,
            )
        } else {
            format!("{}{}", days, strings.day_suffix)
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!(
                "{}{}{joiner}{}{}",
                hours, strings.hour_suffix, minutes, strings.minute_suffix,
            )
        } else {
            format!("{}{}", hours, strings.hour_suffix)
        }
    } else {
        format!("{}{}", minutes, strings.minute_suffix)
    }
}

fn auth_status_title(state: &AppState, strings: Strings) -> &'static str {
    if state.show_claude_code {
        strings.token_expired_title
    } else if state.show_codex {
        strings.codex_token_expired_title
    } else {
        strings.antigravity_token_expired_title
    }
}

fn refresh_detail_popup_if_open() {
    let detail_hwnd = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };
    let Some(detail_hwnd) = detail_hwnd else {
        return;
    };

    unsafe {
        if !IsWindow(detail_hwnd).as_bool() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.details_hwnd = None;
            }
            return;
        }
    }

    let _dpi_scope = DpiScope::for_window(detail_hwnd);
    let snapshot = detail_popup_snapshot();
    let (width, height) = detail_popup_size(&snapshot);
    {
        let mut detail_state = lock_detail_state();
        *detail_state = Some(snapshot);
    }
    unsafe {
        // When the row set changes (Models toggled), keep the bottom edge
        // anchored - the popup sits above the tray, so growing downwards
        // would push it off the screen.
        let mut old_rect = RECT::default();
        if GetWindowRect(detail_hwnd, &mut old_rect).is_ok()
            && (old_rect.right - old_rect.left != width || old_rect.bottom - old_rect.top != height)
        {
            let _ = SetWindowPos(
                detail_hwnd,
                HWND_TOPMOST,
                old_rect.left,
                old_rect.bottom - height,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        let _ = InvalidateRect(detail_hwnd, None, false);
    }
}

fn detail_group_height(group: &DetailProviderGroup) -> i32 {
    2 * DETAIL_GROUP_PAD_V + DETAIL_GROUP_HEADER_H + group.rows.len() as i32 * DETAIL_WINDOW_ROW_H
}

fn detail_popup_size(snapshot: &DetailPopupState) -> (i32, i32) {
    let content_h = if snapshot.providers.is_empty() {
        DETAIL_EMPTY_H
    } else {
        let groups: i32 = snapshot.providers.iter().map(detail_group_height).sum();
        groups + (snapshot.providers.len() as i32 - 1) * DETAIL_GROUP_GAP
    };
    (
        sc(DETAIL_POPUP_WIDTH),
        sc(DETAIL_HEADER_H + content_h + DETAIL_CONTENT_BOTTOM_PAD + DETAIL_FOOTER_H),
    )
}

/// Diagnostic: render the taskbar badge strip and floating numeric monitor at
/// 125% DPI with representative values. The output deliberately uses the
/// production scene and GDI executor, so layout previews exercise the same
/// fonts, provider tiles, colors, and clipping as the live Debug windows.
pub fn dump_widget(dir: &str) -> i32 {
    let _dpi = DpiScope::new(120);
    let window = |label: &str, percent: f64, countdown: &str, severity: compact_view::Severity| {
        compact_view::WindowView {
            label: label.to_string(),
            percent: Some(percent),
            display_percent: compact_view::display_percent(percent),
            percent_text: format!("{}%", compact_view::display_percent(percent)),
            countdown: countdown.to_string(),
            duration_seconds: None,
            severity,
        }
    };
    let provider = |kind, windows, attention| compact_view::ProviderView {
        kind,
        badge: None,
        windows,
        placeholder: None,
        attention,
    };
    let warn_vm = CompactViewModel {
        providers: vec![
            provider(
                tray_icon::TrayIconKind::Claude,
                vec![
                    window("5h", 64.0, "\u{00b7}3h", compact_view::Severity::Normal),
                    window("7d", 92.0, "\u{00b7}4d", compact_view::Severity::Warn),
                ],
                compact_view::Attention::Warn,
            ),
            provider(
                tray_icon::TrayIconKind::Codex,
                vec![window(
                    "7d",
                    51.0,
                    "\u{00b7}6d",
                    compact_view::Severity::Normal,
                )],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Antigravity,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 1.0, "\u{00b7}2d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
        ],
    };
    if let Err(error) = std::fs::create_dir_all(dir) {
        diagnose::log_error("dump compact surfaces: create directory failed", error);
        return 1;
    }

    let mut normal_vm = warn_vm.clone();
    for provider in &mut normal_vm.providers {
        provider.attention = compact_view::Attention::Normal;
        for window in &mut provider.windows {
            window.severity = compact_view::Severity::Normal;
            if window.display_percent >= compact_view::WARN_THRESHOLD_PERCENT {
                window.percent = Some(82.0);
                window.display_percent = 82;
                window.percent_text = "82%".to_string();
            }
        }
    }
    // Regression fixture for mixed-width percentages inside one provider. The
    // text stays right-aligned, while each gauge must begin under the displayed
    // value instead of under the widest value in the group.
    let alignment_vm = CompactViewModel {
        providers: vec![
            provider(
                tray_icon::TrayIconKind::Claude,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 29.0, "\u{00b7}5d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Codex,
                vec![window(
                    "7d",
                    27.0,
                    "\u{00b7}6d",
                    compact_view::Severity::Normal,
                )],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Antigravity,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 1.0, "\u{00b7}1d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
        ],
    };
    let nodata_vm = CompactViewModel {
        providers: [
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ]
        .into_iter()
        .map(|kind| compact_view::ProviderView {
            kind,
            badge: None,
            windows: Vec::new(),
            placeholder: Some("--".to_string()),
            attention: compact_view::Attention::Normal,
        })
        .collect(),
    };
    let mut error_vm = normal_vm.clone();
    if let Some(provider) = error_vm
        .providers
        .iter_mut()
        .find(|provider| provider.kind == tray_icon::TrayIconKind::Codex)
    {
        provider.attention = compact_view::Attention::Error;
    }

    let mut failed = false;
    for (state_name, vm) in [
        ("normal", &normal_vm),
        ("warn", &warn_vm),
        ("nodata", &nodata_vm),
        ("error", &error_vm),
    ] {
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            for (surface_name, floating) in [("badges", false), ("rows", true)] {
                let name = format!("{surface_name}-{state_name}-{theme_name}.bmp");
                if dump_compact_surface_bmp(dir, &name, vm, floating, is_dark, false).is_err() {
                    failed = true;
                }
            }
        }
    }
    for (state_name, vm) in [("warn", &warn_vm), ("error", &error_vm)] {
        for (surface_name, floating) in [("badges", false), ("rows", true)] {
            let name = format!("{surface_name}-{state_name}-hc-dark.bmp");
            if dump_compact_surface_bmp(dir, &name, vm, floating, true, true).is_err() {
                failed = true;
            }
        }
    }
    for (theme_name, is_dark) in [("dark", true), ("light", false)] {
        let name = format!("rows-alignment-{theme_name}.bmp");
        if dump_compact_surface_bmp(dir, &name, &alignment_vm, true, is_dark, false).is_err() {
            failed = true;
        }
    }
    let tooltip_snapshots = [
        WidgetTooltipSnapshot {
            kind: tray_icon::TrayIconKind::Claude,
            provider_name: "Claude Code".to_string(),
            rows: vec![
                WidgetTooltipRow {
                    window_label: "5h".to_string(),
                    percent_text: "63%".to_string(),
                    reset_text: "3小时5分钟后重置".to_string(),
                    warn: false,
                },
                WidgetTooltipRow {
                    window_label: "7d".to_string(),
                    percent_text: "92%".to_string(),
                    reset_text: "6天11小时后重置".to_string(),
                    warn: true,
                },
            ],
        },
        WidgetTooltipSnapshot {
            kind: tray_icon::TrayIconKind::Codex,
            provider_name: "Codex".to_string(),
            rows: vec![WidgetTooltipRow {
                window_label: "7d".to_string(),
                percent_text: "31%".to_string(),
                reset_text: "6天11小时后重置".to_string(),
                warn: false,
            }],
        },
    ];
    for snapshot in &tooltip_snapshots {
        let provider = match snapshot.kind {
            tray_icon::TrayIconKind::Claude => "claude",
            tray_icon::TrayIconKind::Codex => "codex",
            tray_icon::TrayIconKind::Antigravity => "antigravity",
        };
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            let name = format!("tooltip-{provider}-{theme_name}.bmp");
            if dump_widget_tooltip_bmp(dir, &name, snapshot, is_dark, false).is_err() {
                failed = true;
            }
        }
    }
    if dump_widget_tooltip_bmp(
        dir,
        "tooltip-claude-hc-dark.bmp",
        &tooltip_snapshots[0],
        true,
        true,
    )
    .is_err()
    {
        failed = true;
    }
    i32::from(failed)
}

fn dump_compact_surface_bmp(
    dir: &str,
    name: &str,
    vm: &CompactViewModel,
    floating: bool,
    is_dark: bool,
    high_contrast: bool,
) -> Result<(), String> {
    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }
        let scene = compact_scene(screen_dc, vm, high_contrast, floating);
        let width = if floating {
            sc(FLOATING_CONTENT_LEFT_MARGIN) + scene.width
        } else {
            sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN) + scene.width
        };
        let height = scene.height;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return Err("CreateDIBSection failed".to_string());
        }
        let old_bmp = SelectObject(mem_dc, dib);
        paint_compact_surface(
            mem_dc,
            width,
            height,
            &scene,
            floating,
            is_dark,
            high_contrast,
        );
        let _ = GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = PathBuf::from(dir).join(name);
        std::fs::write(&path, file).map_err(|error| error.to_string())?;
        diagnose::log(format!("dumped compact surface to {}", path.display()));
        Ok(())
    }
}

fn dump_widget_tooltip_bmp(
    dir: &str,
    name: &str,
    snapshot: &WidgetTooltipSnapshot,
    is_dark: bool,
    high_contrast: bool,
) -> Result<(), String> {
    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }
        let layout = widget_tooltip_layout(screen_dc, snapshot);
        let width = layout.width;
        let height = layout.height;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return Err("CreateDIBSection failed".to_string());
        }
        let old_bmp = SelectObject(mem_dc, dib);
        paint_widget_tooltip_content(mem_dc, width, height, snapshot, is_dark, high_contrast);
        let _ = GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = PathBuf::from(dir).join(name);
        std::fs::write(&path, file).map_err(|error| error.to_string())?;
        diagnose::log(format!("dumped widget tooltip to {}", path.display()));
        Ok(())
    }
}

/// Diagnostic: render the detail popup with representative data (matching the
/// README screenshot's providers) to a BMP and exit. Mirrors
/// `tray_icon::dump_icons`; lets popup layout changes be eyeballed without
/// hunting for the live tray popup. Renders at 125%, matching the target
/// desktop used for final visual review.
pub fn dump_detail_popup(dir: &str) -> i32 {
    ACTIVE_WINDOW_DPI.with(|dpi| dpi.set(120));

    let row = |label: &str, percent: f64, reset: &str, dividers: i32| DetailUsageRow {
        window_label: label.to_string(),
        percent: Some(percent),
        reset_text: reset.to_string(),
        dividers,
        warn: compact_view::display_percent(percent) >= compact_view::WARN_THRESHOLD_PERCENT,
    };
    let snapshot = DetailPopupState {
        title: "Gengchou".to_string(),
        providers: vec![
            DetailProviderGroup {
                kind: tray_icon::TrayIconKind::Claude,
                name: "Claude Code".to_string(),
                badge: Some(DetailBadge {
                    text: "接近上限".to_string(),
                    tone: DetailBadgeTone::Critical,
                }),
                rows: vec![
                    row("5h", 8.0, "3小时5分钟后重置 · 01:00", 5),
                    row("7d", 92.0, "5小时5分钟后重置 · 03:00", 7),
                ],
            },
            DetailProviderGroup {
                kind: tray_icon::TrayIconKind::Codex,
                name: "Codex".to_string(),
                badge: Some(DetailBadge {
                    text: "正常".to_string(),
                    tone: DetailBadgeTone::Neutral,
                }),
                rows: vec![row("7d", 51.0, "5天9小时后重置 · 07:00", 7)],
            },
            DetailProviderGroup {
                kind: tray_icon::TrayIconKind::Antigravity,
                name: "Antigravity".to_string(),
                badge: Some(DetailBadge {
                    text: "正常".to_string(),
                    tone: DetailBadgeTone::Neutral,
                }),
                rows: vec![
                    row("5h", 0.0, "5小时后重置 · 02:55", 5),
                    row("7d", 1.0, "3天3小时后重置 · 01:51", 7),
                ],
            },
        ],
        status: "每 1分钟 · 44秒后刷新".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let (width, height) = detail_popup_size(&snapshot);
    unsafe {
        let screen_dc = GetDC(HWND::default());
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return 1;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        paint_detail_content(mem_dc, width, height, &snapshot);
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut buf = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        // GDI fills/text leave the DIB alpha byte at 0; force opaque so a PNG
        // conversion of this BMP does not come out fully transparent.
        for px in buf.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }

        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&buf);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = format!("{dir}/detail-popup.bmp");
        match std::fs::write(&path, file) {
            Ok(_) => {
                diagnose::log(format!("dumped detail popup to {path}"));
                0
            }
            Err(error) => {
                diagnose::log_error("dump detail popup: write failed", error);
                1
            }
        }
    }
}

unsafe fn detail_popup_position(width: i32, height: i32) -> (i32, i32) {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        pt.x = GetSystemMetrics(SM_CXSCREEN) - width - sc(16);
        pt.y = GetSystemMetrics(SM_CYSCREEN) - height - sc(48);
    }

    // Clamp into the work area of the monitor under the cursor, so the popup
    // never straddles two screens or covers that screen's taskbar.
    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
        monitor_info.rcWork
    } else {
        RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    };
    let margin = sc(8);

    let min_x = work.left + margin;
    let max_x = work.right - width - margin;
    let min_y = work.top + margin;
    let max_y = work.bottom - height - margin;

    let x = pt.x - width + sc(28);
    let mut y = pt.y - height - sc(12);
    if y < min_y {
        y = pt.y + sc(12);
    }

    (clamp_i32(x, min_x, max_x), clamp_i32(y, min_y, max_y))
}

fn clamp_i32(value: i32, min_value: i32, max_value: i32) -> i32 {
    if max_value < min_value {
        return min_value;
    }
    value.max(min_value).min(max_value)
}

fn ensure_detail_window_class() -> bool {
    if DETAIL_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("detail popup: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(DETAIL_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // CS_DROPSHADOW matches the native tray flyouts (pairs with the
            // DWM rounded corners set at creation).
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(detail_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            // Do not latch the registered flag on failure: a later attempt
            // (e.g. after handle pressure eases) can still succeed.
            diagnose::log("detail popup: RegisterClassExW failed");
            return false;
        }
    }

    DETAIL_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

extern "system" fn detail_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        detail_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in detail_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn detail_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_DPICHANGED_MSG => {
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            apply_suggested_dpi_rect(hwnd, lparam, "detail popup");
            let _ = InvalidateRect(hwnd, None, false);
            diagnose::log(format!("detail popup: dpi changed dpi={new_dpi}"));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_detail_popup(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // AnimateWindow(AW_BLEND) asks for the content through this message;
        // without it the fade-in would start from an empty frame.
        WM_PRINTCLIENT => {
            paint_detail_popup(HDC(wparam.0 as *mut _), hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER if wparam.0 == TIMER_DETAIL_REFRESH => {
            refresh_detail_popup_if_open();
            LRESULT(0)
        }
        WM_NCHITTEST if DETAIL_MOVEMENT_UNLOCKED.load(Ordering::SeqCst) => {
            let mut point = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let mut rect = RECT::default();
            if ScreenToClient(hwnd, &mut point).as_bool()
                && GetClientRect(hwnd, &mut rect).is_ok()
                && detail_header_is_draggable(point.x, point.y, rect.right - rect.left)
            {
                return LRESULT(HTCAPTION as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let width = rect.right - rect.left;
            let hover = if point_in_rect(x, y, &detail_close_rect(width)) {
                DETAIL_HOVER_CLOSE
            } else if point_in_rect(x, y, &detail_refresh_rect(width)) {
                DETAIL_HOVER_REFRESH
            } else if point_in_rect(x, y, &detail_move_rect(width)) {
                DETAIL_HOVER_MOVE
            } else {
                DETAIL_HOVER_NONE
            };
            if DETAIL_HOVER.swap(hover, Ordering::SeqCst) != hover {
                let _ = InvalidateRect(hwnd, None, false);
            }
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            if DETAIL_HOVER.swap(DETAIL_HOVER_NONE, Ordering::SeqCst) != DETAIL_HOVER_NONE {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if DETAIL_HOVER.load(Ordering::SeqCst) != DETAIL_HOVER_NONE {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let width = rect.right - rect.left;
            if point_in_rect(x, y, &detail_close_rect(width)) {
                diagnose::log("detail popup: close button clicked");
                let _ = DestroyWindow(hwnd);
            } else if point_in_rect(x, y, &detail_refresh_rect(width)) {
                diagnose::log("detail popup: refresh button clicked");
                let main_hwnd = {
                    let state = lock_state();
                    state.as_ref().map(|s| s.hwnd.to_hwnd())
                };
                if let Some(main_hwnd) = main_hwnd {
                    trigger_manual_refresh(main_hwnd);
                }
            } else if point_in_rect(x, y, &detail_move_rect(width)) {
                let unlocked = !DETAIL_MOVEMENT_UNLOCKED.fetch_xor(true, Ordering::SeqCst);
                diagnose::log(if unlocked {
                    "detail popup: movement unlocked"
                } else {
                    "detail popup: movement locked"
                });
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            diagnose::log("detail popup: WM_CLOSE received");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // Tray-flyout conventions: Esc closes, and clicking anywhere else
        // (focus loss) dismisses the popup.
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            diagnose::log("detail popup: closed via Escape");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
            diagnose::log("detail popup: dismissed on focus loss");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            diagnose::log(format!("detail popup: destroyed hwnd={:?}", hwnd));
            let _ = KillTimer(hwnd, TIMER_DETAIL_REFRESH);
            {
                let mut last_dismiss = DETAIL_LAST_DISMISS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *last_dismiss = Some(Instant::now());
            }
            DETAIL_HOVER.store(DETAIL_HOVER_NONE, Ordering::SeqCst);
            DETAIL_MOVEMENT_UNLOCKED.store(DETAIL_DEFAULT_MOVEMENT_UNLOCKED, Ordering::SeqCst);
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.details_hwnd.is_some_and(|stored| stored.0 == hwnd.0) {
                        s.details_hwnd = None;
                    }
                }
            }
            let mut detail_state = lock_detail_state();
            *detail_state = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_floating_window_class() -> bool {
    if FLOATING_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("floating monitor: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(FLOATING_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(floating_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            diagnose::log("floating monitor: RegisterClassExW failed");
            return false;
        }
    }

    FLOATING_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

unsafe fn primary_work_area() -> RECT {
    let mut work = RECT::default();
    if SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut RECT as *mut std::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
    {
        work
    } else {
        RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

unsafe fn work_area_near(x: i32, y: i32) -> RECT {
    let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        primary_work_area()
    }
}

unsafe fn floating_target_position(
    width: i32,
    height: i32,
    stored_x: Option<i32>,
    stored_y: Option<i32>,
) -> (i32, i32) {
    let margin = sc(FLOATING_MARGIN);
    match (stored_x, stored_y) {
        (Some(x), Some(y)) => {
            let work = work_area_near(x, y);
            (
                clamp_i32(x, work.left + margin, work.right - width - margin),
                clamp_i32(y, work.top + margin, work.bottom - height - margin),
            )
        }
        _ => {
            let work = primary_work_area();
            (work.right - width - margin, work.bottom - height - margin)
        }
    }
}

fn floating_monitor_size(hwnd: Option<HWND>) -> (i32, i32) {
    let state = lock_state();
    let Some(state) = state.as_ref() else {
        return (sc(180), sc(52));
    };
    let scene = compact_scene_for_hwnd(
        hwnd.unwrap_or_default(),
        &state.compact_vm,
        state.is_high_contrast,
        true,
    );
    (sc(FLOATING_CONTENT_LEFT_MARGIN) + scene.width, scene.height)
}

const DWM_COLOR_NONE: u32 = 0xFFFF_FFFE;

unsafe fn apply_floating_dwm_style(hwnd: HWND, is_dark: bool, high_contrast: bool) {
    let corner = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&corner) as u32,
    );
    let dark_mode = BOOL::from(is_dark);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark_mode as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&dark_mode) as u32,
    );
    let border_color = if high_contrast {
        theme::system_color(COLOR_WINDOWTEXT).to_colorref()
    } else {
        DWM_COLOR_NONE
    };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border_color as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&border_color) as u32,
    );
}

fn floating_drag_distance_exceeded(delta_x: i32, delta_y: i32) -> bool {
    delta_x.abs() >= sc(FLOATING_DRAG_THRESHOLD) || delta_y.abs() >= sc(FLOATING_DRAG_THRESHOLD)
}

fn ensure_floating_monitor_window() -> Option<HWND> {
    let existing = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.floating_hwnd)
    };
    if let Some(hwnd) = existing {
        if unsafe { IsWindow(hwnd).as_bool() } {
            return Some(hwnd);
        }
    }
    if !ensure_floating_window_class() {
        return None;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("floating monitor: GetModuleHandleW failed", error);
                return None;
            }
        };
        let (title, stored_x, stored_y, is_dark, high_contrast) = {
            let state = lock_state();
            let s = state.as_ref()?;
            (
                s.language.strings().window_title,
                s.floating_x,
                s.floating_y,
                s.is_dark,
                s.is_high_contrast,
            )
        };
        let (width, height) = floating_monitor_size(None);
        let (x, y) = floating_target_position(width, height, stored_x, stored_y);
        let class_name = native_interop::wide_str(FLOATING_WINDOW_CLASS_NAME);
        let title = native_interop::wide_str(title);
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            x,
            y,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(hinstance.0),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("floating monitor: CreateWindowExW failed", error);
                return None;
            }
        };
        {
            let _dpi_scope = DpiScope::for_window(hwnd);
            let (width, height) = floating_monitor_size(Some(hwnd));
            let (x, y) = floating_target_position(width, height, stored_x, stored_y);
            if let Err(error) =
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE)
            {
                diagnose::log_error(
                    "floating monitor: DPI-aware initial positioning failed",
                    error,
                );
            }
        }
        apply_floating_dwm_style(hwnd, is_dark, high_contrast);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_hwnd = Some(hwnd);
            }
        }
        diagnose::log(format!("floating monitor: created hwnd={:?}", hwnd));
        Some(hwnd)
    }
}

fn refresh_floating_monitor(reset_position: bool) {
    let (visible, stored_x, stored_y) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.floating_visible,
                if reset_position { None } else { s.floating_x },
                if reset_position { None } else { s.floating_y },
            ),
            None => return,
        }
    };
    // Countdown/theme refreshes also reach this function. Do not create a
    // permanently hidden HWND for users who never enable the floating window.
    // Resetting while hidden still records the primary-work-area default.
    if !visible {
        if reset_position {
            let _dpi_scope = DpiScope::new(unsafe { GetDpiForSystem() });
            let (width, height) = floating_monitor_size(None);
            let (x, y) = unsafe { floating_target_position(width, height, None, None) };
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_x = Some(x);
                s.floating_y = Some(y);
            }
        }
        return;
    }

    let hwnd = match ensure_floating_monitor_window() {
        Some(hwnd) => hwnd,
        None => return,
    };
    let _dpi_scope = DpiScope::for_window(hwnd);
    let (width, height) = floating_monitor_size(Some(hwnd));
    unsafe {
        if FLOATING_MOVING.load(Ordering::Acquire) {
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        let (x, y) = floating_target_position(width, height, stored_x, stored_y);
        // WS_EX_TOPMOST keeps the window in the topmost band. Preserve its
        // relative z-order here: this path runs on every countdown update and
        // must not repeatedly jump ahead of unrelated topmost windows.
        let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW;
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, width, height, flags);
        let _ = InvalidateRect(hwnd, None, false);
        if reset_position {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_x = Some(x);
                s.floating_y = Some(y);
            }
        }
    }
}

fn toggle_floating_monitor() {
    let visible = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        s.floating_visible = !s.floating_visible;
        s.floating_visible
    };
    if visible {
        if ensure_floating_monitor_window().is_none() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_visible = false;
            }
        } else {
            refresh_floating_monitor(false);
        }
    } else {
        let hwnd = {
            let state = lock_state();
            state.as_ref().and_then(|s| s.floating_hwnd)
        };
        if let Some(hwnd) = hwnd {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
    save_state_settings();
}

fn reset_floating_position() {
    refresh_floating_monitor(true);
    save_state_settings();
}

fn remember_floating_position(hwnd: HWND) -> bool {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return false;
        };
        s.floating_x = Some(rect.left);
        s.floating_y = Some(rect.top);
        true
    }
}

extern "system" fn floating_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        floating_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in floating_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn floating_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_DPICHANGED_MSG => {
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            apply_suggested_dpi_rect(hwnd, lparam, "floating monitor");
            let _ = remember_floating_position(hwnd);
            save_state_settings();
            let _ = InvalidateRect(hwnd, None, false);
            diagnose::log(format!("floating monitor: dpi changed dpi={new_dpi}"));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let mut cursor = POINT::default();
            let mut rect = RECT::default();
            if GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut rect).is_ok() {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                drag.tracking = true;
                drag.moved = false;
                drag.start_cursor_x = cursor.x;
                drag.start_cursor_y = cursor.y;
                drag.start_window_x = rect.left;
                drag.start_window_y = rect.top;
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut cursor = POINT::default();
            if GetCursorPos(&mut cursor).is_err() {
                return LRESULT(0);
            }
            let target = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !drag.tracking {
                    None
                } else {
                    let delta_x = cursor.x - drag.start_cursor_x;
                    let delta_y = cursor.y - drag.start_cursor_y;
                    if !drag.moved && floating_drag_distance_exceeded(delta_x, delta_y) {
                        drag.moved = true;
                        FLOATING_MOVING.store(true, Ordering::Release);
                    }
                    drag.moved
                        .then_some((drag.start_window_x + delta_x, drag.start_window_y + delta_y))
                }
            };
            if let Some((x, y)) = target {
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (tracking, moved) = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = (drag.tracking, drag.moved);
                drag.tracking = false;
                drag.moved = false;
                result
            };
            if tracking {
                let _ = ReleaseCapture();
            }
            FLOATING_MOVING.store(false, Ordering::Release);

            if moved {
                let _ = remember_floating_position(hwnd);
                refresh_floating_monitor(false);
                save_state_settings();
            } else {
                show_usage_details(hwnd);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            let moved = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let moved = drag.moved;
                drag.tracking = false;
                drag.moved = false;
                moved
            };
            FLOATING_MOVING.store(false, Ordering::Release);
            if moved {
                let _ = remember_floating_position(hwnd);
                refresh_floating_monitor(false);
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let main_hwnd = current_main_hwnd();
            if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
                show_context_menu(main_hwnd);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            refresh_floating_monitor(false);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.floating_visible = false;
                }
            }
            save_state_settings();
            LRESULT(0)
        }
        WM_DESTROY => {
            FLOATING_MOVING.store(false, Ordering::Release);
            {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                drag.tracking = false;
                drag.moved = false;
            }
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if s.floating_hwnd.is_some_and(|stored| stored.0 == hwnd.0) {
                    s.floating_hwnd = None;
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn point_in_rect(x: i32, y: i32, rect: &RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn detail_close_rect(width: i32) -> RECT {
    RECT {
        left: width - sc(42),
        top: sc(13),
        right: width - sc(16),
        bottom: sc(39),
    }
}

fn detail_refresh_rect(width: i32) -> RECT {
    RECT {
        left: width - sc(74),
        top: sc(13),
        right: width - sc(48),
        bottom: sc(39),
    }
}

fn detail_move_rect(width: i32) -> RECT {
    RECT {
        left: width - sc(106),
        top: sc(13),
        right: width - sc(80),
        bottom: sc(39),
    }
}

fn detail_header_is_draggable(x: i32, y: i32, width: i32) -> bool {
    point_in_rect(
        x,
        y,
        &RECT {
            left: sc(4),
            top: sc(4),
            right: detail_move_rect(width).left - sc(4),
            bottom: sc(DETAIL_HEADER_H - 4),
        },
    )
}

fn paint_detail_popup(hdc: HDC, hwnd: HWND) {
    let _dpi_scope = DpiScope::for_window(hwnd);
    let snapshot = {
        let detail_state = lock_detail_state();
        detail_state.clone().unwrap_or_else(|| DetailPopupState {
            title: "Gengchou".to_string(),
            providers: Vec::new(),
            status: LanguageId::English.strings().detail_waiting.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;
        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_detail_content(mem_dc, width, height, &snapshot);
        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

/// Popup colours follow the system theme (like the widget) and reuse the
/// widget's per-provider accents so the colour language stays consistent
/// across widget, tray icons and popup.
struct DetailPalette {
    bg: Color,
    card: Color,
    border: Color,
    divider: Color,
    text: Color,
    muted: Color,
    degraded: Color,
    warn: Color,
    warn_bg: Color,
    warn_on_bg: Color,
    track: Color,
}

fn detail_palette(is_dark: bool, high_contrast: bool) -> DetailPalette {
    if high_contrast {
        DetailPalette {
            bg: theme::system_color(COLOR_WINDOW),
            card: theme::system_color(COLOR_WINDOW),
            border: theme::system_color(COLOR_WINDOWFRAME),
            divider: theme::system_color(COLOR_GRAYTEXT),
            text: theme::system_color(COLOR_WINDOWTEXT),
            muted: theme::system_color(COLOR_GRAYTEXT),
            degraded: theme::system_color(COLOR_WINDOWTEXT),
            warn: theme::system_color(COLOR_HIGHLIGHT),
            warn_bg: theme::system_color(COLOR_HIGHLIGHT),
            warn_on_bg: theme::system_color(COLOR_HIGHLIGHTTEXT),
            track: theme::system_color(COLOR_GRAYTEXT),
        }
    } else if is_dark {
        DetailPalette {
            bg: Color::from_hex("#1F1F1F"),
            card: Color::from_hex("#242424"),
            border: Color::from_hex("#353535"),
            divider: Color::from_hex("#303030"),
            text: Color::from_hex("#F3F4F6"),
            muted: Color::from_hex("#9CA3AF"),
            degraded: Color::from_hex("#D8A35D"),
            warn: Color::from_hex("#FF5C66"),
            warn_bg: Color::from_hex("#493033"),
            warn_on_bg: Color::from_hex("#FF747C"),
            track: Color::from_hex("#343434"),
        }
    } else {
        DetailPalette {
            bg: Color::from_hex("#F9F9F9"),
            card: Color::from_hex("#FFFFFF"),
            border: Color::from_hex("#D4D4D8"),
            divider: Color::from_hex("#E4E4E7"),
            text: Color::from_hex("#1B1B1F"),
            muted: Color::from_hex("#6B7280"),
            degraded: Color::from_hex("#946200"),
            warn: Color::from_hex("#DC2626"),
            warn_bg: Color::from_hex("#FDECEC"),
            warn_on_bg: Color::from_hex("#B91C1C"),
            track: Color::from_hex("#E7E7EA"),
        }
    }
}

fn provider_accent(kind: tray_icon::TrayIconKind, is_dark: bool, high_contrast: bool) -> Color {
    match kind {
        tray_icon::TrayIconKind::Claude => claude_accent_color(high_contrast),
        tray_icon::TrayIconKind::Codex => codex_accent_color(is_dark, high_contrast),
        tray_icon::TrayIconKind::Antigravity => antigravity_accent_color(high_contrast),
    }
}

/// Each brand mark keeps its real silhouette while the tile supplies a shared
/// optical footprint. Codex deliberately uses the black OpenAI mark on a light
/// tile in both themes, matching the reference and avoiding a generic app icon.
fn provider_chip_style(
    kind: tray_icon::TrayIconKind,
    is_dark: bool,
    high_contrast: bool,
    palette: &DetailPalette,
) -> (Color, Color, bool) {
    if high_contrast {
        return (palette.card, palette.border, is_dark);
    }

    match (kind, is_dark) {
        (tray_icon::TrayIconKind::Claude, true) => {
            (Color::from_hex("#30211E"), Color::from_hex("#70483D"), true)
        }
        (tray_icon::TrayIconKind::Claude, false) => (
            Color::from_hex("#FFF0EA"),
            Color::from_hex("#F1C8BA"),
            false,
        ),
        (tray_icon::TrayIconKind::Codex, _) => (
            Color::from_hex("#F7F7F5"),
            Color::from_hex("#D4D4D0"),
            false,
        ),
        (tray_icon::TrayIconKind::Antigravity, true) => {
            (Color::from_hex("#172B4A"), Color::from_hex("#3C68A4"), true)
        }
        (tray_icon::TrayIconKind::Antigravity, false) => (
            Color::from_hex("#E8F0FF"),
            Color::from_hex("#BFD3FF"),
            false,
        ),
    }
}

fn paint_detail_content(hdc: HDC, width: i32, height: i32, snapshot: &DetailPopupState) {
    let is_dark = theme::is_dark_mode();
    let high_contrast = theme::is_high_contrast();
    let palette = detail_palette(is_dark, high_contrast);

    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        fill_rect_color(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            &palette.bg,
        );
        for edge in [
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: sc(1),
            },
            RECT {
                left: 0,
                top: height - sc(1),
                right: width,
                bottom: height,
            },
            RECT {
                left: 0,
                top: 0,
                right: sc(1),
                bottom: height,
            },
            RECT {
                left: width - sc(1),
                top: 0,
                right: width,
                bottom: height,
            },
        ] {
            fill_rect_color(hdc, &edge, &palette.border);
        }

        let margin = sc(18);
        draw_detail_text(
            hdc,
            &snapshot.title,
            RECT {
                left: margin,
                top: sc(14),
                right: width - sc(116),
                bottom: sc(40),
            },
            &palette.text,
            18,
            FW_SEMIBOLD.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        // Header buttons: temporary movement lock + refresh + close. Movement
        // is enabled by default and never persisted between popup instances.
        let hover = DETAIL_HOVER.load(Ordering::SeqCst);
        let movement_unlocked = DETAIL_MOVEMENT_UNLOCKED.load(Ordering::SeqCst);
        let move_rect = detail_move_rect(width);
        let refresh_rect = detail_refresh_rect(width);
        let close_rect = detail_close_rect(width);
        let movement_locked = !movement_unlocked;
        if hover == DETAIL_HOVER_MOVE || movement_locked {
            draw_rounded_rect(hdc, &move_rect, &palette.divider, sc(4));
        }
        if hover == DETAIL_HOVER_REFRESH {
            draw_rounded_rect(hdc, &refresh_rect, &palette.divider, sc(4));
        }
        if hover == DETAIL_HOVER_CLOSE {
            draw_rounded_rect(hdc, &close_rect, &palette.divider, sc(4));
        }
        // Segoe MDL2 Assets E72E/E785 are the shell lock/unlock glyphs.
        draw_detail_text_face(
            hdc,
            if movement_unlocked {
                "\u{E785}"
            } else {
                "\u{E72E}"
            },
            move_rect,
            if hover == DETAIL_HOVER_MOVE || movement_locked {
                &palette.text
            } else {
                &palette.muted
            },
            "Segoe MDL2 Assets",
            12,
            FW_NORMAL.0 as i32,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        // Segoe MDL2 Assets ships with Windows 10+; E72C is the standard
        // refresh arrow, matching the shell's own iconography.
        draw_detail_text_face(
            hdc,
            "\u{E72C}",
            refresh_rect,
            &palette.muted,
            "Segoe MDL2 Assets",
            13,
            FW_NORMAL.0 as i32,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        draw_detail_text(
            hdc,
            "\u{2715}",
            close_rect,
            &palette.muted,
            14,
            FW_NORMAL.0 as i32,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        let mut y = sc(DETAIL_HEADER_H);
        if snapshot.providers.is_empty() {
            let waiting = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.language.strings().detail_waiting)
                    .unwrap_or(LanguageId::English.strings().detail_waiting)
            };
            draw_detail_body_text(
                hdc,
                waiting,
                RECT {
                    left: margin,
                    top: y,
                    right: width - margin,
                    bottom: y + sc(DETAIL_EMPTY_H),
                },
                &palette.muted,
                13,
                FW_NORMAL.0 as i32,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        } else {
            for group in &snapshot.providers {
                draw_detail_group(hdc, width, y, group, &palette, is_dark, high_contrast);
                y += sc(detail_group_height(group)) + sc(DETAIL_GROUP_GAP);
            }
        }

        let footer_top = height - sc(DETAIL_FOOTER_H);
        fill_rect_color(
            hdc,
            &RECT {
                left: margin,
                top: footer_top,
                right: width - margin,
                bottom: footer_top + sc(1),
            },
            &palette.divider,
        );
        draw_detail_body_text(
            hdc,
            &snapshot.status,
            RECT {
                left: margin,
                top: footer_top + sc(8),
                right: width - sc(74),
                bottom: height - sc(6),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_text(
            hdc,
            &format!("v{}", snapshot.version),
            RECT {
                left: width - sc(74),
                top: footer_top + sc(8),
                right: width - margin,
                bottom: height - sc(6),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// One provider card: a compact identity header sits above aligned quota rows.
/// Warning groups add a slim left rail and a status pill without tinting the
/// whole card, so the hierarchy stays readable at a glance.
fn draw_detail_group(
    hdc: HDC,
    width: i32,
    group_y: i32,
    group: &DetailProviderGroup,
    palette: &DetailPalette,
    is_dark: bool,
    high_contrast: bool,
) {
    let card = RECT {
        left: sc(DETAIL_CARD_MARGIN),
        top: group_y,
        right: width - sc(DETAIL_CARD_MARGIN),
        bottom: group_y + sc(detail_group_height(group)),
    };
    let card_radius = sc(8);
    draw_rounded_rect(hdc, &card, &palette.border, card_radius);
    draw_rounded_rect(
        hdc,
        &RECT {
            left: card.left + sc(1),
            top: card.top + sc(1),
            right: card.right - sc(1),
            bottom: card.bottom - sc(1),
        },
        &palette.card,
        (card_radius - sc(1)).max(sc(1)),
    );

    let accent = provider_accent(group.kind, is_dark, high_contrast);
    let group_warn = group.rows.iter().any(|row| row.warn)
        || group
            .badge
            .as_ref()
            .is_some_and(|badge| badge.tone == DetailBadgeTone::Critical);
    if group_warn {
        draw_rounded_rect(
            hdc,
            &RECT {
                left: card.left,
                top: card.top + sc(1),
                right: card.left + sc(3),
                bottom: card.bottom - sc(1),
            },
            &palette.warn,
            sc(2),
        );
    }

    // The warning rail nudges its card's content by 2px, as in the reference,
    // while every bar/reset pair still shares one strict column grid.
    let content_left = card.left + sc(14 + if group_warn { 2 } else { 0 });
    let content_right = card.right - sc(12);
    let header_top = card.top + sc(DETAIL_GROUP_PAD_V);
    let header_bottom = header_top + sc(DETAIL_GROUP_HEADER_H);
    let rows_y = header_bottom;
    let row_label_left = content_left;
    let bar_left = content_left + sc(30);
    let percent_right = content_right;
    let percent_text_width =
        measure_detail_text_width(hdc, "100%", "Segoe UI", 16, FW_SEMIBOLD.0 as i32);
    let percent_left = percent_right - detail_percent_column_width(percent_text_width);
    let bar_right = percent_left - sc(4);

    let chip = sc(DETAIL_LOGO_CHIP_SIZE);
    let chip_left = content_left;
    let chip_top = header_top + (sc(DETAIL_GROUP_HEADER_H) - chip) / 2;
    let chip_radius = sc(7);
    match provider_tile_icon(
        group.kind,
        active_window_dpi(),
        is_dark,
        high_contrast,
        TileSize::Chip28,
    ) {
        Some((hicon, tile)) => unsafe {
            debug_assert_eq!(tile, chip);
            let _ = DrawIconEx(
                hdc,
                chip_left,
                chip_top,
                hicon,
                tile,
                tile,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
        },
        None => {
            // High Contrast and decode failures stay palette-driven. Normal
            // provider tiles are rendered offline so their rounded border and
            // detailed mark share one supersampled antialiasing grid.
            let (chip_bg, chip_border, _) =
                provider_chip_style(group.kind, is_dark, high_contrast, palette);
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: chip_left,
                    top: chip_top,
                    right: chip_left + chip,
                    bottom: chip_top + chip,
                },
                &chip_border,
                chip_radius,
            );
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: chip_left + sc(1),
                    top: chip_top + sc(1),
                    right: chip_left + chip - sc(1),
                    bottom: chip_top + chip - sc(1),
                },
                &chip_bg,
                (chip_radius - sc(1)).max(sc(1)),
            );
            let dot = sc(10);
            let dot_left = chip_left + (chip - dot) / 2;
            let dot_top = chip_top + (chip - dot) / 2;
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: dot_left,
                    top: dot_top,
                    right: dot_left + dot,
                    bottom: dot_top + dot,
                },
                &accent,
                dot / 2,
            );
        }
    }

    let badge_layout = group.badge.as_ref().map(|badge| {
        let text_width = measure_detail_text_width(
            hdc,
            &badge.text,
            detail_body_face(&badge.text),
            11,
            FW_NORMAL.0 as i32,
        );
        let width = detail_badge_width(text_width, badge.tone);
        let (badge_left, name_right) = detail_badge_horizontal_bounds(content_right, width);
        (badge, width, badge_left, name_right)
    });
    let name_right = badge_layout
        .as_ref()
        .map(|(_, _, _, name_right)| *name_right)
        .unwrap_or(content_right);

    draw_detail_text(
        hdc,
        &group.name,
        RECT {
            left: chip_left + chip + sc(8),
            top: header_top,
            right: name_right,
            bottom: header_bottom,
        },
        &palette.text,
        16,
        FW_SEMIBOLD.0 as i32,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    if let Some((badge, badge_w, badge_left, _)) = badge_layout {
        if badge.tone == DetailBadgeTone::Critical {
            let badge_h = sc(22);
            let badge_rect = RECT {
                left: badge_left,
                top: header_top + (sc(DETAIL_GROUP_HEADER_H) - badge_h) / 2,
                right: content_right,
                bottom: header_top + (sc(DETAIL_GROUP_HEADER_H) + badge_h) / 2,
            };
            draw_rounded_rect(hdc, &badge_rect, &palette.warn_bg, badge_h / 2);
            draw_detail_body_text(
                hdc,
                &badge.text,
                badge_rect,
                &palette.warn_on_bg,
                11,
                FW_NORMAL.0 as i32,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        } else {
            draw_detail_body_text(
                hdc,
                &badge.text,
                RECT {
                    left: content_right - badge_w,
                    top: header_top,
                    right: content_right,
                    bottom: header_bottom,
                },
                if badge.tone == DetailBadgeTone::Degraded {
                    &palette.degraded
                } else {
                    &palette.muted
                },
                11,
                FW_NORMAL.0 as i32,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
    }

    let mut row_y = rows_y;
    for row in &group.rows {
        let percent_text = match row.percent {
            Some(percent) => format!("{}%", compact_view::display_percent(percent)),
            None => "--".to_string(),
        };
        draw_detail_text(
            hdc,
            &row.window_label,
            RECT {
                left: row_label_left,
                top: row_y,
                right: bar_left - sc(2),
                bottom: row_y + sc(DETAIL_PRIMARY_LINE_H),
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_text(
            hdc,
            &percent_text,
            RECT {
                left: percent_left,
                top: row_y,
                right: percent_right,
                bottom: row_y + sc(DETAIL_PRIMARY_LINE_H),
            },
            if row.warn {
                &palette.warn
            } else if detail_percent_is_display_zero(row.percent) {
                &palette.muted
            } else {
                &palette.text
            },
            16,
            FW_SEMIBOLD.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_rect = RECT {
            left: bar_left,
            top: row_y + sc(4),
            right: bar_right,
            bottom: row_y + sc(14),
        };
        draw_detail_bar(
            hdc,
            &bar_rect,
            row.percent.unwrap_or(0.0),
            if row.warn { &palette.warn } else { &accent },
            &palette.track,
            row.dividers,
        );

        draw_detail_body_text(
            hdc,
            &row.reset_text,
            RECT {
                left: bar_left,
                top: row_y + sc(20),
                right: content_right,
                bottom: row_y + sc(42),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        row_y += sc(DETAIL_WINDOW_ROW_H);
    }
}

fn detail_bar_cell_rect(rect: &RECT, cell_count: i32, index: i32) -> RECT {
    let cell_count = cell_count.max(1);
    debug_assert!((0..cell_count).contains(&index));

    let gap = if cell_count > 1 {
        sc(DETAIL_BAR_GAP)
    } else {
        0
    };
    let span = rect.right - rect.left;
    let left = rect.left + span * index / cell_count;
    let boundary = rect.left + span * (index + 1) / cell_count;
    let right = if index + 1 == cell_count {
        rect.right
    } else {
        (boundary - gap).max(left + 1)
    };

    RECT {
        left,
        top: rect.top,
        right,
        bottom: rect.bottom,
    }
}

fn draw_detail_bar(
    hdc: HDC,
    rect: &RECT,
    percent: f64,
    accent: &Color,
    track: &Color,
    dividers: i32,
) {
    // The bar is split into `dividers` discrete cells (5 for the 5-hour window,
    // 7 for the 7-day one - see usage_window_dividers), so the segment count
    // echoes the window length and matches the taskbar widget's pip language.
    // The fill stays continuous across the cells and the boundary cell fills
    // proportionally, so a precise percentage still reads at low values.
    unsafe {
        let n = dividers.max(1);
        let radius = sc(2);
        let span = rect.right - rect.left;
        let fill_x = rect.left + ((span as f64) * percent.clamp(0.0, 100.0) / 100.0).round() as i32;
        for i in 0..n {
            let cell = detail_bar_cell_rect(rect, n, i);
            draw_rounded_rect(hdc, &cell, track, radius);

            let filled = (fill_x - cell.left).clamp(0, cell.right - cell.left);
            if filled > 0 {
                let rgn = CreateRoundRectRgn(
                    cell.left,
                    cell.top,
                    cell.right + 1,
                    cell.bottom + 1,
                    radius * 2,
                    radius * 2,
                );
                let _ = SelectClipRgn(hdc, rgn);
                fill_rect_color(
                    hdc,
                    &RECT {
                        left: cell.left,
                        top: cell.top,
                        right: cell.left + filled,
                        bottom: cell.bottom,
                    },
                    accent,
                );
                let _ = SelectClipRgn(hdc, HRGN::default());
                let _ = DeleteObject(rgn);
            }
        }
    }
}

/// Cache of fonts keyed by (face, pixel height, weight), shared by the widget
/// and the detail popup. GDI fonts are cheap but both surfaces repaint on
/// every poll refresh; a handful of cached handles (a few per DPI the window
/// has lived at) beats create/destroy per frame.
type FontCacheEntry = ((&'static str, i32, i32), isize);
static FONT_CACHE: Mutex<Vec<FontCacheEntry>> = Mutex::new(Vec::new());

fn cached_font_named(face: &'static str, height_px: i32, weight: i32) -> HFONT {
    let mut cache = FONT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, handle)) = cache
        .iter()
        .find(|(key, _)| *key == (face, height_px, weight))
    {
        return HFONT(*handle as *mut _);
    }
    let font_name = native_interop::wide_str(face);
    let font = unsafe {
        CreateFontW(
            -height_px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        )
    };
    cache.push(((face, height_px, weight), font.0 as isize));
    font
}

fn cached_font(height_px: i32, weight: i32) -> HFONT {
    cached_font_named("Segoe UI", height_px, weight)
}

fn is_east_asian_character(ch: char) -> bool {
    matches!(
        ch,
        '\u{3040}'..='\u{30FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{F900}'..='\u{FAFF}'
    )
}

fn detail_body_face(text: &str) -> &'static str {
    if text.chars().any(|ch| matches!(ch, '\u{AC00}'..='\u{D7AF}')) {
        "Malgun Gothic"
    } else if text.chars().any(|ch| matches!(ch, '\u{3040}'..='\u{30FF}')) {
        "Yu Gothic UI"
    } else if text.chars().any(|ch| {
        matches!(
            ch,
            '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
        )
    }) {
        "Microsoft YaHei UI"
    } else {
        "Segoe UI"
    }
}

/// Complete provider tiles rendered at 8x from the pinned SVGs, then reduced
/// with premultiplied-alpha Lanczos filtering. The final PNG combines the
/// rounded background, border, and 19dp mark on one antialiasing grid.
const PROVIDER_TILE_BUCKET_DPIS: [u32; 5] = [96, 120, 144, 168, 192];
const PROVIDER_TILE_SIZES: [i32; 5] = [28, 35, 42, 49, 56];
const PROVIDER_TILE_C20_SIZES: [i32; 5] = [20, 25, 30, 35, 40];
const PROVIDER_TILE_C16_SIZES: [i32; 5] = [16, 20, 24, 28, 32];

const PROVIDER_TILE_CLAUDE_DARK: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-192.png"),
];
const PROVIDER_TILE_CLAUDE_LIGHT: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-192.png"),
];
const PROVIDER_TILE_CLAUDE_DARK_C20: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-192.png"),
];
const PROVIDER_TILE_CLAUDE_LIGHT_C20: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-192.png"),
];
const PROVIDER_TILE_CLAUDE_DARK_C16: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-192.png"),
];
const PROVIDER_TILE_CLAUDE_LIGHT_C16: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-192.png"),
];
const PROVIDER_TILE_OPENAI: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/openai-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-192.png"),
];
const PROVIDER_TILE_OPENAI_C20: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/openai-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-192.png"),
];
const PROVIDER_TILE_OPENAI_C16: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/openai-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_DARK: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_LIGHT: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_DARK_C20: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_LIGHT_C20: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_DARK_C16: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-192.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_LIGHT_C16: [&[u8]; 5] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-192.png"),
];

#[derive(Clone, Copy)]
struct ProviderTileAsset {
    bucket: usize,
    size: i32,
    bytes: &'static [u8],
}

fn nearest_provider_tile_bucket(dpi: u32) -> usize {
    let dpi = normalize_dpi(dpi);
    let mut best = 0;
    let mut best_distance = dpi.abs_diff(PROVIDER_TILE_BUCKET_DPIS[0]);
    for (index, bucket_dpi) in PROVIDER_TILE_BUCKET_DPIS.iter().enumerate().skip(1) {
        let distance = dpi.abs_diff(*bucket_dpi);
        if distance < best_distance {
            best = index;
            best_distance = distance;
        }
    }
    best
}

fn provider_tile_asset(
    kind: tray_icon::TrayIconKind,
    dpi: u32,
    is_dark: bool,
    tile_size: TileSize,
) -> ProviderTileAsset {
    let bucket = nearest_provider_tile_bucket(dpi);
    let bytes = match (kind, is_dark, tile_size) {
        (tray_icon::TrayIconKind::Claude, true, TileSize::Chip28) => {
            PROVIDER_TILE_CLAUDE_DARK[bucket]
        }
        (tray_icon::TrayIconKind::Claude, false, TileSize::Chip28) => {
            PROVIDER_TILE_CLAUDE_LIGHT[bucket]
        }
        (tray_icon::TrayIconKind::Claude, true, TileSize::Chip20) => {
            PROVIDER_TILE_CLAUDE_DARK_C20[bucket]
        }
        (tray_icon::TrayIconKind::Claude, false, TileSize::Chip20) => {
            PROVIDER_TILE_CLAUDE_LIGHT_C20[bucket]
        }
        (tray_icon::TrayIconKind::Claude, true, TileSize::Chip16) => {
            PROVIDER_TILE_CLAUDE_DARK_C16[bucket]
        }
        (tray_icon::TrayIconKind::Claude, false, TileSize::Chip16) => {
            PROVIDER_TILE_CLAUDE_LIGHT_C16[bucket]
        }
        (tray_icon::TrayIconKind::Codex, _, TileSize::Chip28) => PROVIDER_TILE_OPENAI[bucket],
        (tray_icon::TrayIconKind::Codex, _, TileSize::Chip20) => PROVIDER_TILE_OPENAI_C20[bucket],
        (tray_icon::TrayIconKind::Codex, _, TileSize::Chip16) => PROVIDER_TILE_OPENAI_C16[bucket],
        (tray_icon::TrayIconKind::Antigravity, true, TileSize::Chip28) => {
            PROVIDER_TILE_ANTIGRAVITY_DARK[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, false, TileSize::Chip28) => {
            PROVIDER_TILE_ANTIGRAVITY_LIGHT[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, true, TileSize::Chip20) => {
            PROVIDER_TILE_ANTIGRAVITY_DARK_C20[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, false, TileSize::Chip20) => {
            PROVIDER_TILE_ANTIGRAVITY_LIGHT_C20[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, true, TileSize::Chip16) => {
            PROVIDER_TILE_ANTIGRAVITY_DARK_C16[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, false, TileSize::Chip16) => {
            PROVIDER_TILE_ANTIGRAVITY_LIGHT_C16[bucket]
        }
    };
    let (logical_size, size) = match tile_size {
        TileSize::Chip16 => (16, PROVIDER_TILE_C16_SIZES[bucket]),
        TileSize::Chip20 => (20, PROVIDER_TILE_C20_SIZES[bucket]),
        TileSize::Chip28 => (28, PROVIDER_TILE_SIZES[bucket]),
    };
    debug_assert_eq!(
        size,
        scale_px_for_dpi(logical_size, PROVIDER_TILE_BUCKET_DPIS[bucket])
    );
    ProviderTileAsset {
        bucket,
        size,
        bytes,
    }
}

/// HICONs decoded from exact-DPI PNG tiles, keyed by provider, bucket and theme.
/// Like the font cache, the popup repaints on every refresh, so caching a
/// handful of handles beats decoding each frame. Windows Vista+ accepts PNG
/// icon resource bits, so no runtime image dependency is required.
type ProviderTileCacheEntry = ((tray_icon::TrayIconKind, usize, bool, TileSize), isize);
static PROVIDER_TILE_CACHE: Mutex<Vec<ProviderTileCacheEntry>> = Mutex::new(Vec::new());

fn provider_tile_icon(
    kind: tray_icon::TrayIconKind,
    dpi: u32,
    is_dark: bool,
    high_contrast: bool,
    tile_size: TileSize,
) -> Option<(HICON, i32)> {
    if high_contrast {
        return None;
    }
    let asset = provider_tile_asset(kind, dpi, is_dark, tile_size);
    let mut cache = PROVIDER_TILE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (kind, asset.bucket, is_dark, tile_size);
    if let Some((_, handle)) = cache.iter().find(|(cached_key, _)| *cached_key == key) {
        return Some((HICON(*handle as *mut _), asset.size));
    }
    match unsafe {
        CreateIconFromResourceEx(
            asset.bytes,
            TRUE,
            0x0003_0000,
            asset.size,
            asset.size,
            LR_DEFAULTCOLOR,
        )
    } {
        Ok(hicon) => {
            cache.push((key, hicon.0 as isize));
            Some((hicon, asset.size))
        }
        Err(_) => None,
    }
}

fn draw_detail_text(
    hdc: HDC,
    text: &str,
    rect: RECT,
    color: &Color,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    draw_detail_text_face(hdc, text, rect, color, "Segoe UI", font_size, weight, flags);
}

fn measure_detail_text_width(
    hdc: HDC,
    text: &str,
    face: &'static str,
    font_size: i32,
    weight: i32,
) -> i32 {
    unsafe {
        let font = cached_font_named(face, sc(font_size), weight);
        let old_font = SelectObject(hdc, font);
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let measured = if wide.is_empty() || !GetTextExtentPoint32W(hdc, &wide, &mut size).as_bool()
        {
            0
        } else {
            size.cx
        };
        SelectObject(hdc, old_font);
        measured
    }
}

fn detail_badge_width(measured_text_width: i32, tone: DetailBadgeTone) -> i32 {
    let padding = if tone == DetailBadgeTone::Critical {
        sc(20)
    } else {
        sc(2)
    };
    (measured_text_width + padding).clamp(sc(64), sc(104))
}

fn detail_badge_horizontal_bounds(content_right: i32, badge_width: i32) -> (i32, i32) {
    let badge_left = content_right - badge_width;
    (badge_left, badge_left - sc(8))
}

fn detail_percent_column_width(measured_text_width: i32) -> i32 {
    (measured_text_width + sc(2)).clamp(sc(42), sc(48))
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_body_text(
    hdc: HDC,
    text: &str,
    rect: RECT,
    color: &Color,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    draw_detail_text_face(
        hdc,
        text,
        rect,
        color,
        detail_body_face(text),
        font_size,
        weight,
        flags,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_text_face(
    hdc: HDC,
    text: &str,
    mut rect: RECT,
    color: &Color,
    face: &'static str,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let font = cached_font_named(face, sc(font_size), weight);
        let old_font = SelectObject(hdc, font);
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut text_wide, &mut rect, flags);
        SelectObject(hdc, old_font);
    }
}

fn fill_rect_color(hdc: HDC, rect: &RECT, color: &Color) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    // No release channel configured (this project's default): show the plain
    // version instead of a "Check for updates" action that can only fail.
    if !updater::update_channel_configured() {
        return format!("v{current}");
    }
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => match install_channel {
            InstallChannel::Portable => {
                format!(
                    "v{current} - {} v{}",
                    strings.update_to, release.latest_version
                )
            }
            InstallChannel::Winget => format!(
                "v{current} - {} v{}",
                localization::update_via_winget(language),
                release.latest_version
            ),
        },
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    if !updater::update_channel_configured() {
        return;
    }
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel, already_in_progress) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        let strings = app_state.language.strings();
        let already_in_progress = matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        );
        if !already_in_progress {
            app_state.update_status = UpdateStatus::Checking;
        }

        (strings, app_state.install_channel, already_in_progress)
    };
    if already_in_progress {
        if interactive {
            show_info_message(hwnd, strings.updates, strings.update_in_progress);
        }
        return;
    }

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    match install_channel {
                        InstallChannel::Portable => begin_update_apply(hwnd, release),
                        InstallChannel::Winget => begin_winget_update(hwnd),
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, already_in_progress) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        let strings = app_state.language.strings();
        let already_in_progress = matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        );
        if !already_in_progress {
            app_state.update_status = UpdateStatus::Applying;
        }

        (strings, already_in_progress)
    };
    if already_in_progress {
        show_info_message(hwnd, strings.updates, strings.update_in_progress);
        return;
    }

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => request_process_quit(),
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    match updater::begin_winget_update() {
        Ok(()) => request_process_quit(),
        Err(error) => {
            let message = format!("{}.\n\n{}", strings.update_failed, error);
            show_error_message(hwnd, strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "AIUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string. Strip surrounding
        // quotes so both the quoted form we write now and unquoted values from
        // older builds compare equal.
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice);
        let reg_value = reg_value.trim_end_matches('\0').trim_matches('"');

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Quote the path: an unquoted Run value breaks (or can be
                // hijacked) as soon as the exe lives in a folder with spaces.
                let exe = String::from_utf16_lossy(&exe_buf[..len]);
                let quoted: Vec<u16> = format!("\"{exe}\"")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        quoted.as_ptr() as *const u8,
                        quoted.len() * 2,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

const LEFT_DIVIDER_W: i32 = 5;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
// Fits the longest compact English forms (such as "100%·59m" and
// "100%·now") at Segoe UI 12px without making short values look sparse.
const WIDGET_HEIGHT: i32 = 46;

fn is_drag_handle_point(client_x: i32, client_y: i32) -> bool {
    let divider_h = sc(25);
    let divider_top = (sc(WIDGET_HEIGHT) - divider_h) / 2;
    client_x >= 0
        && client_x < sc(LEFT_DIVIDER_W)
        && client_y >= divider_top
        && client_y < divider_top + divider_h
}

fn cursor_is_on_drag_handle(hwnd: HWND) -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() || !ScreenToClient(hwnd, &mut pt).as_bool() {
            return false;
        }
        is_drag_handle_point(pt.x, pt.y)
    }
}

fn active_model_count(show_claude_code: bool, show_codex: bool, show_antigravity: bool) -> i32 {
    (show_claude_code as i32 + show_codex as i32 + show_antigravity as i32).max(1)
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let metrics = compact_metrics();
    let placeholder_width = sc(12);
    let initial_pill_width =
        metrics.pill_pad_x * 2 + metrics.chip16 + metrics.chip_gap + placeholder_width;
    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + initial_pill_width * active_models
        + metrics.badge_gap * (active_models - 1)
        + metrics.badge_right_pad
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    let scene = compact_scene_for_hwnd(
        state.hwnd.to_hwnd(),
        &state.compact_vm,
        state.is_high_contrast,
        false,
    );
    sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN) + scene.width
}

fn total_widget_width() -> i32 {
    let state = lock_state();
    state
        .as_ref()
        .map(total_widget_width_for_state)
        .unwrap_or_else(|| total_widget_width_for(1))
}

fn sync_widget_tooltip_hits(scene: &Scene) {
    lock_widget_tooltip_runtime().hits = scene.badge_hits.clone();
}

fn widget_tooltip_kind_at(client_x: i32, client_y: i32) -> Option<tray_icon::TrayIconKind> {
    let origin_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
    lock_widget_tooltip_runtime()
        .hits
        .iter()
        .find(|hit| {
            client_x >= origin_x + hit.rect.x
                && client_x < origin_x + hit.rect.x + hit.rect.w
                && client_y >= hit.rect.y
                && client_y < hit.rect.y + hit.rect.h
        })
        .map(|hit| hit.kind)
}

fn widget_tooltip_hwnd() -> Option<HWND> {
    let state = lock_state();
    state
        .as_ref()
        .and_then(|s| s.widget_tooltip_hwnd)
        .map(SendHwnd::to_hwnd)
        .filter(|hwnd| unsafe { IsWindow(*hwnd).as_bool() })
}

fn widget_tooltip_abbrev(kind: tray_icon::TrayIconKind) -> &'static str {
    match kind {
        tray_icon::TrayIconKind::Claude => "CL",
        tray_icon::TrayIconKind::Codex => "CX",
        tray_icon::TrayIconKind::Antigravity => "AG",
    }
}

fn widget_tooltip_aux_text(row: &WidgetTooltipRow) -> String {
    if row.percent_text.is_empty() {
        row.reset_text.clone()
    } else {
        format!("· {}", row.reset_text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WidgetTooltipLayout {
    width: i32,
    height: i32,
    label_width: i32,
    percent_width: i32,
}

fn widget_tooltip_layout(hdc: HDC, snapshot: &WidgetTooltipSnapshot) -> WidgetTooltipLayout {
    let padding = sc(10);
    let chip = sc(20);
    let header_gap = sc(8);
    let column_gap = sc(8);
    let row_height = sc(20);
    let header_width = chip
        + header_gap
        + measure_detail_text_width(
            hdc,
            &snapshot.provider_name,
            detail_body_face(&snapshot.provider_name),
            12,
            FW_SEMIBOLD.0 as i32,
        );
    let label_width = snapshot
        .rows
        .iter()
        .map(|row| {
            measure_detail_text_width(
                hdc,
                &row.window_label,
                detail_body_face(&row.window_label),
                12,
                FW_NORMAL.0 as i32,
            )
        })
        .max()
        .unwrap_or_default();
    let percent_width = snapshot
        .rows
        .iter()
        .map(|row| {
            measure_detail_text_width(hdc, &row.percent_text, "Segoe UI", 12, FW_SEMIBOLD.0 as i32)
        })
        .max()
        .unwrap_or_default();
    let reset_width = snapshot
        .rows
        .iter()
        .map(|row| {
            let text = widget_tooltip_aux_text(row);
            measure_detail_text_width(hdc, &text, detail_body_face(&text), 12, FW_NORMAL.0 as i32)
        })
        .max()
        .unwrap_or_default();
    let body_width = label_width
        + usize::from(percent_width > 0) as i32 * column_gap
        + percent_width
        + column_gap
        + reset_width;
    let width = (padding * 2 + header_width.max(body_width))
        .clamp(sc(WIDGET_TOOLTIP_MIN_WIDTH), sc(WIDGET_TOOLTIP_MAX_WIDTH));
    let height = padding * 2 + chip + sc(6) + row_height * snapshot.rows.len() as i32;
    WidgetTooltipLayout {
        width,
        height,
        label_width,
        percent_width,
    }
}

fn paint_widget_tooltip(hdc: HDC, hwnd: HWND) {
    let snapshot = lock_widget_tooltip_runtime().snapshot.clone();
    let Some(snapshot) = snapshot else {
        return;
    };
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    paint_widget_tooltip_content(
        hdc,
        client.right - client.left,
        client.bottom - client.top,
        &snapshot,
        theme::is_dark_mode(),
        theme::is_high_contrast(),
    );
}

fn paint_widget_tooltip_content(
    hdc: HDC,
    width: i32,
    height: i32,
    snapshot: &WidgetTooltipSnapshot,
    is_dark: bool,
    high_contrast: bool,
) {
    let palette = detail_palette(is_dark, high_contrast);
    let client = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
    }
    draw_rounded_rect(hdc, &client, &palette.border, sc(7));
    draw_rounded_rect(
        hdc,
        &RECT {
            left: sc(1),
            top: sc(1),
            right: client.right - sc(1),
            bottom: client.bottom - sc(1),
        },
        &palette.card,
        sc(6),
    );

    let layout = widget_tooltip_layout(hdc, snapshot);
    let padding = sc(10);
    let chip = sc(20);
    let header_gap = sc(8);
    let column_gap = sc(8);
    let row_height = sc(20);
    if let Some((icon, tile)) = provider_tile_icon(
        snapshot.kind,
        active_window_dpi(),
        is_dark,
        high_contrast,
        TileSize::Chip20,
    ) {
        unsafe {
            let _ = DrawIconEx(
                hdc,
                padding,
                padding,
                icon,
                tile,
                tile,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
        }
    } else {
        let chip_rect = RECT {
            left: padding,
            top: padding,
            right: padding + chip,
            bottom: padding + chip,
        };
        draw_rounded_rect(hdc, &chip_rect, &palette.border, sc(5));
        draw_rounded_rect(
            hdc,
            &RECT {
                left: chip_rect.left + sc(1),
                top: chip_rect.top + sc(1),
                right: chip_rect.right - sc(1),
                bottom: chip_rect.bottom - sc(1),
            },
            &palette.card,
            sc(4),
        );
        draw_detail_text(
            hdc,
            widget_tooltip_abbrev(snapshot.kind),
            chip_rect,
            &palette.text,
            9,
            FW_SEMIBOLD.0 as i32,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    draw_detail_body_text(
        hdc,
        &snapshot.provider_name,
        RECT {
            left: padding + chip + header_gap,
            top: padding,
            right: layout.width - padding,
            bottom: padding + chip,
        },
        &palette.text,
        12,
        FW_SEMIBOLD.0 as i32,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    let mut row_top = padding + chip + sc(6);
    for row in &snapshot.rows {
        let row_bottom = row_top + row_height;
        let label_left = padding;
        let percent_left = label_left + layout.label_width + column_gap;
        let percent_right = percent_left + layout.percent_width;
        let aux_left = if layout.percent_width > 0 {
            percent_right + column_gap
        } else {
            label_left + layout.label_width + column_gap
        };
        draw_detail_body_text(
            hdc,
            &row.window_label,
            RECT {
                left: label_left,
                top: row_top,
                right: label_left + layout.label_width,
                bottom: row_bottom,
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        if !row.percent_text.is_empty() {
            draw_detail_text(
                hdc,
                &row.percent_text,
                RECT {
                    left: percent_left,
                    top: row_top,
                    right: percent_right,
                    bottom: row_bottom,
                },
                if row.warn {
                    &palette.warn
                } else {
                    &palette.text
                },
                12,
                FW_SEMIBOLD.0 as i32,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        let aux_text = widget_tooltip_aux_text(row);
        draw_detail_body_text(
            hdc,
            &aux_text,
            RECT {
                left: aux_left,
                top: row_top,
                right: layout.width - padding,
                bottom: row_bottom,
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        row_top = row_bottom;
    }
}

fn ensure_widget_tooltip_window_class() -> bool {
    if WIDGET_TOOLTIP_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }
    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("widget tooltip: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(WIDGET_TOOLTIP_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(widget_tooltip_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            diagnose::log("widget tooltip: RegisterClassExW failed");
            return false;
        }
    }
    WIDGET_TOOLTIP_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

extern "system" fn widget_tooltip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        widget_tooltip_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in widget_tooltip_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn widget_tooltip_wnd_proc_impl(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_widget_tooltip(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            paint_widget_tooltip(HDC(wparam.0 as *mut _), hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DPICHANGED_MSG => {
            let _message_dpi_scope = DpiScope::new(dpi_from_wparam(wparam));
            apply_suggested_dpi_rect(hwnd, lparam, "widget tooltip");
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DESTROY => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if s.widget_tooltip_hwnd
                    .is_some_and(|stored| stored.to_hwnd() == hwnd)
                {
                    s.widget_tooltip_hwnd = None;
                }
            }
            lock_widget_tooltip_runtime().snapshot = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_widget_tooltip_window(owner: HWND) -> Option<HWND> {
    if let Some(hwnd) = widget_tooltip_hwnd() {
        return Some(hwnd);
    }
    if !ensure_widget_tooltip_window_class() {
        return None;
    }
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).ok()?;
        let class_name = native_interop::wide_str(WIDGET_TOOLTIP_WINDOW_CLASS_NAME);
        let tooltip = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("widget tooltip: CreateWindowExW failed", error);
                return None;
            }
        };
        let (is_dark, high_contrast, owner_matches) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (s.is_dark, s.is_high_contrast, s.hwnd.to_hwnd() == owner),
                None => (false, false, false),
            }
        };
        if !owner_matches {
            let _ = DestroyWindow(tooltip);
            return None;
        }
        apply_floating_dwm_style(tooltip, is_dark, high_contrast);
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_tooltip_hwnd = Some(SendHwnd::from_hwnd(tooltip));
        }
        Some(tooltip)
    }
}

fn widget_tooltip_anchor_rect(owner: HWND, kind: tray_icon::TrayIconKind) -> Option<RECT> {
    let hit = lock_widget_tooltip_runtime()
        .hits
        .iter()
        .find(|hit| hit.kind == kind)
        .copied()?;
    let mut owner_rect = RECT::default();
    unsafe {
        GetWindowRect(owner, &mut owner_rect).ok()?;
    }
    let origin_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
    Some(RECT {
        left: owner_rect.left + origin_x + hit.rect.x,
        top: owner_rect.top + hit.rect.y,
        right: owner_rect.left + origin_x + hit.rect.x + hit.rect.w,
        bottom: owner_rect.top + hit.rect.y + hit.rect.h,
    })
}

fn widget_tooltip_position_for_anchor(
    anchor: RECT,
    work: RECT,
    width: i32,
    height: i32,
    gap: i32,
) -> (i32, i32) {
    let centered_x = anchor.left + (anchor.right - anchor.left - width) / 2;
    let centered_y = anchor.top + (anchor.bottom - anchor.top - height) / 2;
    let (x, y) = if anchor.top >= work.bottom {
        (centered_x, anchor.top - gap - height)
    } else if anchor.bottom <= work.top {
        (centered_x, anchor.bottom + gap)
    } else if anchor.left >= work.right {
        (anchor.left - gap - width, centered_y)
    } else if anchor.right <= work.left {
        (anchor.right + gap, centered_y)
    } else if anchor.top - work.top >= height + gap {
        (centered_x, anchor.top - gap - height)
    } else {
        (centered_x, anchor.bottom + gap)
    };
    (
        clamp_i32(x, work.left, work.right - width),
        clamp_i32(y, work.top, work.bottom - height),
    )
}

fn widget_tooltip_position(
    owner: HWND,
    kind: tray_icon::TrayIconKind,
    width: i32,
    height: i32,
) -> Option<(i32, i32)> {
    let anchor = widget_tooltip_anchor_rect(owner, kind)?;
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: anchor.left + (anchor.right - anchor.left) / 2,
                y: anchor.top + (anchor.bottom - anchor.top) / 2,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
    }
    Some(widget_tooltip_position_for_anchor(
        anchor,
        info.rcWork,
        width,
        height,
        sc(WIDGET_TOOLTIP_EDGE_GAP),
    ))
}

unsafe fn hide_widget_tooltip(owner: HWND, clear_hover: bool) {
    let _ = KillTimer(owner, TIMER_WIDGET_TOOLTIP);
    if clear_hover {
        lock_widget_tooltip_runtime().hover_kind = None;
    }
    if let Some(tooltip) = widget_tooltip_hwnd() {
        let _ = ShowWindow(tooltip, SW_HIDE);
    }
}

unsafe fn update_widget_tooltip_hover(owner: HWND, client_x: i32, client_y: i32) {
    let next = widget_tooltip_kind_at(client_x, client_y);
    let changed = {
        let mut runtime = lock_widget_tooltip_runtime();
        if runtime.hover_kind == next {
            false
        } else {
            runtime.hover_kind = next;
            true
        }
    };
    if !changed {
        return;
    }
    hide_widget_tooltip(owner, false);
    if next.is_some() && SetTimer(owner, TIMER_WIDGET_TOOLTIP, WIDGET_TOOLTIP_DELAY_MS, None) == 0 {
        diagnose::log("widget tooltip: unable to start hover timer");
    }
}

unsafe fn show_widget_tooltip_for_hover(owner: HWND) {
    let _ = KillTimer(owner, TIMER_WIDGET_TOOLTIP);
    let expected = lock_widget_tooltip_runtime().hover_kind;
    let Some(kind) = expected else {
        return;
    };
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() || !ScreenToClient(owner, &mut cursor).as_bool() {
        hide_widget_tooltip(owner, true);
        return;
    }
    if widget_tooltip_kind_at(cursor.x, cursor.y) != Some(kind) {
        hide_widget_tooltip(owner, true);
        return;
    }

    let snapshot = widget_tooltip_snapshot(kind);
    lock_widget_tooltip_runtime().snapshot = Some(snapshot.clone());
    let Some(tooltip) = ensure_widget_tooltip_window(owner) else {
        return;
    };
    let _tooltip_dpi_scope = DpiScope::for_window(tooltip);
    let hdc = GetDC(tooltip);
    let layout = if hdc.0.is_null() {
        WidgetTooltipLayout {
            width: sc(240),
            height: sc(56 + snapshot.rows.len() as i32 * 20),
            label_width: sc(20),
            percent_width: sc(36),
        }
    } else {
        let layout = widget_tooltip_layout(hdc, &snapshot);
        ReleaseDC(tooltip, hdc);
        layout
    };
    let Some((x, y)) = widget_tooltip_position(owner, kind, layout.width, layout.height) else {
        return;
    };
    let (is_dark, high_contrast) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.is_dark, s.is_high_contrast))
            .unwrap_or((false, false))
    };
    apply_floating_dwm_style(tooltip, is_dark, high_contrast);
    let _ = SetWindowPos(
        tooltip,
        HWND_TOPMOST,
        x,
        y,
        layout.width,
        layout.height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = InvalidateRect(tooltip, None, false);
    let _ = UpdateWindow(tooltip);
}

unsafe fn refresh_widget_tooltip_if_visible(owner: HWND) {
    if widget_tooltip_hwnd().is_some_and(|tooltip| IsWindowVisible(tooltip).as_bool()) {
        show_widget_tooltip_for_hover(owner);
    }
}

fn compact_metrics() -> Metrics {
    let logical = Metrics::logical();
    Metrics {
        taskbar_h: sc(logical.taskbar_h),
        floating_h: sc(logical.floating_h),
        pill_h: sc(logical.pill_h),
        pill_pad_x: sc(logical.pill_pad_x),
        chip16: sc(logical.chip16),
        chip_gap: sc(logical.chip_gap),
        badge_gap: sc(logical.badge_gap),
        badge_right_pad: sc(logical.badge_right_pad),
        badge_text_gap: sc(logical.badge_text_gap),
        border_w: sc(logical.border_w).max(1),
        status_w: sc(logical.status_w),
        status_gap: sc(logical.status_gap),
        chip20: sc(logical.chip20),
        group_chip_gap: sc(logical.group_chip_gap),
        label_min_w: sc(logical.label_min_w),
        label_max_w: sc(logical.label_max_w),
        label_gap: sc(logical.label_gap),
        separator_w: sc(logical.separator_w),
        row_text_h: sc(logical.row_text_h),
        gauge_min_w: sc(logical.gauge_min_w),
        gauge_h: sc(logical.gauge_h),
        gauge_top_gap: sc(logical.gauge_top_gap),
        unit_gap: sc(logical.unit_gap),
        sep_margin: sc(logical.sep_margin),
        sep_h: sc(logical.sep_h),
        rows_left_pad: sc(logical.rows_left_pad),
        rows_right_pad: sc(logical.rows_right_pad),
    }
}

fn compact_font(key: FontKey) -> HFONT {
    match key {
        FontKey::Data12 => cached_font(sc(12), FW_MEDIUM.0 as i32),
    }
}

fn measure_compact_text(hdc: HDC, font: FontKey, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    unsafe {
        let old_font = SelectObject(hdc, compact_font(font));
        let wide = text.encode_utf16().collect::<Vec<_>>();
        let mut size = SIZE::default();
        let measured = if GetTextExtentPoint32W(hdc, &wide, &mut size).as_bool() {
            size.cx
        } else {
            0
        };
        SelectObject(hdc, old_font);
        measured
    }
}

fn compact_scene(hdc: HDC, vm: &CompactViewModel, high_contrast: bool, floating: bool) -> Scene {
    let metrics = compact_metrics();
    let measure = |font, text: &str| measure_compact_text(hdc, font, text);
    if floating {
        compact_layout::layout_provider_rows(vm, &metrics, high_contrast, &measure)
    } else {
        compact_layout::layout_badges(vm, &metrics, high_contrast, &measure)
    }
}

fn compact_scene_for_hwnd(
    hwnd: HWND,
    vm: &CompactViewModel,
    high_contrast: bool,
    floating: bool,
) -> Scene {
    unsafe {
        let hdc = GetDC(hwnd);
        if hdc.0.is_null() {
            let metrics = compact_metrics();
            let fallback = |font: FontKey, text: &str| {
                let logical_per_char = match font {
                    FontKey::Data12 => 6,
                };
                sc(logical_per_char * text.chars().count() as i32)
            };
            return if floating {
                compact_layout::layout_provider_rows(vm, &metrics, high_contrast, &fallback)
            } else {
                compact_layout::layout_badges(vm, &metrics, high_contrast, &fallback)
            };
        }
        let scene = compact_scene(hdc, vm, high_contrast, floating);
        ReleaseDC(hwnd, hdc);
        scene
    }
}

fn claude_accent_color(high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else {
        Color::from_hex("#D97757")
    }
}

fn codex_accent_color(is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_accent_color(high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else {
        Color::from_hex("#4285F4")
    }
}

struct WidgetPalette {
    bg: Color,
}

fn widget_palette(is_dark: bool, high_contrast: bool) -> WidgetPalette {
    if high_contrast {
        WidgetPalette {
            bg: theme::system_color(COLOR_WINDOW),
        }
    } else {
        WidgetPalette {
            bg: if is_dark {
                Color::from_hex("#1C1C1C")
            } else {
                Color::from_hex("#F3F3F3")
            },
        }
    }
}

fn compact_color(key: ColorKey, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        return match key {
            ColorKey::PillBg | ColorKey::GaugeTrack => theme::system_color(COLOR_WINDOW),
            ColorKey::PillBgWarn | ColorKey::GaugeWarn => theme::system_color(COLOR_HIGHLIGHT),
            ColorKey::PillAlertText => theme::system_color(COLOR_HIGHLIGHTTEXT),
            ColorKey::CanvasWarnPrimary | ColorKey::CanvasWarnSecondary => {
                theme::system_color(COLOR_WINDOWTEXT)
            }
            ColorKey::GaugeAccent(_) => theme::system_color(COLOR_HIGHLIGHT),
            ColorKey::AuxText | ColorKey::Separator => theme::system_color(COLOR_GRAYTEXT),
            ColorKey::PillText
            | ColorKey::NeutralText
            | ColorKey::HighContrastText
            | ColorKey::ErrorText => theme::system_color(COLOR_WINDOWTEXT),
        };
    }

    match key {
        ColorKey::PillBg => {
            if is_dark {
                Color::from_hex("#2B2B2B")
            } else {
                Color::from_hex("#E3E3E3")
            }
        }
        ColorKey::PillBgWarn => {
            if is_dark {
                Color::from_hex("#422A2E")
            } else {
                Color::from_hex("#FDECEC")
            }
        }
        ColorKey::PillText => {
            if is_dark {
                Color::from_hex("#E3E3E3")
            } else {
                Color::from_hex("#1F1F1F")
            }
        }
        ColorKey::PillAlertText | ColorKey::CanvasWarnPrimary | ColorKey::ErrorText => {
            if is_dark {
                Color::from_hex("#FF747C")
            } else {
                Color::from_hex("#B91C1C")
            }
        }
        ColorKey::CanvasWarnSecondary => {
            if is_dark {
                Color::from_hex("#C97F84")
            } else {
                Color::from_hex("#A14B50")
            }
        }
        ColorKey::AuxText => {
            if is_dark {
                Color::from_hex("#9A9A9A")
            } else {
                Color::from_hex("#6E6E6E")
            }
        }
        ColorKey::NeutralText => {
            if is_dark {
                Color::from_hex("#E8E8E8")
            } else {
                Color::from_hex("#1F1F1F")
            }
        }
        ColorKey::GaugeTrack => {
            if is_dark {
                Color::from_hex("#3A3A3A")
            } else {
                Color::from_hex("#C9C9C9")
            }
        }
        ColorKey::GaugeAccent(kind) => match kind {
            tray_icon::TrayIconKind::Claude => claude_accent_color(false),
            tray_icon::TrayIconKind::Codex => codex_accent_color(is_dark, false),
            tray_icon::TrayIconKind::Antigravity => antigravity_accent_color(false),
        },
        ColorKey::GaugeWarn => {
            if is_dark {
                Color::from_hex("#FF5C66")
            } else {
                Color::from_hex("#DC2626")
            }
        }
        ColorKey::Separator => {
            if is_dark {
                Color::from_hex("#2E2E2E")
            } else {
                Color::from_hex("#DADADA")
            }
        }
        ColorKey::HighContrastText => theme::system_color(COLOR_WINDOWTEXT),
    }
}

fn offset_compact_rect(rect: compact_layout::Rect, x: i32, y: i32) -> RECT {
    RECT {
        left: rect.x + x,
        top: rect.y + y,
        right: rect.x + x + rect.w,
        bottom: rect.y + y + rect.h,
    }
}

fn render_compact_scene(
    hdc: HDC,
    scene: &Scene,
    origin_x: i32,
    origin_y: i32,
    is_dark: bool,
    high_contrast: bool,
) {
    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        for command in &scene.cmds {
            match command {
                DrawCmd::RoundRect {
                    rect,
                    color,
                    radius,
                } => {
                    let rect = offset_compact_rect(*rect, origin_x, origin_y);
                    if rect.right > rect.left && rect.bottom > rect.top {
                        if *radius <= 0 {
                            fill_rect_color(
                                hdc,
                                &rect,
                                &compact_color(*color, is_dark, high_contrast),
                            );
                        } else {
                            draw_rounded_rect(
                                hdc,
                                &rect,
                                &compact_color(*color, is_dark, high_contrast),
                                *radius,
                            );
                        }
                    }
                }
                DrawCmd::StrokeRoundRect {
                    rect,
                    color,
                    radius,
                    width,
                } => {
                    let rect = offset_compact_rect(*rect, origin_x, origin_y);
                    let brush = CreateSolidBrush(COLORREF(
                        compact_color(*color, is_dark, high_contrast).to_colorref(),
                    ));
                    let region = CreateRoundRectRgn(
                        rect.left,
                        rect.top,
                        rect.right + 1,
                        rect.bottom + 1,
                        radius * 2,
                        radius * 2,
                    );
                    let _ = FrameRgn(hdc, region, brush, (*width).max(1), (*width).max(1));
                    let _ = DeleteObject(region);
                    let _ = DeleteObject(brush);
                }
                DrawCmd::GaugeFill {
                    track,
                    fraction,
                    color,
                    radius,
                } => {
                    let mut rect = offset_compact_rect(*track, origin_x, origin_y);
                    rect.right = rect.left
                        + ((track.w as f64 * fraction.clamp(0.0, 1.0)).round() as i32)
                            .clamp(1, track.w);
                    draw_rounded_rect(
                        hdc,
                        &rect,
                        &compact_color(*color, is_dark, high_contrast),
                        (*radius).min((rect.right - rect.left) / 2),
                    );
                }
                DrawCmd::Text {
                    rect,
                    text,
                    font,
                    color,
                } => {
                    if text.is_empty() || rect.w <= 0 || rect.h <= 0 {
                        continue;
                    }
                    let old_font = SelectObject(hdc, compact_font(*font));
                    let _ = SetTextColor(
                        hdc,
                        COLORREF(compact_color(*color, is_dark, high_contrast).to_colorref()),
                    );
                    let mut rect = offset_compact_rect(*rect, origin_x, origin_y);
                    let mut wide = text.encode_utf16().collect::<Vec<_>>();
                    let _ = DrawTextW(
                        hdc,
                        &mut wide,
                        &mut rect,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old_font);
                }
                DrawCmd::ProviderTile { rect, kind, size } => {
                    if let Some((icon, _)) = provider_tile_icon(
                        *kind,
                        active_window_dpi(),
                        is_dark,
                        high_contrast,
                        *size,
                    ) {
                        let rect = offset_compact_rect(*rect, origin_x, origin_y);
                        let _ = DrawIconEx(
                            hdc,
                            rect.left,
                            rect.top,
                            icon,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            0,
                            HBRUSH::default(),
                            DI_NORMAL,
                        );
                    }
                }
            }
        }
    }
}

fn draw_drag_divider(hdc: HDC, height: i32, is_dark: bool, high_contrast: bool) {
    let divider_h = sc(25);
    let divider_top = (height - divider_h) / 2;
    let divider_bottom = divider_top + divider_h;
    let (left, right) = if high_contrast {
        (
            theme::system_color(COLOR_WINDOWTEXT),
            theme::system_color(COLOR_GRAYTEXT),
        )
    } else if is_dark {
        (Color::new(62, 62, 62), Color::new(34, 34, 34))
    } else {
        (Color::new(176, 176, 176), Color::new(226, 226, 226))
    };
    fill_rect_color(
        hdc,
        &RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        },
        &left,
    );
    fill_rect_color(
        hdc,
        &RECT {
            left: sc(3),
            top: divider_top,
            right: sc(4),
            bottom: divider_bottom,
        },
        &right,
    );
}

fn paint_compact_surface(
    hdc: HDC,
    width: i32,
    height: i32,
    scene: &Scene,
    floating: bool,
    is_dark: bool,
    high_contrast: bool,
) {
    let palette = widget_palette(is_dark, high_contrast);
    fill_rect_color(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        &palette.bg,
    );
    let origin_x = if floating {
        sc(FLOATING_CONTENT_LEFT_MARGIN)
    } else {
        draw_drag_divider(hdc, height, is_dark, high_contrast);
        sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN)
    };
    render_compact_scene(hdc, scene, origin_x, 0, is_dark, high_contrast);
}

/// Register and create the hidden broadcast helper window (see
/// BROADCAST_WINDOW_CLASS_NAME). Never shown; lives for the whole process, so
/// broadcast handling survives widget destruction and revival.
unsafe fn create_broadcast_helper(hinstance: HINSTANCE) -> Option<HWND> {
    let class_name = native_interop::wide_str(BROADCAST_WINDOW_CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(broadcast_wnd_proc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        ..Default::default()
    };
    if RegisterClassExW(&wc) == 0 {
        diagnose::log("broadcast helper: RegisterClassExW failed");
        return None;
    }
    match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR::from_raw(class_name.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        HWND::default(),
        HMENU::default(),
        hinstance,
        None,
    ) {
        Ok(hwnd) => {
            let taskbar_created = native_interop::wide_str("TaskbarCreated");
            let message = RegisterWindowMessageW(PCWSTR::from_raw(taskbar_created.as_ptr()));
            if message != 0 {
                TASKBAR_CREATED_MSG.store(message, Ordering::Release);
            } else {
                diagnose::log("broadcast helper: unable to register TaskbarCreated");
            }
            if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
                diagnose::log("broadcast helper: WTS session registration failed");
            }
            diagnose::log(format!("broadcast helper created hwnd={:?}", hwnd));
            Some(hwnd)
        }
        Err(error) => {
            diagnose::log_error("broadcast helper: CreateWindowExW failed", error);
            None
        }
    }
}

unsafe fn handle_poll_timer() {
    let auth_watch = {
        let state = lock_state();
        state.as_ref().map(|s| {
            (
                s.auth_error_paused_polling,
                s.auth_watch_mode,
                s.auth_watch_snapshot.clone(),
            )
        })
    };
    match auth_watch {
        Some((true, watch_mode, previous_snapshot)) => {
            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
            if current_snapshot != previous_snapshot {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.auth_error_paused_polling && s.auth_watch_mode == watch_mode {
                        s.auth_watch_snapshot = current_snapshot;
                    }
                }
                drop(state);
                request_poll();
            }
        }
        Some((false, _, _)) => request_poll(),
        None => {}
    }
}

unsafe fn handle_reset_poll_timer() {
    let should_poll = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| !s.auth_error_paused_polling)
            .unwrap_or(false)
    };
    if should_poll {
        request_poll();
    }
}

unsafe fn handle_countdown_timer() {
    update_display();
    let main_hwnd = current_main_hwnd();
    if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
        render_layered();
    }
    refresh_floating_monitor(false);
    refresh_detail_popup_if_open();
    schedule_countdown_timer();
}

unsafe fn handle_usage_updated() {
    check_theme_change();
    check_language_change();

    let main_hwnd = current_main_hwnd();
    if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
        render_layered();
        position_at_taskbar();
        suppress_tray_reposition_for(Duration::from_millis(
            TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
        ));
        sync_tray_icons(main_hwnd);
    }
    schedule_countdown_timer();
    refresh_floating_monitor(false);
    refresh_detail_popup_if_open();
}

unsafe fn recover_shell_surfaces(reason: &str) {
    diagnose::log(format!("shell recovery requested: {reason}"));
    check_theme_change();
    check_language_change();

    let (main_hwnd, stored_taskbar, widget_visible) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd.to_hwnd(),
                s.taskbar_hwnd.map(|taskbar| taskbar.0 as isize),
                s.widget_visible,
            ),
            None => return,
        }
    };
    let binding_ok = stored_taskbar.is_some_and(|taskbar| {
        native_interop::is_embedded_in_taskbar(main_hwnd, HWND(taskbar as *mut _))
    });

    if binding_ok {
        position_at_taskbar();
        render_layered();
        sync_tray_icons(main_hwnd);
        if widget_visible {
            let _ = ShowWindow(main_hwnd, SW_SHOWNOACTIVATE);
        }
    } else {
        if IsWindow(main_hwnd).as_bool() {
            let _ = ShowWindow(main_hwnd, SW_HIDE);
        }
        revive_request();
    }
    refresh_floating_monitor(false);
    refresh_detail_popup_if_open();
}

unsafe fn handle_session_change(code: usize) {
    match code {
        WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOCK => {
            SESSION_INACTIVE.store(true, Ordering::Release);
            diagnose::log(format!(
                "session change {code}: shell re-embedding deferred; provider polling continues"
            ));
        }
        WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT | WTS_SESSION_UNLOCK => {
            SESSION_INACTIVE.store(false, Ordering::Release);
            recover_shell_surfaces("session restored");
        }
        _ => {}
    }
}

unsafe extern "system" fn broadcast_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        broadcast_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            diagnose::log(format!(
                "panic in broadcast_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe fn broadcast_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    match msg {
        // Setting/display broadcasts arrive in bursts (an RDP transition
        // fires dozens of WM_SETTINGCHANGE in a row). Trailing-edge debounce:
        // each message re-arms the timer, so the refresh work runs once,
        // shortly after the burst ends, against the final state - a leading
        // -edge throttle would act on an intermediate state and drop the
        // last message.
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED_MSG => {
            SetTimer(hwnd, TIMER_BROADCAST_DEBOUNCE, BROADCAST_DEBOUNCE_MS, None);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BROADCAST_DEBOUNCE => {
            let _ = KillTimer(hwnd, TIMER_BROADCAST_DEBOUNCE);
            recover_shell_surfaces("display/settings change");
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_POLL => {
            handle_poll_timer();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_RESET_POLL => {
            handle_reset_poll_timer();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_COUNTDOWN => {
            handle_countdown_timer();
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE_MSG => {
            handle_session_change(wparam.0);
            LRESULT(0)
        }
        _ if taskbar_created != 0 && msg == taskbar_created => {
            recover_shell_surfaces("TaskbarCreated");
            LRESULT(0)
        }
        _ if msg == WM_APP_USAGE_UPDATED => {
            handle_usage_updated();
            LRESULT(0)
        }
        // Revival ready signal, routed here instead of a thread message so a
        // modal message loop cannot discard it (see post_revive_ready).
        _ if msg == WM_APP_REVIVE_READY => {
            revive_execute();
            LRESULT(0)
        }
        _ if msg == WM_APP_REQUEST_QUIT => {
            let main_hwnd = {
                let state = lock_state();
                state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
            };
            request_quit(main_hwnd);
            LRESULT(0)
        }
        // A second launched instance asks us to surface the detail popup
        // (posted from run()'s single-instance guard).
        _ if msg == WM_APP_TRAY => {
            if lparam.0 as u32 == WM_LBUTTONUP {
                diagnose::log("broadcast helper: show-details request from second instance");
                show_usage_details(hwnd);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = WTSUnRegisterSessionNotification(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        set_default_dpi(GetDpiForSystem());
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running.
    // Exception: when relaunched after an explorer restart (ENV_RELAUNCH set),
    // wait for the previous instance to release the mutex, then take over.
    let is_relaunch = std::env::var(ENV_RELAUNCH).is_ok();
    let mutex_name = native_interop::wide_str("Global\\AIUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    if is_relaunch {
                        diagnose::log("relaunch: waiting for previous instance to exit");
                        // Retry instead of giving up: bailing out here used to
                        // leave no instance alive at all when the old process
                        // was slow to exit (issue: widget gone until reboot).
                        let mut acquired = false;
                        for attempt in 1..=3 {
                            let wait_result = WaitForSingleObject(h, 10_000);
                            if wait_result == WAIT_OBJECT_0 || wait_result == WAIT_ABANDONED {
                                acquired = true;
                                break;
                            }
                            diagnose::log(format!(
                                "relaunch: previous instance still alive after wait {attempt}/3 ({wait_result:?})"
                            ));
                        }
                        if !acquired {
                            diagnose::log(
                                "startup aborted: previous instance never released the mutex",
                            );
                            return;
                        }
                    } else {
                        // Give the double-launch visible feedback: ask the
                        // running instance (via its broadcast helper window)
                        // to show the usage detail popup, then bow out.
                        let helper_class = native_interop::wide_str(BROADCAST_WINDOW_CLASS_NAME);
                        match FindWindowW(PCWSTR::from_raw(helper_class.as_ptr()), PCWSTR::null()) {
                            Ok(existing) if existing != HWND::default() => {
                                let _ = PostMessageW(
                                    existing,
                                    WM_APP_TRAY,
                                    WPARAM(0),
                                    LPARAM(WM_LBUTTONUP as isize),
                                );
                                diagnose::log(
                                    "startup aborted: another instance is already running; asked it to show details",
                                );
                            }
                            _ => {
                                diagnose::log(
                                    "startup aborted: another instance is already running",
                                );
                            }
                        }
                        return;
                    }
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    UI_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let class_name = native_interop::wide_str(WINDOW_CLASS_NAME);

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("startup aborted: GetModuleHandleW failed", error);
                return;
            }
        };
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = settings::load();
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count = active_model_count(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
        );
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(initial_model_count),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("startup aborted: CreateWindowExW failed", error);
                return;
            }
        };

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let is_high_contrast = theme::is_high_contrast();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                is_high_contrast,
                embedded: false,
                language_override,
                language,
                install_channel,
                claude_widget: placeholder_widget(),
                codex_widget: placeholder_widget(),
                antigravity_widget: placeholder_widget(),
                compact_vm: compact_view::placeholder_model(
                    "--",
                    &settings.provider_order,
                    settings.show_claude_code,
                    settings.show_codex,
                    settings.show_antigravity,
                ),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
                provider_order: settings.provider_order.clone(),
                pending_provider_order: None,
                pending_provider_order_samples: 0,
                last_observed_tray_order: None,
                data: None,
                data_is_cached: false,
                last_error: None,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                last_success_unix: None,
                notify_session_reset: settings.notify_session_reset,
                notify_weekly_reset: settings.notify_weekly_reset,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                details_hwnd: None,
                floating_hwnd: None,
                floating_visible: settings.floating_visible,
                detailed_tray_icons: settings.detailed_tray_icons,
                floating_x: settings.floating_x,
                floating_y: settings.floating_y,
                widget_tooltip_hwnd: None,
                taskbar_index: settings.taskbar_index,
                tray_offset: settings.tray_offset,
                preferred_taskbar_index: settings.taskbar_index,
                preferred_tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_client_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
            });
        }

        // Broadcast helper: receives the top-level-only broadcast messages,
        // second-instance activation requests, and revival ready signals for
        // the process lifetime.
        if let Some(helper) = create_broadcast_helper(HINSTANCE(hinstance.0)) {
            BROADCAST_HELPER_HWND.store(helper.0 as isize, Ordering::SeqCst);
        } else {
            // Degraded fallback only: polling and WTS handling still work
            // while the main widget HWND survives.
            let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        }

        // Show the previous run's usage numbers immediately (marked as cached
        // in the detail popup) instead of "--" until the first poll lands.
        if let Some((cached_data, saved_unix)) = load_usage_cache() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.data = Some(cached_data);
                s.data_is_cached = true;
                s.last_poll_ok = true;
                s.last_success_unix = Some(saved_unix);
                refresh_usage_texts(s);
            }
            diagnose::log("loaded usage snapshot from previous run");
        }

        // Try to embed in taskbar
        if attach_to_taskbar(hwnd, settings.taskbar_index) {
            embedded = true;
        }

        // The taskbar widget is not a fallback desktop popup. If Explorer is
        // not ready yet, keep it hidden until verified re-embedding succeeds.
        if !embedded {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Registering our icons resizes the notification area asynchronously;
        // wait for its rect to settle so the first visible position is final
        // instead of being corrected (a visible jump) moments after showing.
        wait_for_tray_geometry_stable(Duration::from_secs(3));
        refresh_provider_order_from_tray(hwnd);
        SetTimer(hwnd, TIMER_TRAY_ORDER, TRAY_ORDER_SAMPLE_MS, None);

        // Position and render first, show last: the widget appears in its
        // final place with real content instead of flashing into view first.
        position_at_taskbar();
        render_layered();
        if settings.floating_visible {
            refresh_floating_monitor(false);
        }
        if settings.widget_visible && embedded {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log(if embedded {
            "taskbar widget ready"
        } else {
            "taskbar widget hidden pending shell recovery"
        });
        schedule_countdown_timer();

        // Provider polling belongs to the process-level helper so it survives
        // taskbar/RDP destruction of the embedded widget HWND.
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(poll_controller_hwnd(), TIMER_POLL, initial_poll_ms, None);

        // Watch for explorer.exe restarts so we can re-embed and re-add the tray
        // icon (the shell discards tray registrations when it restarts). This
        // runs on a dedicated thread, NOT a window timer: once explorer destroys
        // the taskbar, our embedded child window stops receiving all messages
        // (WM_TIMER included), so a timer would never fire again.
        spawn_taskbar_watchdog();

        // Initial poll
        diagnose::log("initial poll requested");
        request_poll();
        if !embedded {
            revive_request();
        }

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        if let Err(error) = updater::confirm_update_ready() {
            diagnose::log(format!(
                "unable to confirm successful update startup: {error}"
            ));
        }

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            // Thread messages (no window): revive after external destruction.
            // They cannot go through wnd_proc because the window is gone.
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REVIVE {
                revive_request();
                continue;
            }
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REQUEST_QUIT {
                let main_hwnd = {
                    let state = lock_state();
                    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
                };
                request_quit(main_hwnd);
                continue;
            }
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REVIVE_READY {
                revive_execute();
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        diagnose::log("message loop exited");
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    let (hwnd_val, is_dark, high_contrast, embedded, compact_vm) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.is_high_contrast,
                s.embedded,
                s.compact_vm.clone(),
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();
    let _dpi_scope = DpiScope::for_window(hwnd);
    let tooltip_scene = compact_scene_for_hwnd(hwnd, &compact_vm, high_contrast, false);
    sync_widget_tooltip_hits(&tooltip_scene);
    unsafe {
        refresh_widget_tooltip_if_visible(hwnd);
    }

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(Metrics::logical().taskbar_h);

    let palette = widget_palette(is_dark, high_contrast);

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;
        let scene = compact_scene(mem_dc, &compact_vm, high_contrast, false);

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_compact_surface(mem_dc, width, height, &scene, false, is_dark, high_contrast);

        // Background pixels -> alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels -> fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = palette.bg.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

fn poll_controller_hwnd() -> HWND {
    let helper = BROADCAST_HELPER_HWND.load(Ordering::Acquire);
    if helper != 0 {
        let hwnd = HWND(helper as *mut _);
        if unsafe { IsWindow(hwnd).as_bool() } {
            return hwnd;
        }
    }

    let state = lock_state();
    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
}

fn current_main_hwnd() -> HWND {
    let state = lock_state();
    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
}

fn post_usage_updated() {
    let hwnd = poll_controller_hwnd();
    if hwnd != HWND::default() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
        }
    }
}

fn request_poll() {
    request_poll_with(false);
}

fn request_poll_with(force_claude_refresh: bool) {
    if QUIT_REQUESTED.load(Ordering::Acquire) {
        return;
    }

    // Synchronize the generation bump with `do_poll` applying a result under
    // the same state lock. Once a worker verifies its generation while holding
    // this lock, no newer request can make that result stale mid-commit.
    let should_start_worker = {
        let _state = lock_state();
        POLL_COORDINATOR.request(force_claude_refresh)
    };
    if should_start_worker {
        std::thread::spawn(poll_worker);
    }
}

fn poll_worker() {
    loop {
        let (generation, force_claude_refresh) = POLL_COORDINATOR.begin_pass();
        do_poll(generation, force_claude_refresh);
        if !POLL_COORDINATOR.finish_pass() {
            break;
        }
        diagnose::log("poll request coalesced; starting pending refresh");
    }
}

fn do_poll(generation: u64, force_claude_refresh: bool) {
    let controller_hwnd = poll_controller_hwnd();
    let main_hwnd = current_main_hwnd();
    let (show_claude_code, show_codex, show_antigravity) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.show_claude_code, s.show_codex, s.show_antigravity))
            .unwrap_or((true, false, false))
    };

    match poller::poll(
        show_claude_code,
        show_codex,
        show_antigravity,
        force_claude_refresh,
    ) {
        Ok(mut data) => {
            let updated_unix = now_unix_secs();
            stamp_provider_updates(&mut data, updated_unix);
            let mut state = lock_state();
            if !POLL_COORDINATOR.is_current(generation) {
                diagnose::log(format!(
                    "discarded stale poll result generation={generation} current={}",
                    POLL_COORDINATOR.generation.load(Ordering::Acquire)
                ));
                return;
            }
            if let Some(s) = state.as_mut() {
                let rate_limited = data.rate_limited;
                let retry_after_ms = data.rate_limit_retry_after_ms;
                let merged = merge_missing_provider_data(
                    s.data.as_ref(),
                    data,
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                );
                // A cached previous snapshot is from an earlier run: any
                // reset that elapsed while the app was closed is old news,
                // not an event worth a balloon (pre-cache behavior: the
                // first poll of a run never notified).
                let reset_notifications = if s.data_is_cached {
                    Vec::new()
                } else {
                    collect_reset_notifications(
                        s.data.as_ref(),
                        &merged,
                        s.notify_session_reset,
                        s.notify_weekly_reset,
                        s.language.strings(),
                    )
                };

                // Mirror of the arming condition in schedule_countdown_timer:
                // the 5s reset fast poll must stop not only when every window
                // refreshed, but also when the only past-reset windows belong
                // to a failing provider - merge carries its stale section for
                // the whole outage, so app_is_past_reset alone never clears.
                if !healthy_provider_past_reset(&merged) {
                    unsafe {
                        let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(merged);
                s.data_is_cached = false;
                s.last_error = None;
                s.last_poll_ok = true;
                s.last_success_unix = Some(updated_unix);
                refresh_usage_texts(s);

                for notification in reset_notifications {
                    diagnose::log(format!("reset notification shown: {}", notification.body));
                    tray_icon::notify_balloon(
                        main_hwnd,
                        notification.kind,
                        &notification.title,
                        &notification.body,
                    );
                }

                if rate_limited {
                    s.retry_count = s.retry_count.saturating_add(1);
                    let retry_ms = rate_limit_retry_ms(retry_after_ms, s.poll_interval_ms);
                    diagnose::log(format!(
                        "rate limited provider preserved previous data; retrying poll in {}s",
                        retry_ms / 1000
                    ));
                    unsafe {
                        let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                        SetTimer(controller_hwnd, TIMER_POLL, retry_ms, None);
                    }
                } else {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        // Re-arm from completion instead of keeping the old
                        // periodic phase. Claude can then reach its 180s/120s
                        // eligibility deadline without sampling just before it.
                        SetTimer(controller_hwnd, TIMER_POLL, interval, None);
                    }
                }
                s.force_notify_auth_error = false;
                s.auth_error_paused_polling = false;
                s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                s.auth_watch_snapshot.clear();
            }

            // Persist the snapshot outside the lock so the next start can
            // show these numbers immediately.
            let cache_snapshot = state.as_ref().and_then(|s| s.data.clone());
            drop(state);
            if let Some(snapshot) = cache_snapshot.as_ref() {
                save_usage_cache(snapshot);
            }

            post_usage_updated();
        }
        Err(e) => {
            let auth_watch = credential_watch_mode_for_failure(
                e,
                show_claude_code,
                show_codex,
                show_antigravity,
            )
            .map(|watch_mode| (watch_mode, poller::credential_watch_snapshot(watch_mode)));
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                if !POLL_COORDINATOR.is_current(generation) {
                    diagnose::log(format!(
                        "discarded stale poll error generation={generation} current={}",
                        POLL_COORDINATOR.generation.load(Ordering::Acquire)
                    ));
                    return;
                }
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    s.last_error = Some(e);
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            set_widget_placeholders(s, "!");
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(controller_hwnd, TIMER_POLL);
                                let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(controller_hwnd, TIMER_COUNTDOWN);
                                SetTimer(controller_hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            if s.data.is_some() {
                                // Any transient failure (429, network blip,
                                // server error) keeps the last known numbers
                                // on screen - cached or live - instead of
                                // blanking them to "..."; the popup's status
                                // badges carry the failure. Re-derive the
                                // texts in case a manual refresh already
                                // replaced them with the loading placeholder.
                                s.last_poll_ok = true;
                                refresh_usage_texts(s);
                            } else {
                                set_widget_placeholders(s, "...");
                            }
                            s.retry_count = s.retry_count.saturating_add(1);
                            let retry_ms = match e {
                                poller::PollError::RateLimited(retry_after_ms) => {
                                    rate_limit_retry_ms(retry_after_ms, s.poll_interval_ms)
                                }
                                _ => {
                                    let backoff = RETRY_BASE_MS.saturating_mul(
                                        1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                                    );
                                    backoff.min(s.poll_interval_ms)
                                }
                            };
                            unsafe {
                                let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                                SetTimer(controller_hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        if s.show_claude_code {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Claude,
                                s.language.strings().token_expired_title,
                                s.language.strings().token_expired_body,
                            )
                        } else if s.show_codex {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Codex,
                                s.language.strings().codex_token_expired_title,
                                s.language.strings().codex_token_expired_body,
                            )
                        } else {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Antigravity,
                                s.language.strings().antigravity_token_expired_title,
                                s.language.strings().antigravity_token_expired_body,
                            )
                        }
                    })
                };
                if let Some((_strings, kind, title, body)) = balloon {
                    tray_icon::notify_balloon(main_hwnd, kind, title, body);
                }
            }

            post_usage_updated();
        }
    }
}

/// True when some provider that is currently healthy (no per-provider error
/// recorded by the last poll) has a quota window past its reset time - the
/// only case where the 5s reset fast poll actually helps.
fn healthy_provider_past_reset(data: &AppUsageData) -> bool {
    let check = |usage: Option<&UsageData>, error: Option<ProviderStatus>| {
        error.is_none() && usage.is_some_and(poller::is_past_reset)
    };
    check(data.claude_code.as_ref(), data.claude_code_error)
        || check(data.codex.as_ref(), data.codex_error)
        || check(data.antigravity.as_ref(), data.antigravity_error)
}

fn schedule_countdown_timer() {
    let controller_hwnd = poll_controller_hwnd();
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(controller_hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data - but
    // only when the past-reset provider itself is healthy. A failing
    // provider's carried-forward stale window also looks "past reset" for
    // the whole outage (merge keeps it), and fast-polling then would hammer
    // a broken endpoint (and rewrite the usage cache) at 5s cadence; the
    // retry/backoff timer owns that case.
    if healthy_provider_past_reset(data) && s.last_error.is_none() && !data.rate_limited {
        unsafe {
            SetTimer(controller_hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let min_delay = [
        data.claude_code.as_ref(),
        data.codex.as_ref(),
        data.antigravity.as_ref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(|usage| usage.windows.iter())
    .filter_map(|window| poller::time_until_display_change(window.resets_at))
    .min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(controller_hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let new_high_contrast = theme::is_high_contrast();
    let (changed, hwnd, floating_hwnd) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark || s.is_high_contrast != new_high_contrast {
                s.is_dark = new_dark;
                s.is_high_contrast = new_high_contrast;
                (true, Some(s.hwnd.to_hwnd()), s.floating_hwnd)
            } else {
                (false, None, None)
            }
        } else {
            (false, None, None)
        }
    };
    if changed {
        render_layered();
        if let Some(floating_hwnd) = floating_hwnd {
            unsafe {
                apply_floating_dwm_style(floating_hwnd, new_dark, new_high_contrast);
                let _ = InvalidateRect(floating_hwnd, None, false);
            }
        }
        // The tray icons and the detail popup follow the theme too.
        if let Some(hwnd) = hwnd {
            sync_tray_icons(hwnd);
        }
        refresh_detail_popup_if_open();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
        // Tray tooltips and the popup carry localized text too; without this
        // they would keep the old language until the next poll.
        let hwnd = {
            let state = lock_state();
            state.as_ref().map(|s| s.hwnd.to_hwnd())
        };
        if let Some(hwnd) = hwnd {
            sync_tray_icons(hwnd);
        }
        refresh_detail_popup_if_open();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

/// Wait briefly for the taskbar's notification area to stop moving before
/// the widget is positioned and shown for the first time. Registering our
/// own tray icons (and, right after sign-in, every other startup app's)
/// widens TrayNotifyWnd asynchronously; positioning against a rect that is
/// still changing is what made the widget visibly jump right after launch.
fn wait_for_tray_geometry_stable(max_wait: Duration) {
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + max_wait;
    let mut last: Option<(i32, i32, i32, i32)> = None;
    loop {
        let taskbar_hwnd = {
            let state = lock_state();
            state.as_ref().and_then(|s| s.taskbar_hwnd)
        };
        // Hidden/unembedded mode: nothing to wait for.
        let Some(taskbar_hwnd) = taskbar_hwnd else {
            return;
        };
        let current = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
            .and_then(native_interop::get_window_rect_safe)
            .or_else(|| native_interop::get_taskbar_rect(taskbar_hwnd))
            .map(|r| (r.left, r.top, r.right, r.bottom));
        if current.is_some() && current == last {
            return;
        }
        last = current;
        if Instant::now() + SAMPLE_INTERVAL > deadline {
            diagnose::log("tray geometry did not stabilize in time; positioning anyway");
            return;
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

fn position_at_taskbar() {
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, preferred_tray_offset, taskbar_hwnd) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag.
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            s.preferred_tray_offset,
            taskbar_hwnd,
        )
    };

    if unsafe { !IsWindow(hwnd).as_bool() } {
        diagnose::log(format!(
            "position_at_taskbar skipped: widget hwnd missing hwnd={:?}",
            hwnd
        ));
        let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            let _ = unsafe { PostThreadMessageW(thread_id, WM_APP_REVIVE, WPARAM(0), LPARAM(0)) };
        }
        return;
    }
    let _dpi_scope = DpiScope::for_window(hwnd);

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();
    let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
    let tray_offset = preferred_tray_offset.clamp(0, max_offset);
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.tray_offset = tray_offset;
        }
    }
    if tray_offset != preferred_tray_offset {
        diagnose::log(format!(
            "position anchor clamped preferred_offset={preferred_tray_offset} effective_offset={tray_offset} max_offset={max_offset}"
        ));
    }
    let widget_height = sc(WIDGET_HEIGHT);
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = tray_left - taskbar_rect.left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    } else {
        // Topmost popup: screen coordinates
        let x = tray_left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // A panic unwinding across this FFI boundary would abort the process;
    // recover and log instead.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_tray_location_changed_impl(hwnd)
    }))
    .is_err()
    {
        diagnose::log("panic in on_tray_location_changed (recovered)");
    }
}

fn on_tray_location_changed_impl(hwnd: HWND) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd || unsafe { IsChild(h, hwnd).as_bool() })
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > TRAY_ORDER_EVENT_THROTTLE_MS)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            let main_hwnd = {
                let state = lock_state();
                state.as_ref().map(|s| s.hwnd.to_hwnd())
            };
            if let Some(main_hwnd) = main_hwnd {
                if !refresh_provider_order_from_tray(main_hwnd) {
                    position_at_taskbar();
                    render_layered();
                }
            }
        }
    }
}

/// Main window procedure: panic guard around the real handler. A panic
/// unwinding across this FFI boundary would abort the process; recover, log,
/// and fall back to default handling for the offending message.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            diagnose::log(format!("panic in wnd_proc msg={msg:#06x} (recovered)"));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe fn wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_WTSSESSION_CHANGE_MSG => {
            handle_session_change(wparam.0);
            LRESULT(0)
        }
        WM_DPICHANGED_MSG => {
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            // lParam is a screen-space recommendation for top-level windows.
            // Once embedded, this HWND is a taskbar child and is laid out
            // after WM_DPICHANGED_AFTERPARENT instead.
            if !embedded {
                apply_suggested_dpi_rect(hwnd, lparam, "main widget");
            }
            position_at_taskbar();
            render_layered();
            diagnose::log(format!(
                "main widget: dpi changed dpi={new_dpi} embedded={embedded}"
            ));
            LRESULT(0)
        }
        WM_DPICHANGED_AFTERPARENT => {
            position_at_taskbar();
            render_layered();
            diagnose::log(format!(
                "main widget: parent dpi change applied dpi={}",
                GetDpiForWindow(hwnd)
            ));
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
                // The popup follows the system theme too; repaint if open.
                refresh_detail_popup_if_open();
            }
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    handle_poll_timer();
                }
                TIMER_COUNTDOWN => {
                    handle_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    handle_reset_poll_timer();
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                TIMER_TRAY_ORDER => {
                    refresh_provider_order_from_tray(hwnd);
                }
                TIMER_TRAY_ORDER_CONFIRM => {
                    let _ = KillTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM);
                    refresh_provider_order_from_tray(hwnd);
                }
                TIMER_WIDGET_TOOLTIP => {
                    show_widget_tooltip_for_hover(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            handle_usage_updated();
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if cursor_is_on_drag_handle(hwnd) {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            hide_widget_tooltip(hwnd, true);
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if !is_drag_handle_point(client_x, client_y) {
                return LRESULT(0);
            }

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.dragging = true;
                s.drag_start_mouse_x = pt.x;
                s.drag_start_client_x = client_x;
                s.drag_start_offset = s.tray_offset;
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            update_widget_tooltip_hover(hwnd, client_x, client_y);
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go right of default)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width_for_state(s);
                            let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;
                            s.preferred_tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = if embedded {
                                tray_left - taskbar_rect.left - widget_width - new_offset
                            } else {
                                tray_left - widget_width - new_offset
                            };
                            Some((
                                hwnd_val,
                                embedded,
                                x,
                                y,
                                taskbar_rect.top,
                                widget_width,
                                widget_height,
                            ))
                        } else {
                            s.tray_offset = new_offset;

                            s.preferred_tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;

                        s.preferred_tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            hide_widget_tooltip(hwnd, true);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let drag_result = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        Some((s.taskbar_index, s.drag_start_client_x))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((current_taskbar_index, drag_start_client_x)) = drag_result {
                let _ = ReleaseCapture();
                if let Some((target_index, target_taskbar)) = taskbar_at_point(pt) {
                    if target_index != current_taskbar_index {
                        let new_offset = offset_for_drop_point(
                            target_taskbar.hwnd,
                            target_taskbar.rect,
                            pt,
                            drag_start_client_x,
                        );
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.tray_offset = new_offset;
                                s.preferred_tray_offset = new_offset;
                                s.preferred_taskbar_index = target_index;
                            }
                        }
                        if attach_to_taskbar(hwnd, target_index) {
                            position_at_taskbar();
                            render_layered();
                        }
                    }
                }
                save_state_settings();
            } else {
                // Plain click on the widget body (not a drag): open the usage
                // detail popup - a far bigger click target than the tray icon.
                show_usage_details(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            hide_widget_tooltip(hwnd, true);
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_quit(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                IDM_REFRESH_NOW => {
                    trigger_manual_refresh(hwnd);
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    request_quit(hwnd);
                }
                IDM_RESET_POSITION => {
                    let default_taskbar_index = primary_taskbar_index();
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                            s.preferred_tray_offset = 0;
                            s.preferred_taskbar_index = default_taskbar_index;
                        }
                    }
                    save_state_settings();
                    if attach_to_taskbar(hwnd, default_taskbar_index) {
                        position_at_taskbar();
                        render_layered();
                    } else {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        revive_request();
                    }
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(poll_controller_hwnd(), TIMER_POLL, new_interval, None);
                }
                IDM_MODEL_CLAUDE_CODE | IDM_MODEL_CODEX | IDM_MODEL_ANTIGRAVITY => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_MODEL_CLAUDE_CODE => {
                                    if s.show_codex || s.show_antigravity || !s.show_claude_code {
                                        s.show_claude_code = !s.show_claude_code;
                                    }
                                }
                                IDM_MODEL_CODEX => {
                                    if s.show_claude_code || s.show_antigravity || !s.show_codex {
                                        s.show_codex = !s.show_codex;
                                    }
                                }
                                IDM_MODEL_ANTIGRAVITY => {
                                    if s.show_claude_code || s.show_codex || !s.show_antigravity {
                                        s.show_antigravity = !s.show_antigravity;
                                    }
                                }
                                _ => {}
                            }
                            set_widget_placeholders(s, "...");
                            s.pending_provider_order = None;
                            s.pending_provider_order_samples = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    refresh_floating_monitor(false);
                    sync_tray_icons(hwnd);
                    refresh_provider_order_from_tray(hwnd);
                    request_poll();
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_SIMPLIFIED_CHINESE
                | IDM_LANG_TRADITIONAL_CHINESE
                | IDM_LANG_RUSSIAN
                | IDM_LANG_PORTUGUESE_BRAZIL => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_SIMPLIFIED_CHINESE => Some(LanguageId::SimplifiedChinese),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        IDM_LANG_RUSSIAN => Some(LanguageId::Russian),
                        IDM_LANG_PORTUGUESE_BRAZIL => Some(LanguageId::PortugueseBrazil),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                    refresh_floating_monitor(false);
                }
                IDM_NOTIFY_SESSION_RESET | IDM_NOTIFY_WEEKLY_RESET => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_NOTIFY_SESSION_RESET => {
                                    s.notify_session_reset = !s.notify_session_reset;
                                }
                                IDM_NOTIFY_WEEKLY_RESET => {
                                    s.notify_weekly_reset = !s.notify_weekly_reset;
                                }
                                _ => {}
                            }
                        }
                    }
                    save_state_settings();
                }
                IDM_DETAILED_TRAY_ICONS => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.detailed_tray_icons = !s.detailed_tray_icons;
                            s.pending_provider_order = None;
                            s.pending_provider_order_samples = 0;
                        }
                    }
                    save_state_settings();
                    sync_tray_icons(hwnd);
                    position_at_taskbar();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                IDM_TOGGLE_FLOATING => {
                    toggle_floating_monitor();
                }
                IDM_RESET_FLOATING_POSITION => {
                    reset_floating_position();
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ShowDetails => {
                    show_usage_details(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu(kind) => {
                    show_context_menu(hwnd);
                    match kind {
                        Some(kind) => tray_icon::restore_focus(hwnd, kind),
                        None => tray_icon::restore_app_focus(hwnd),
                    }
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_WIDGET_TOOLTIP);
            {
                let mut runtime = lock_widget_tooltip_runtime();
                runtime.hover_kind = None;
                runtime.hits.clear();
                runtime.snapshot = None;
            }
            let (hook, tooltip) = {
                let mut state = lock_state();
                match state.as_mut() {
                    Some(s) => (s.win_event_hook.take(), s.widget_tooltip_hwnd.take()),
                    None => (None, None),
                }
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            if let Some(tooltip) = tooltip {
                let tooltip = tooltip.to_hwnd();
                if IsWindow(tooltip).as_bool() {
                    let _ = DestroyWindow(tooltip);
                }
            }
            let _ = WTSUnRegisterSessionNotification(hwnd);
            tray_icon::remove_all(hwnd);
            if QUIT_REQUESTED.load(Ordering::SeqCst) {
                PostQuitMessage(0);
            } else {
                // Nothing destroys the main widget window on purpose (the
                // detail popup manages its own DestroyWindow), so reaching
                // here means explorer destroyed our embedded child window
                // (taskbar rebuilt, or the hosting secondary taskbar vanished
                // after an RDP session switch). Upstream quit the process here
                // - the "widget gone until reboot" bug. Revive instead; the
                // thread message keeps the loop alive after this window dies.
                diagnose::log("window destroyed externally; scheduling in-process revival");
                let _ =
                    PostThreadMessageW(GetCurrentThreadId(), WM_APP_REVIVE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            floating_visible,
            detailed_tray_icons,
            show_claude_code,
            show_codex,
            show_antigravity,
            notify_session_reset,
            notify_weekly_reset,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.floating_visible,
                    s.detailed_tray_icons,
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.notify_session_reset,
                    s.notify_weekly_reset,
                ),
                None => (
                    POLL_15_MIN,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    false,
                    true,
                    true,
                    false,
                    false,
                    false,
                    false,
                ),
            }
        };

        // Menu creation can fail under GDI/USER handle pressure; skipping the
        // menu for one right-click beats aborting the whole process.
        let Ok(menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            return;
        };

        // Refresh submenu: immediate action first, followed by the interval.
        let Ok(refresh_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
        let refresh_now = native_interop::wide_str(strings.refresh_now);
        let _ = AppendMenuW(
            refresh_menu,
            MENU_ITEM_FLAGS(0),
            IDM_REFRESH_NOW as usize,
            PCWSTR::from_raw(refresh_now.as_ptr()),
        );
        let _ = AppendMenuW(refresh_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let freq_items: [(u16, u32, &str); 4] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                refresh_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let refresh_label = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            refresh_menu.0 as usize,
            PCWSTR::from_raw(refresh_label.as_ptr()),
        );

        // Models submenu
        let Ok(models_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
        let claude_model = native_interop::wide_str(strings.claude_code_model);
        let claude_flags = if show_claude_code {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            claude_flags,
            IDM_MODEL_CLAUDE_CODE as usize,
            PCWSTR::from_raw(claude_model.as_ptr()),
        );

        let codex_model = native_interop::wide_str(strings.codex_model);
        let codex_flags = if show_codex {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            codex_flags,
            IDM_MODEL_CODEX as usize,
            PCWSTR::from_raw(codex_model.as_ptr()),
        );

        let antigravity_model = native_interop::wide_str(strings.antigravity_model);
        let antigravity_flags = if show_antigravity {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            antigravity_flags,
            IDM_MODEL_ANTIGRAVITY as usize,
            PCWSTR::from_raw(antigravity_model.as_ptr()),
        );

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        // Settings submenu
        let Ok(settings_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let reset_widget_label = native_interop::wide_str(strings.reset_widget_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_widget_label.as_ptr()),
        );
        let reset_floating_label = native_interop::wide_str(strings.reset_floating_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_FLOATING_POSITION as usize,
            PCWSTR::from_raw(reset_floating_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let Ok(notifications_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let session_reset_label = native_interop::wide_str(strings.notify_session_reset);
        let session_reset_flags = if notify_session_reset {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            notifications_menu,
            session_reset_flags,
            IDM_NOTIFY_SESSION_RESET as usize,
            PCWSTR::from_raw(session_reset_label.as_ptr()),
        );
        let weekly_reset_label = native_interop::wide_str(strings.notify_weekly_reset);
        let weekly_reset_flags = if notify_weekly_reset {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            notifications_menu,
            weekly_reset_flags,
            IDM_NOTIFY_WEEKLY_RESET as usize,
            PCWSTR::from_raw(weekly_reset_label.as_ptr()),
        );
        let notifications_label = native_interop::wide_str(strings.notifications);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            notifications_menu.0 as usize,
            PCWSTR::from_raw(notifications_label.as_ptr()),
        );
        let Ok(language_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            // settings_menu is not attached to menu yet; destroy it separately.
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::SimplifiedChinese => IDM_LANG_SIMPLIFIED_CHINESE,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
                LanguageId::Russian => IDM_LANG_RUSSIAN,
                LanguageId::PortugueseBrazil => IDM_LANG_PORTUGUESE_BRAZIL,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if !updater::update_channel_configured()
            || matches!(
                update_status,
                UpdateStatus::Checking | UpdateStatus::Applying
            ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let floating_flags = if floating_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let detailed_icons_flags = if detailed_tray_icons {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let detailed_icons_label = native_interop::wide_str(strings.detailed_tray_icons);
        let _ = AppendMenuW(
            menu,
            detailed_icons_flags,
            IDM_DETAILED_TRAY_ICONS as usize,
            PCWSTR::from_raw(detailed_icons_label.as_ptr()),
        );
        let widget_label = native_interop::wide_str(strings.show_widget);
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );
        let floating_label = native_interop::wide_str(strings.show_floating_monitor);
        let _ = AppendMenuW(
            menu,
            floating_flags,
            IDM_TOGGLE_FLOATING as usize,
            PCWSTR::from_raw(floating_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn paint(hdc: HDC, hwnd: HWND) {
    let _dpi_scope = DpiScope::for_window(hwnd);
    let (is_dark, high_contrast, compact_vm, is_floating) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.is_high_contrast,
                s.compact_vm.clone(),
                s.floating_hwnd.is_some_and(|stored| stored.0 == hwnd.0),
            ),
            None => return,
        }
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);
        let scene = compact_scene(mem_dc, &compact_vm, high_contrast, is_floating);

        paint_compact_surface(
            mem_dc,
            width,
            height,
            &scene,
            is_floating,
            is_dark,
            high_contrast,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

#[cfg(test)]
mod reset_notification_tests {
    use super::*;

    struct OwnedIconInfoBitmaps {
        color: HBITMAP,
        mask: HBITMAP,
    }

    impl Drop for OwnedIconInfoBitmaps {
        fn drop(&mut self) {
            unsafe {
                if !self.color.is_invalid() {
                    let _ = DeleteObject(self.color);
                }
                if !self.mask.is_invalid() {
                    let _ = DeleteObject(self.mask);
                }
            }
        }
    }

    fn hicon_color_bitmap_metrics(hicon: HICON) -> (i32, i32, u16) {
        unsafe {
            let mut info = ICONINFO::default();
            GetIconInfo(hicon, &mut info).expect("GetIconInfo");
            let bitmaps = OwnedIconInfoBitmaps {
                color: info.hbmColor,
                mask: info.hbmMask,
            };
            assert!(
                !bitmaps.color.is_invalid(),
                "provider PNG should produce a color HBITMAP"
            );

            let mut bitmap = BITMAP::default();
            let copied = GetObjectW(
                bitmaps.color,
                std::mem::size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bitmap).cast()),
            );
            assert_eq!(
                copied,
                std::mem::size_of::<BITMAP>() as i32,
                "GetObjectW(BITMAP)"
            );

            (bitmap.bmWidth, bitmap.bmHeight, bitmap.bmBitsPixel)
        }
    }

    fn png_ihdr(bytes: &[u8]) -> (u32, u32, u8, u8) {
        assert!(bytes.len() >= 26);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&bytes[12..16], b"IHDR");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
            bytes[24],
            bytes[25],
        )
    }

    #[test]
    fn dpi_scaling_is_window_local_and_restored_after_nested_scope() {
        assert_eq!(scale_px_for_dpi(16, 96), 16);
        assert_eq!(scale_px_for_dpi(16, 120), 20);
        assert_eq!(scale_px_for_dpi(16, 144), 24);
        assert_eq!(scale_px_for_dpi(16, 192), 32);

        let baseline = sc(16);
        {
            let _outer = DpiScope::new(144);
            assert_eq!(sc(16), 24);
            {
                let _inner = DpiScope::new(192);
                assert_eq!(sc(16), 32);
            }
            assert_eq!(sc(16), 24);
        }
        assert_eq!(sc(16), baseline);
    }

    #[test]
    fn compact_high_contrast_text_roles_match_their_backdrops() {
        let system = |index| theme::system_color(index).to_colorref();
        let resolved = |key| compact_color(key, true, true).to_colorref();

        assert_eq!(
            resolved(ColorKey::PillAlertText),
            system(COLOR_HIGHLIGHTTEXT)
        );
        assert_eq!(
            resolved(ColorKey::CanvasWarnPrimary),
            system(COLOR_WINDOWTEXT)
        );
        assert_eq!(
            resolved(ColorKey::CanvasWarnSecondary),
            system(COLOR_WINDOWTEXT)
        );
        assert_eq!(
            resolved(ColorKey::HighContrastText),
            system(COLOR_WINDOWTEXT)
        );
        assert_eq!(resolved(ColorKey::ErrorText), system(COLOR_WINDOWTEXT));
        assert_eq!(resolved(ColorKey::PillBgWarn), system(COLOR_HIGHLIGHT));
        assert_eq!(resolved(ColorKey::PillBg), system(COLOR_WINDOW));

        assert_ne!(
            resolved(ColorKey::PillAlertText),
            resolved(ColorKey::PillBgWarn)
        );
        assert_ne!(
            resolved(ColorKey::CanvasWarnPrimary),
            resolved(ColorKey::PillBg)
        );
        assert_ne!(
            resolved(ColorKey::CanvasWarnSecondary),
            resolved(ColorKey::PillBg)
        );
        assert_ne!(resolved(ColorKey::ErrorText), resolved(ColorKey::PillBg));
    }

    #[test]
    fn suggested_dpi_rectangle_preserves_negative_monitor_coordinates() {
        let rect = RECT {
            left: -1920,
            top: -240,
            right: -1280,
            bottom: 480,
        };
        let parsed = suggested_dpi_rect(LPARAM(&rect as *const RECT as isize)).unwrap();

        assert_eq!(parsed.left, -1920);
        assert_eq!(parsed.top, -240);
        assert_eq!(parsed.right - parsed.left, 640);
        assert_eq!(parsed.bottom - parsed.top, 720);
    }

    #[test]
    fn provider_tray_tooltip_uses_one_quota_window_per_line() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(85.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(78.0, None, Some(ONE_WEEK_SECONDS)),
        ]);

        assert_eq!(
            provider_tooltip("Claude Code", Some(&usage), LanguageId::English.strings()),
            "Claude Code\n5h: 85%\n7d: 78%"
        );
        assert_eq!(
            app_tooltip_provider_line("Claude Code", Some(&usage), LanguageId::English.strings()),
            "Claude Code: 5h 85% · 7d 78%"
        );
    }

    #[test]
    fn widget_tooltip_lines_keep_every_reported_window() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(20.0, None, Some(ONE_WEEK_SECONDS)),
            UsageWindow::new(7.0, None, Some(24 * 60 * 60)),
        ]);
        let lines = provider_tooltip_lines(
            "Claude Code",
            usage.windows.iter(),
            LanguageId::English.strings(),
        );

        assert_eq!(lines, ["Claude Code", "5h: 53%", "1d: 7%", "7d: 20%"]);
    }

    #[test]
    fn widget_tooltip_reset_copy_is_relative_and_unwrapped() {
        let resets_at = SystemTime::now()
            .checked_add(Duration::from_secs(6 * 60 * 60 + 11 * 60))
            .unwrap();
        let text = widget_tooltip_reset_text(resets_at, LanguageId::English.strings());

        assert!(text.starts_with("Resets in "));
        assert!(!text.contains('('));
        assert!(!text.contains(')'));
    }

    #[test]
    fn widget_tooltip_position_tracks_all_taskbar_edges() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1_000,
            bottom: 700,
        };
        let size = (200, 80);

        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 400,
                    top: 700,
                    right: 500,
                    bottom: 740,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (350, 613)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 400,
                    top: -40,
                    right: 500,
                    bottom: 0,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (350, 7)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 1_000,
                    top: 300,
                    right: 1_040,
                    bottom: 400,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (793, 310)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: -40,
                    top: 300,
                    right: 0,
                    bottom: 400,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (7, 310)
        );
    }

    #[test]
    fn provider_tray_tooltip_puts_reset_details_in_parentheses() {
        let usage = UsageData::from_windows(vec![UsageWindow::new(
            85.0,
            SystemTime::now().checked_add(Duration::from_secs(23 * 60)),
            Some(FIVE_HOURS_SECONDS),
        )]);
        let tooltip = provider_tooltip("Claude Code", Some(&usage), LanguageId::English.strings());
        let lines = tooltip.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "Claude Code");
        assert!(lines[1].starts_with("5h: 85% (Resets in "));
        assert!(lines[1].ends_with("))"));
    }

    #[test]
    fn tray_tooltip_truncates_at_a_complete_utf16_character() {
        let long = format!("Status {}", "😀".repeat(100));
        let tooltip = tray_tooltip_from_lines([long.as_str()]);

        assert!(tooltip.encode_utf16().count() <= TRAY_TOOLTIP_MAX_UTF16);
        assert!(tooltip.ends_with('…'));
    }

    #[test]
    fn floating_drag_threshold_distinguishes_click_from_move() {
        let threshold = sc(FLOATING_DRAG_THRESHOLD);
        assert!(!floating_drag_distance_exceeded(
            threshold.saturating_sub(1),
            threshold.saturating_sub(1)
        ));
        assert!(floating_drag_distance_exceeded(threshold, 0));
        assert!(floating_drag_distance_exceeded(0, -threshold));
    }

    #[test]
    fn compact_gdi_data_font_fits_supported_dpis() {
        for dpi in PROVIDER_TILE_BUCKET_DPIS {
            let _dpi = DpiScope::new(dpi);
            unsafe {
                let hdc = GetDC(HWND::default());
                assert!(!hdc.0.is_null());
                let metrics = compact_metrics();
                assert!(
                    measure_compact_text(hdc, FontKey::Data12, "5h 100% ·4d") > 0,
                    "compact data font did not measure at {dpi} DPI"
                );
                assert!(
                    measure_compact_text(hdc, FontKey::Data12, "365d") <= metrics.label_max_w,
                    "floating label exceeds its measured slot at {dpi} DPI"
                );
                ReleaseDC(HWND::default(), hdc);
            }
        }
    }

    #[test]
    fn poll_coordinator_coalesces_requests_and_marks_old_generation_stale() {
        let coordinator = PollCoordinator::new();

        assert!(coordinator.request(false));
        let (first_generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
        assert!(coordinator.is_current(first_generation));

        assert!(!coordinator.request(false));
        assert!(!coordinator.request(true));
        assert!(!coordinator.is_current(first_generation));
        assert!(coordinator.finish_pass());

        let (latest_generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(force_claude_refresh);
        assert!(coordinator.is_current(latest_generation));
        assert!(!coordinator.finish_pass());

        assert!(coordinator.request(false));
    }

    #[test]
    fn poll_coordinator_releases_worker_when_no_request_is_pending() {
        let coordinator = PollCoordinator::new();

        assert!(coordinator.request(false));
        let (generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
        assert!(coordinator.is_current(generation));
        assert!(!coordinator.finish_pass());
        assert!(coordinator.request(false));
    }

    #[test]
    fn poll_coordinator_invalidation_discards_active_and_pending_work() {
        let coordinator = PollCoordinator::new();
        assert!(coordinator.request(false));
        let (generation, _) = coordinator.begin_pass();
        assert!(!coordinator.request(true));

        coordinator.invalidate_pending();

        assert!(!coordinator.is_current(generation));
        assert!(!coordinator.finish_pass());
        assert!(coordinator.request(false));
        let (_, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
    }

    #[test]
    fn watchdog_uses_the_live_parent_binding_not_a_transient_enumeration() {
        assert!(!watchdog_needs_taskbar_recovery(true, true, true));
        assert!(!watchdog_needs_taskbar_recovery(true, true, false));
        assert!(watchdog_needs_taskbar_recovery(true, false, true));
        assert!(watchdog_needs_taskbar_recovery(false, false, true));
        assert!(!watchdog_needs_taskbar_recovery(false, false, false));
    }

    #[test]
    fn credential_watch_mode_tracks_the_only_enabled_provider() {
        assert_eq!(
            credential_watch_mode_for_failure(poller::PollError::AuthRequired, false, true, false,),
            Some(poller::CredentialWatchMode::Codex)
        );
        assert_eq!(
            credential_watch_mode_for_failure(poller::PollError::AuthRequired, false, false, true,),
            Some(poller::CredentialWatchMode::Antigravity)
        );
        assert_eq!(
            credential_watch_mode_for_failure(poller::PollError::NoCredentials, true, false, false,),
            Some(poller::CredentialWatchMode::AllSources)
        );
    }

    #[test]
    fn credential_watch_mode_uses_all_providers_for_combined_auth_failure() {
        assert_eq!(
            credential_watch_mode_for_failure(poller::PollError::TokenExpired, true, true, true,),
            Some(poller::CredentialWatchMode::AllProviders)
        );
        assert_eq!(
            credential_watch_mode_for_failure(poller::PollError::RequestFailed, true, true, true,),
            None
        );
    }

    #[test]
    fn visible_reorder_preserves_hidden_provider_slot() {
        let full = vec![
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let visible = vec![
            tray_icon::TrayIconKind::Antigravity,
            tray_icon::TrayIconKind::Claude,
        ];

        assert_eq!(
            merge_visible_provider_order(&full, &visible),
            vec![
                tray_icon::TrayIconKind::Antigravity,
                tray_icon::TrayIconKind::Codex,
                tray_icon::TrayIconKind::Claude,
            ]
        );
    }

    #[test]
    fn provider_order_requires_a_fast_stable_confirmation() {
        let current = vec![
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let candidate = vec![
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let mut pending = None;
        let mut samples = 0;

        assert_eq!(
            observe_provider_order_candidate(&current, &candidate, &mut pending, &mut samples),
            ProviderOrderObservation::Pending
        );
        assert_eq!(samples, 1);
        assert_eq!(pending.as_deref(), Some(candidate.as_slice()));
        assert_eq!(
            observe_provider_order_candidate(&current, &candidate, &mut pending, &mut samples),
            ProviderOrderObservation::Apply
        );
        assert_eq!(samples, 0);
        assert!(pending.is_none());
    }

    fn window(resets_at: SystemTime) -> UsageWindow {
        UsageWindow::new(0.0, Some(resets_at), Some(FIVE_HOURS_SECONDS))
    }

    #[test]
    fn reset_window_refreshed_requires_elapsed_and_advanced_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_sub(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(reset_window_refreshed(
            &window(previous_reset),
            &window(next_reset)
        ));
    }

    #[test]
    fn reset_window_refreshed_ignores_predicted_future_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_add(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(!reset_window_refreshed(
            &window(previous_reset),
            &window(next_reset)
        ));
    }

    #[test]
    fn weekly_only_codex_usage_feeds_the_tray_percent() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(1.0, None, Some(ONE_WEEK_SECONDS))]);
        let widget = provider_widget_from_usage(Some(&usage));

        assert_eq!(widget.windows.len(), 1);
        assert_eq!(widget.windows[0].percent, Some(1.0));
    }

    #[test]
    fn widget_selects_two_most_used_windows_when_provider_has_more() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(91.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(92.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(10.0, None, Some(ONE_WEEK_SECONDS)),
        ]);
        let widget = provider_widget_from_usage(Some(&usage));

        assert_eq!(widget.windows.len(), 2);
        assert_eq!(widget.windows[0].percent, Some(91.0));
        assert_eq!(widget.windows[1].percent, Some(92.0));
    }

    #[test]
    fn compact_window_labels_stay_english_in_every_ui_language() {
        let five_hours = UsageWindow::new(0.0, None, Some(FIVE_HOURS_SECONDS));
        let seven_days = UsageWindow::new(0.0, None, Some(ONE_WEEK_SECONDS));
        let thirty_minutes = UsageWindow::new(0.0, None, Some(30 * 60));
        let strings = LanguageId::Korean.strings();

        assert_eq!(
            compact_view::compact_usage_window_label(&five_hours, strings),
            "5h"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&seven_days, strings),
            "7d"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&thirty_minutes, strings),
            "30m"
        );
        assert_eq!(usage_window_label(&five_hours, strings), "5시간");
    }

    #[test]
    fn detail_popup_keeps_all_provider_windows() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(10.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(20.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(30.0, None, Some(ONE_WEEK_SECONDS)),
        ]);
        let group = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            Some(&usage),
            None,
            false,
            LanguageId::English.strings(),
        );

        assert_eq!(group.rows.len(), 3);
        assert_eq!(group.rows[0].window_label, "5h");
        assert_eq!(group.rows[1].window_label, "1d");
        assert_eq!(group.rows[2].window_label, "7d");
    }

    #[test]
    fn detail_percent_rounding_and_warning_share_one_value() {
        for (percent, displayed, warns) in [
            (-1.0, 0, false),
            (89.4, 89, false),
            (89.6, 90, true),
            (100.4, 100, true),
        ] {
            assert_eq!(compact_view::display_percent(percent), displayed);
            let window = UsageWindow::new(percent, None, Some(FIVE_HOURS_SECONDS));
            let row = detail_usage_row(
                "5h".to_string(),
                Some(&window),
                None,
                5,
                LanguageId::English.strings(),
            );
            assert_eq!(row.warn, warns);
        }
    }

    #[test]
    fn detail_zero_tone_follows_the_displayed_percentage() {
        assert!(!detail_percent_is_display_zero(None));
        assert!(detail_percent_is_display_zero(Some(0.0)));
        assert!(detail_percent_is_display_zero(Some(0.4)));
        assert!(!detail_percent_is_display_zero(Some(0.5)));
    }

    #[test]
    fn detail_provider_badges_reflect_freshness_usage_and_errors() {
        let strings = LanguageId::English.strings();
        let group_for = |percent: f64, cached: bool| {
            let usage = UsageData::from_windows(vec![UsageWindow::new(
                percent,
                None,
                Some(FIVE_HOURS_SECONDS),
            )]);
            detail_provider_group(
                tray_icon::TrayIconKind::Claude,
                "Claude Code",
                Some(&usage),
                None,
                cached,
                strings,
            )
        };
        let assert_badge = |group: &DetailProviderGroup, text: &str, tone: DetailBadgeTone| {
            let badge = group.badge.as_ref().expect("provider badge");
            assert_eq!(badge.text, text);
            assert_eq!(badge.tone, tone);
        };

        assert_badge(&group_for(0.0, false), "Unused", DetailBadgeTone::Neutral);
        // A rounded display of 0% is not the same as no usage.
        assert_badge(&group_for(0.4, false), "Normal", DetailBadgeTone::Neutral);
        assert_badge(&group_for(1.0, false), "Normal", DetailBadgeTone::Neutral);
        assert_badge(&group_for(51.0, true), "Cached", DetailBadgeTone::Neutral);

        let cached_warning = group_for(92.0, true);
        assert_badge(&cached_warning, "Cached", DetailBadgeTone::Neutral);
        assert!(cached_warning.rows[0].warn);

        let near_limit = group_for(89.6, false);
        assert_badge(&near_limit, "Near limit", DetailBadgeTone::Critical);

        let usage =
            UsageData::from_windows(vec![UsageWindow::new(92.0, None, Some(FIVE_HOURS_SECONDS))]);
        for (error, expected, tone) in [
            (
                ProviderStatus::AuthRequired,
                "Sign in required",
                DetailBadgeTone::Critical,
            ),
            (
                ProviderStatus::RateLimited,
                "Rate limited",
                DetailBadgeTone::Degraded,
            ),
            (
                ProviderStatus::RequestFailed,
                "Request failed",
                DetailBadgeTone::Degraded,
            ),
        ] {
            let group = detail_provider_group(
                tray_icon::TrayIconKind::Claude,
                "Claude Code",
                Some(&usage),
                Some(error),
                true,
                strings,
            );
            assert_badge(&group, expected, tone);
        }

        let empty = UsageData::default();
        let loading = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&empty),
            None,
            false,
            strings,
        );
        assert_badge(&loading, "Loading", DetailBadgeTone::Neutral);
    }

    #[test]
    fn detail_rows_use_compact_labels_and_short_placeholder() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(1.0, None, Some(30 * 60)),
            UsageWindow::new(2.0, None, Some(365 * 24 * 60 * 60)),
        ]);
        let group = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            Some(&usage),
            None,
            false,
            LanguageId::Korean.strings(),
        );
        assert_eq!(group.rows[0].window_label, "30m");
        assert_eq!(group.rows[1].window_label, "365d");

        let loading = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            None,
            None,
            false,
            LanguageId::English.strings(),
        );
        assert_eq!(loading.rows[0].window_label, "--");
    }

    #[test]
    fn detail_dynamic_columns_clamp_and_keep_the_name_gap() {
        for dpi in [96, 120, 144, 192] {
            let _dpi = DpiScope::new(dpi);
            assert_eq!(detail_badge_width(0, DetailBadgeTone::Critical), sc(64));
            assert_eq!(
                detail_badge_width(sc(500), DetailBadgeTone::Critical),
                sc(104)
            );
            let badge_width = detail_badge_width(sc(80), DetailBadgeTone::Critical);
            let (badge_left, name_right) = detail_badge_horizontal_bounds(sc(378), badge_width);
            assert_eq!(badge_left - name_right, sc(8));
            assert!(name_right < badge_left);

            assert_eq!(detail_percent_column_width(0), sc(42));
            assert_eq!(detail_percent_column_width(sc(500)), sc(48));
        }

        let _dpi = DpiScope::new(96);
        unsafe {
            let hdc = GetDC(HWND::default());
            let auth_width = measure_detail_text_width(
                hdc,
                "Sign in required",
                "Segoe UI",
                11,
                FW_NORMAL.0 as i32,
            );
            let percent_width =
                measure_detail_text_width(hdc, "100%", "Segoe UI", 16, FW_SEMIBOLD.0 as i32);
            ReleaseDC(HWND::default(), hdc);

            assert!(detail_badge_width(auth_width, DetailBadgeTone::Critical) > sc(64));
            assert!(detail_percent_column_width(percent_width) >= percent_width);
        }
    }

    #[test]
    fn reference_card_height_and_popup_size_scale_as_one_layout() {
        let row = DetailUsageRow {
            window_label: "5h".to_string(),
            percent: Some(42.0),
            reset_text: "Resets in 2h".to_string(),
            dividers: 5,
            warn: false,
        };
        let group = DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Claude,
            name: "Claude Code".to_string(),
            badge: None,
            rows: vec![row.clone(), row],
        };
        let snapshot = DetailPopupState {
            title: "Gengchou".to_string(),
            providers: vec![group.clone()],
            status: "Updated now".to_string(),
            version: "test".to_string(),
        };

        assert_eq!(detail_group_height(&group), 152);
        let mut badged = group.clone();
        badged.badge = Some(DetailBadge {
            text: "Sign in required".to_string(),
            tone: DetailBadgeTone::Critical,
        });
        assert_eq!(detail_group_height(&badged), 152);

        for (dpi, expected) in [
            (96, (408, 258)),
            (120, (510, 323)),
            (144, (612, 387)),
            (192, (816, 516)),
        ] {
            let _dpi = DpiScope::new(dpi);
            assert_eq!(detail_popup_size(&snapshot), expected);
        }

        let full_snapshot = DetailPopupState {
            title: snapshot.title.clone(),
            providers: vec![
                group.clone(),
                DetailProviderGroup {
                    rows: vec![group.rows[0].clone()],
                    ..group.clone()
                },
                group,
            ],
            status: snapshot.status,
            version: snapshot.version,
        };
        let _dpi = DpiScope::new(120);
        assert_eq!(detail_popup_size(&full_snapshot), (510, 668));
    }

    #[test]
    fn detail_bar_cells_leave_gaps_only_between_segments() {
        for dpi in [96, 144, 192] {
            let _dpi = DpiScope::new(dpi);
            let rect = RECT {
                left: sc(13),
                top: sc(4),
                right: sc(213),
                bottom: sc(16),
            };

            for cell_count in [5, 7] {
                let cells: Vec<_> = (0..cell_count)
                    .map(|index| detail_bar_cell_rect(&rect, cell_count, index))
                    .collect();
                assert_eq!(cells.first().unwrap().left, rect.left);
                assert_eq!(cells.last().unwrap().right, rect.right);
                assert!(cells.iter().all(|cell| {
                    cell.top == rect.top && cell.bottom == rect.bottom && cell.right > cell.left
                }));
                assert!(cells
                    .windows(2)
                    .all(|pair| pair[1].left - pair[0].right == sc(DETAIL_BAR_GAP)));
            }
        }
    }

    #[test]
    fn bundled_provider_tiles_use_exact_png_and_hicon_sizes() {
        for dpi in PROVIDER_TILE_BUCKET_DPIS {
            let _dpi = DpiScope::new(dpi);
            for kind in [
                tray_icon::TrayIconKind::Claude,
                tray_icon::TrayIconKind::Codex,
                tray_icon::TrayIconKind::Antigravity,
            ] {
                for (tile_size, logical_size) in [
                    (TileSize::Chip16, 16),
                    (TileSize::Chip20, 20),
                    (TileSize::Chip28, DETAIL_LOGO_CHIP_SIZE),
                ] {
                    let asset = provider_tile_asset(kind, dpi, false, tile_size);
                    assert_eq!(asset.size, sc(logical_size));
                    assert_eq!(
                        png_ihdr(asset.bytes),
                        (asset.size as u32, asset.size as u32, 8, 6)
                    );

                    let (hicon, size) = provider_tile_icon(kind, dpi, false, false, tile_size)
                        .expect("provider tile");
                    assert_eq!(size, asset.size);
                    let (width, height, bits_per_pixel) = hicon_color_bitmap_metrics(hicon);
                    assert_eq!((width, height), (size, size));
                    assert_eq!(bits_per_pixel, 32);

                    let dark_asset = provider_tile_asset(kind, dpi, true, tile_size);
                    assert_eq!(
                        png_ihdr(dark_asset.bytes),
                        (dark_asset.size as u32, dark_asset.size as u32, 8, 6)
                    );
                    let (dark_hicon, dark_size) =
                        provider_tile_icon(kind, dpi, true, false, tile_size)
                            .expect("dark provider tile");
                    assert_eq!(dark_size, dark_asset.size);
                    let (dark_width, dark_height, dark_bits_per_pixel) =
                        hicon_color_bitmap_metrics(dark_hicon);
                    assert_eq!((dark_width, dark_height), (dark_size, dark_size));
                    assert_eq!(dark_bits_per_pixel, 32);
                    assert!(provider_tile_icon(kind, dpi, true, true, tile_size).is_none());
                }
            }
        }
    }

    #[test]
    fn provider_tile_bucket_selection_clamps_and_breaks_ties_downward() {
        for (index, dpi) in PROVIDER_TILE_BUCKET_DPIS.iter().enumerate() {
            assert_eq!(nearest_provider_tile_bucket(*dpi), index);
        }
        for (dpi, bucket) in [
            (0, 0),
            (72, 0),
            (108, 0),
            (109, 1),
            (132, 1),
            (133, 2),
            (156, 2),
            (157, 3),
            (180, 3),
            (181, 4),
            (240, 4),
        ] {
            assert_eq!(nearest_provider_tile_bucket(dpi), bucket, "dpi={dpi}");
        }
    }

    #[test]
    fn detail_duration_spacing_follows_the_writing_system() {
        let duration = 3 * 60 * 60 + 38 * 60;
        assert_eq!(
            detail_duration_from_secs(duration, LanguageId::English.strings()),
            "3h 38m"
        );
        assert_eq!(
            detail_duration_from_secs(duration, LanguageId::SimplifiedChinese.strings()),
            "3小时38分钟"
        );
        assert_eq!(detail_body_face("3小时38分钟后重置"), "Microsoft YaHei UI");
    }

    #[test]
    fn detail_drag_region_stays_clear_of_header_buttons() {
        let width = sc(DETAIL_POPUP_WIDTH);
        assert!(detail_header_is_draggable(sc(20), sc(20), width));

        for button in [
            detail_move_rect(width),
            detail_refresh_rect(width),
            detail_close_rect(width),
        ] {
            let x = (button.left + button.right) / 2;
            let y = (button.top + button.bottom) / 2;
            assert!(!detail_header_is_draggable(x, y, width));
        }
    }

    #[test]
    fn detail_poll_timing_advances_each_second() {
        let strings = LanguageId::English.strings();
        let first = detail_poll_timing_status(1_000, false, POLL_1_MIN, strings, 1_047);
        let second = detail_poll_timing_status(1_000, false, POLL_1_MIN, strings, 1_048);

        assert!(first.contains("Every 1m"));
        assert!(first.contains("next in 13s"));
        assert!(!first.contains("Updated"));
        assert!(second.contains("Every 1m"));
        assert!(second.contains("next in 12s"));

        let cached = detail_poll_timing_status(1_000, true, POLL_1_MIN, strings, 1_047);
        assert!(cached.contains("Data from last run"));
        assert!(cached.contains("Updated 47s ago"));
        assert!(!cached.contains("Every"));
        assert!(!cached.contains("next in"));
    }

    #[test]
    fn legacy_cache_drops_ghost_zero_window_and_preserves_weekly_usage() {
        let provider = UsageCacheProvider {
            updated_unix: None,
            windows: Vec::new(),
            session: Some(UsageCacheWindow::default()),
            weekly: Some(UsageCacheWindow {
                percent: 12.0,
                resets_unix: Some(1_800_000_000),
                ..Default::default()
            }),
        };

        let usage = usage_provider_from_cache(&provider);
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percentage, 12.0);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn dynamic_cache_round_trip_preserves_window_metadata() {
        let usage = UsageData::from_windows(vec![UsageWindow::new(
            7.0,
            Some(UNIX_EPOCH + Duration::from_secs(42)),
            None,
        )
        .with_source_label(Some("Quota".to_string()))]);

        let cache = usage_provider_to_cache(&usage, Some(123));
        let restored = usage_provider_from_cache(&cache);
        assert_eq!(cache.updated_unix, Some(123));
        assert_eq!(restored.windows.len(), 1);
        assert_eq!(restored.windows[0].percentage, 7.0);
        assert_eq!(restored.windows[0].resets_at, usage.windows[0].resets_at);
        assert_eq!(restored.windows[0].source_label.as_deref(), Some("Quota"));
    }

    #[test]
    fn provider_cache_uses_legacy_file_time_and_drops_stale_sections() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let legacy = usage_provider_to_cache(&usage, None);

        let (_, updated_unix) =
            fresh_cached_provider(Some(&legacy), 1_000, 1_001).expect("fresh legacy cache");
        assert_eq!(updated_unix, 1_000);
        assert!(
            fresh_cached_provider(Some(&legacy), 1_000, 1_000 + USAGE_CACHE_MAX_AGE_SECS + 1,)
                .is_none()
        );
    }

    #[test]
    fn partial_poll_preserves_failed_provider_freshness() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let previous = AppUsageData {
            claude_code: Some(usage.clone()),
            codex: Some(usage.clone()),
            claude_code_updated_unix: Some(100),
            codex_updated_unix: Some(100),
            ..Default::default()
        };
        let mut next = AppUsageData {
            codex: Some(usage),
            claude_code_error: Some(ProviderStatus::RateLimited),
            ..Default::default()
        };
        stamp_provider_updates(&mut next, 200);

        let merged = merge_missing_provider_data(Some(&previous), next, true, true, false);
        assert!(merged.claude_code.is_some());
        assert_eq!(merged.claude_code_updated_unix, Some(100));
        assert_eq!(merged.codex_updated_unix, Some(200));
    }
}
