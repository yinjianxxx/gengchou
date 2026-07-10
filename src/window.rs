use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
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

use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{AppUsageData, ProviderStatus, UsageData, UsageSection};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL, TIMER_UPDATE_CHECK, WM_APP_TRAY,
    WM_APP_USAGE_UPDATED,
};
use crate::poller;
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
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    antigravity_session_percent: f64,
    antigravity_session_text: String,
    antigravity_weekly_percent: f64,
    antigravity_weekly_text: String,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,

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

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;
const RATE_LIMIT_MIN_RETRY_MS: u32 = POLL_5_MIN;
const RATE_LIMIT_MAX_RETRY_MS: u32 = POLL_1_HOUR;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
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
/// WM_MOUSELEAVE (winuser.h); the windows crate gates it behind the
/// Win32_UI_Controls feature, which we do not otherwise need.
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
/// Timer on the broadcast helper window that coalesces setting/display
/// broadcast bursts into one refresh (trailing-edge debounce).
const TIMER_BROADCAST_DEBOUNCE: usize = 10;
const BROADCAST_DEBOUNCE_MS: u32 = 250;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
/// Thread message (msg.hwnd == null) handled directly in the message loop:
/// recreate/re-attach the widget window after it was destroyed externally.
const WM_APP_REVIVE: u32 = WM_APP + 4;
/// Thread message posted by the revival background thread once the taskbar
/// set is stable and the UI thread should recreate/re-attach the widget.
const WM_APP_REVIVE_READY: u32 = WM_APP + 5;
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

/// Revival tuning: how long to wait for the taskbar set to stop changing,
/// and how often/patiently to retry window creation before giving up.
const REVIVE_STABLE_WAIT_MAX_SECS: u64 = 120;
const REVIVE_CREATE_ATTEMPTS: u32 = 12;
const REVIVE_CREATE_RETRY_DELAY: Duration = Duration::from_secs(5);
/// A session disconnect/lock freezes revival and the watchdog for at most
/// this long; the matching reconnect normally clears it much earlier. The
/// cap matters because once our window is destroyed we can no longer receive
/// the WTS reconnect notification.
const SESSION_UNSTABLE_MAX_SECS: u64 = 30;

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

/// Unix time until which the session is considered unstable (RDP switch,
/// lock screen). 0 = stable.
static SESSION_UNSTABLE_UNTIL: AtomicU64 = AtomicU64::new(0);

/// UI thread id, so the watchdog can reach the message loop once the window
/// (the usual PostMessage target) no longer exists.
static UI_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// The Win32 window class; also part of the app's identity (kept distinct
/// from the original CodeZeno app so both can run side by side).
const WINDOW_CLASS_NAME: &str = "AIUsageMonitor";
const DETAIL_WINDOW_CLASS_NAME: &str = "AIUsageMonitorDetails";
/// Hidden top-level helper window. Two jobs the embedded widget cannot do
/// itself: receive broadcast messages (WM_SETTINGCHANGE / WM_DISPLAYCHANGE
/// are only sent to top-level windows, and the widget is a WS_CHILD of the
/// taskbar in its normal mode - without this a dark/light theme switch was
/// not reflected until the next poll), and be findable by class name so a
/// second launched instance can ask us to show the detail popup.
const BROADCAST_WINDOW_CLASS_NAME: &str = "AIUsageMonitorBroadcast";
const DETAIL_POPUP_WIDTH: i32 = 420;
/// Title area above the first provider group.
const DETAIL_HEADER_H: i32 = 50;
/// Provider header row: accent dot + provider name + status badge.
const DETAIL_GROUP_HEADER_H: i32 = 28;
/// One quota window: label/bar/percent line plus the reset line below it.
const DETAIL_WINDOW_ROW_H: i32 = 44;
const DETAIL_GROUP_GAP: i32 = 10;
const DETAIL_CONTENT_BOTTOM_PAD: i32 = 8;
const DETAIL_FOOTER_H: i32 = 46;
/// Content height when no provider rows exist yet (waiting message).
const DETAIL_EMPTY_H: i32 = 40;
/// A popup dismissed this recently is treated as "the user clicked the tray
/// icon to close it": the click that caused the focus loss also arrives as an
/// open request, and re-opening would make the popup flicker instead of
/// toggling.
const DETAIL_REOPEN_SUPPRESS_MS: u128 = 300;

fn session_is_unstable() -> bool {
    now_unix_secs() < SESSION_UNSTABLE_UNTIL.load(Ordering::SeqCst)
}

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
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
        // Only relevant once we have embedded into a taskbar at least once.
        let Some((hwnd, Some(old))) = stored else {
            continue;
        };
        let taskbars = native_interop::find_taskbars();
        let widget_missing = unsafe { !IsWindow(hwnd).as_bool() };
        let taskbar_changed =
            !taskbars.is_empty() && !taskbars.iter().any(|taskbar| taskbar.hwnd == old);
        if widget_missing || taskbar_changed {
            if widget_missing {
                diagnose::log(format!(
                    "watchdog: widget hwnd missing hwnd={:?} -> requesting revival",
                    hwnd
                ));
            }
            if taskbar_changed {
                diagnose::log(format!(
                    "watchdog: taskbar changed old={:?} new={:?} -> requesting revival",
                    old.0, taskbars[0].hwnd.0
                ));
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
                // Give the UI thread time to run the revival (it waits for a
                // stable taskbar) before re-checking.
                std::thread::sleep(Duration::from_secs(10));
            } else {
                diagnose::log("watchdog: UI thread unreachable -> relaunching");
                relaunch_self();
            }
        }
    });
}

/// Wait until the set of taskbars stops changing (two consecutive identical,
/// non-empty enumerations) or a hard deadline passes. Called on the UI thread
/// before reviving; during an RDP switch the taskbars are torn down and
/// rebuilt over several seconds, and acting mid-rebuild is what used to kill
/// the upstream app.
fn wait_for_stable_taskbar() {
    let deadline = Instant::now() + Duration::from_secs(REVIVE_STABLE_WAIT_MAX_SECS);
    let mut last: Option<Vec<isize>> = None;
    loop {
        if Instant::now() >= deadline {
            diagnose::log("revival: taskbar stability wait timed out; proceeding anyway");
            return;
        }
        if session_is_unstable() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        let current: Vec<isize> = native_interop::find_taskbars()
            .iter()
            .map(|taskbar| taskbar.hwnd.0 as isize)
            .collect();
        if !current.is_empty() && last.as_ref() == Some(&current) {
            return;
        }
        last = Some(current);
        std::thread::sleep(Duration::from_secs(2));
    }
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

/// First stage of revival, instant on the UI thread: mark a revival as in
/// flight and hand the potentially minutes-long taskbar-stability wait to a
/// background thread, so tray and menu interaction stay responsive while the
/// shell is still rebuilding. The background thread posts WM_APP_REVIVE_READY
/// back to the UI thread when it is time to act.
fn revive_request() {
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        return;
    }
    if REVIVING.swap(true, Ordering::SeqCst) {
        return; // another revival is already in flight
    }
    REVIVING_SINCE.store(now_unix_secs(), Ordering::SeqCst);
    REVIVE_ATTEMPTS.store(0, Ordering::SeqCst);
    diagnose::log("revival: begin (waiting for a stable taskbar in the background)");
    std::thread::spawn(|| {
        wait_for_stable_taskbar();
        post_revive_ready();
    });
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

/// Second stage of revival, on the UI thread with no long waits: bring the
/// widget back after explorer destroyed our window (or moved the taskbar out
/// from under us). Recreates the window if needed, re-embeds (or falls back
/// to a topmost popup), re-registers the tray icons and timers, and refreshes
/// the data - all in-process, keeping the current usage state. A failed
/// window creation retries via a delayed background re-post instead of
/// sleeping here; a full process relaunch remains the last resort.
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

    if !attach_to_taskbar(hwnd, preferred_taskbar_index) {
        // Same fallback as at startup: live as a topmost layered popup. Make
        // sure a previously embedded window is detached from the dead taskbar.
        diagnose::log("revival: no taskbar available; falling back to popup mode");
        native_interop::detach_to_popup(hwnd);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = false;
            }
        }
        position_fallback_popup(hwnd);
    }

    let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    sync_tray_icons(hwnd);
    // Position and render before showing so the revived widget reappears in
    // place with content instead of flashing in and being moved.
    position_at_taskbar();
    render_layered();
    if widget_visible {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    // Window timers died with the old window; re-arm them on the new one.
    let poll_ms = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.poll_interval_ms)
            .unwrap_or(POLL_15_MIN)
    };
    SetTimer(hwnd, TIMER_POLL, poll_ms, None);
    schedule_countdown_timer();
    schedule_auto_update_check(hwnd);

    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    std::thread::spawn(move || {
        do_poll(send_hwnd);
    });

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
    /// Short status shown right-aligned on the provider header line;
    /// the bool selects the warn colour (auth problems) over muted.
    badge: Option<(String, bool)>,
    rows: Vec<DetailUsageRow>,
}

#[derive(Clone)]
struct DetailUsageRow {
    window_label: &'static str,
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
/// Which header button the mouse is over: 0 none, 1 refresh, 2 close.
static DETAIL_HOVER: AtomicU8 = AtomicU8::new(0);
const DETAIL_HOVER_NONE: u8 = 0;
const DETAIL_HOVER_REFRESH: u8 = 1;
const DETAIL_HOVER_CLOSE: u8 = 2;

fn lock_detail_state() -> MutexGuard<'static, Option<DetailPopupState>> {
    DETAIL_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

const APP_DIR_NAME: &str = "AIUsageMonitor";
const LEGACY_APP_DIR_NAME: &str = "ClaudeCodexUsageMonitor";

fn settings_path_for(app_dir_name: &str) -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join(app_dir_name)
        .join("settings.json")
}

fn settings_path() -> PathBuf {
    settings_path_for(APP_DIR_NAME)
}

fn legacy_settings_path() -> PathBuf {
    settings_path_for(LEGACY_APP_DIR_NAME)
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default)]
    taskbar_index: usize,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    show_antigravity: bool,
    #[serde(default)]
    notify_session_reset: bool,
    #[serde(default)]
    notify_weekly_reset: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            taskbar_index: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            notify_session_reset: false,
            notify_weekly_reset: false,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
}

fn load_settings() -> SettingsFile {
    let (content, loaded_legacy) = match std::fs::read_to_string(settings_path()) {
        Ok(c) => (c, false),
        Err(_) => match std::fs::read_to_string(legacy_settings_path()) {
            Ok(c) => {
                diagnose::log(
                    "loaded legacy ClaudeCodexUsageMonitor settings for one-time migration",
                );
                (c, true)
            }
            Err(_) => return SettingsFile::default(),
        },
    };
    let mut settings: SettingsFile = serde_json::from_str(&content).unwrap_or_default();
    if !settings.show_claude_code && !settings.show_codex && !settings.show_antigravity {
        settings.show_claude_code = true;
    }
    if loaded_legacy {
        save_settings(&settings);
    }
    settings
}

/// Write via a temp file + rename so a crash mid-write can never leave a
/// truncated file behind (std::fs::rename replaces the target on Windows).
fn write_file_atomic(path: &PathBuf, contents: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, contents).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn save_settings(settings: &SettingsFile) {
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        write_file_atomic(&settings_path(), &json);
    }
}

const USAGE_CACHE_MAX_AGE_SECS: u64 = 48 * 60 * 60;

fn usage_cache_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join(APP_DIR_NAME)
        .join("usage-cache.json")
}

/// Snapshot of the last successful poll, persisted so a restart can show the
/// previous numbers immediately instead of "--" until the first poll lands.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheSection {
    percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_unix: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageCacheProvider {
    session: UsageCacheSection,
    weekly: UsageCacheSection,
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

fn usage_section_to_cache(section: &UsageSection) -> UsageCacheSection {
    UsageCacheSection {
        percent: section.percentage,
        resets_unix: section
            .resets_at
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    }
}

fn usage_section_from_cache(section: &UsageCacheSection) -> UsageSection {
    UsageSection {
        // The file is user-writable: a corrupt-but-parseable value must not
        // panic at startup (SystemTime + Duration panics on overflow) or
        // paint absurd percentages.
        percentage: section.percent.clamp(0.0, 100.0),
        resets_at: section
            .resets_unix
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
    }
}

fn usage_provider_to_cache(usage: &UsageData) -> UsageCacheProvider {
    UsageCacheProvider {
        session: usage_section_to_cache(&usage.session),
        weekly: usage_section_to_cache(&usage.weekly),
    }
}

fn usage_provider_from_cache(provider: &UsageCacheProvider) -> UsageData {
    UsageData {
        session: usage_section_from_cache(&provider.session),
        weekly: usage_section_from_cache(&provider.weekly),
    }
}

fn save_usage_cache(data: &AppUsageData) {
    let file = UsageCacheFile {
        saved_unix: now_unix_secs(),
        claude_code: data.claude_code.as_ref().map(usage_provider_to_cache),
        codex: data.codex.as_ref().map(usage_provider_to_cache),
        antigravity: data.antigravity.as_ref().map(usage_provider_to_cache),
    };
    if let Ok(json) = serde_json::to_string(&file) {
        write_file_atomic(&usage_cache_path(), &json);
    }
}

fn load_usage_cache() -> Option<(AppUsageData, u64)> {
    let content = std::fs::read_to_string(usage_cache_path()).ok()?;
    let file: UsageCacheFile = serde_json::from_str(&content).ok()?;
    if now_unix_secs().saturating_sub(file.saved_unix) > USAGE_CACHE_MAX_AGE_SECS {
        return None;
    }
    let data = AppUsageData {
        claude_code: file.claude_code.as_ref().map(usage_provider_from_cache),
        codex: file.codex.as_ref().map(usage_provider_from_cache),
        antigravity: file.antigravity.as_ref().map(usage_provider_from_cache),
        ..Default::default()
    };
    if data.claude_code.is_none() && data.codex.is_none() && data.antigravity.is_none() {
        return None;
    }
    Some((data, file.saved_unix))
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.preferred_tray_offset,
            taskbar_index: s.preferred_taskbar_index,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            show_antigravity: s.show_antigravity,
            notify_session_reset: s.notify_session_reset,
            notify_weekly_reset: s.notify_weekly_reset,
        });
    }
}

fn tray_icon_data_from_state() -> Vec<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: Some(s.session_percent),
                    weekly_percent: Some(s.weekly_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().claude_code_model,
                        s.session_text,
                        s.weekly_text
                    ),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: Some(s.codex_session_percent),
                    weekly_percent: Some(s.codex_weekly_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().codex_model,
                        s.codex_session_text,
                        s.codex_weekly_text
                    ),
                });
            }
            if s.show_antigravity {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Antigravity,
                    percent: Some(s.antigravity_session_percent),
                    weekly_percent: Some(s.antigravity_weekly_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().antigravity_model,
                        s.antigravity_session_text,
                        s.antigravity_weekly_text
                    ),
                });
            }
            icons
        }
        Some(s) => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: None,
                    weekly_percent: None,
                    tooltip: s.language.strings().window_title.to_string(),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: None,
                    weekly_percent: None,
                    tooltip: s.language.strings().codex_window_title.to_string(),
                });
            }
            if s.show_antigravity {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Antigravity,
                    percent: None,
                    weekly_percent: None,
                    tooltip: s.language.strings().antigravity_window_title.to_string(),
                });
            }
            icons
        }
        None => Vec::new(),
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icons = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons);
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn attach_to_taskbar(hwnd: HWND, requested_index: usize) -> bool {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        diagnose::log("taskbar not found; using fallback popup window");
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

    native_interop::embed_in_taskbar(hwnd, taskbar.hwnd);

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

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session, strings);
        state.weekly_text = poller::format_line(&claude_code.weekly, strings);
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_text = poller::format_line(&codex.session, strings);
        state.codex_weekly_text = poller::format_line(&codex.weekly, strings);
    }

    if let Some(antigravity) = data.antigravity.as_ref() {
        state.antigravity_session_text = poller::format_line(&antigravity.session, strings);
        state.antigravity_weekly_text =
            if antigravity.weekly.resets_at.is_none() && antigravity.weekly.percentage == 0.0 {
                "--".to_string()
            } else {
                poller::format_line(&antigravity.weekly, strings)
            };
    }
}

fn apply_usage_percents(state: &mut AppState, data: &AppUsageData) {
    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_percent = claude_code.session.percentage;
        state.weekly_percent = claude_code.weekly.percentage;
    }
    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_percent = codex.session.percentage;
        state.codex_weekly_percent = codex.weekly.percentage;
    }
    if let Some(antigravity) = data.antigravity.as_ref() {
        state.antigravity_session_percent = antigravity.session.percentage;
        state.antigravity_weekly_percent = antigravity.weekly.percentage;
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
        }
        if show_codex && next.codex.is_none() {
            next.codex = previous.codex.clone();
        }
        if show_antigravity && next.antigravity.is_none() {
            next.antigravity = previous.antigravity.clone();
        }
    }
    next
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

    if notify_session_reset && reset_window_refreshed(&previous.session, &next.session) {
        notifications.push(make_reset_notification(
            kind,
            provider_label,
            strings.session_window,
            strings,
        ));
    }
    if notify_weekly_reset && reset_window_refreshed(&previous.weekly, &next.weekly) {
        notifications.push(make_reset_notification(
            kind,
            provider_label,
            strings.weekly_window,
            strings,
        ));
    }
}

fn reset_window_refreshed(previous: &UsageSection, next: &UsageSection) -> bool {
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
        .max(RATE_LIMIT_MIN_RETRY_MS)
        .min(RATE_LIMIT_MAX_RETRY_MS)
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

/// Reset every provider's text to the loading placeholder and kick off a
/// poll. Shared by the context-menu Refresh entry and the detail popup's
/// refresh button.
fn trigger_manual_refresh(hwnd: HWND) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.session_text = "...".to_string();
            s.weekly_text = "...".to_string();
            s.codex_session_text = "...".to_string();
            s.codex_weekly_text = "...".to_string();
            s.antigravity_session_text = "...".to_string();
            s.antigravity_weekly_text = "...".to_string();
            s.force_notify_auth_error = true;
        }
    }
    render_layered();
    let sh = SendHwnd::from_hwnd(hwnd);
    std::thread::spawn(move || {
        do_poll(sh);
    });
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
    let (width, height) = detail_popup_size(&snapshot);
    let title = snapshot.title.clone();
    let (x, y) = unsafe { detail_popup_position(width, height) };

    {
        let mut detail_state = lock_detail_state();
        *detail_state = Some(snapshot);
    }

    let existing = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };

    unsafe {
        if let Some(detail_hwnd) = existing {
            if IsWindow(detail_hwnd).as_bool() {
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

        diagnose::log(format!("detail popup: created hwnd={:?}", detail_hwnd));
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
            title: "AI Usage Monitor".to_string(),
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
    strings: Strings,
) -> DetailProviderGroup {
    let badge = match error {
        Some(ProviderStatus::AuthRequired) => {
            Some((strings.detail_auth_required.to_string(), true))
        }
        Some(ProviderStatus::RateLimited) => {
            Some((strings.detail_badge_rate_limited.to_string(), false))
        }
        Some(ProviderStatus::RequestFailed) => {
            Some((strings.detail_badge_error.to_string(), false))
        }
        None if usage.is_none() => Some((strings.detail_badge_loading.to_string(), false)),
        None => None,
    };

    DetailProviderGroup {
        kind,
        name: name.to_string(),
        badge,
        rows: vec![
            detail_usage_row(
                strings.session_window,
                usage.map(|usage| &usage.session),
                error,
                5,
                strings,
            ),
            detail_usage_row(
                strings.weekly_window,
                usage.map(|usage| &usage.weekly),
                error,
                7,
                strings,
            ),
        ],
    }
}

fn detail_usage_row(
    window_label: &'static str,
    section: Option<&UsageSection>,
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
                Some(resets_at) => detail_reset_line(resets_at, strings),
            },
        }
    };

    DetailUsageRow {
        window_label,
        percent,
        reset_text,
        dividers,
        warn: percent.unwrap_or(0.0) >= 90.0,
    }
}

/// "Resets in 2h 13m (21:30)" - relative countdown plus the absolute local
/// time, which is what people actually plan around (especially for 7d).
fn detail_reset_line(resets_at: SystemTime, strings: Strings) -> String {
    match resets_at.duration_since(SystemTime::now()) {
        Ok(duration) if duration.as_secs() > 0 => {
            let mut text = strings
                .detail_resets_in
                .replace("{duration}", &detail_duration_text(duration, strings));
            if let Some(at) = format_local_time(resets_at, strings) {
                text.push_str(" (");
                text.push_str(&at);
                text.push(')');
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

    let elapsed = now_unix_secs().saturating_sub(last_success_unix);
    let updated = strings
        .detail_updated_ago
        .replace("{ago}", &detail_duration_from_secs(elapsed, strings));
    let mut status = if state.data_is_cached {
        format!("{} · {updated}", strings.detail_stale)
    } else {
        updated
    };

    let interval_secs = (state.poll_interval_ms / 1000) as u64;
    status.push_str(" · ");
    status.push_str(&strings.detail_poll_every.replace(
        "{interval}",
        &detail_duration_from_secs(interval_secs, strings),
    ));
    if !state.data_is_cached && interval_secs > elapsed {
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

    let total_minutes = ((total_secs + 59) / 60).max(1);
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        if hours > 0 {
            format!(
                "{}{} {}{}",
                days, strings.day_suffix, hours, strings.hour_suffix
            )
        } else {
            format!("{}{}", days, strings.day_suffix)
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!(
                "{}{} {}{}",
                hours, strings.hour_suffix, minutes, strings.minute_suffix
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
    DETAIL_GROUP_HEADER_H + group.rows.len() as i32 * DETAIL_WINDOW_ROW_H
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
    match msg {
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
            {
                let mut last_dismiss = DETAIL_LAST_DISMISS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *last_dismiss = Some(Instant::now());
            }
            DETAIL_HOVER.store(DETAIL_HOVER_NONE, Ordering::SeqCst);
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

fn point_in_rect(x: i32, y: i32, rect: &RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn detail_close_rect(width: i32) -> RECT {
    RECT {
        left: width - sc(42),
        top: sc(12),
        right: width - sc(16),
        bottom: sc(38),
    }
}

fn detail_refresh_rect(width: i32) -> RECT {
    RECT {
        left: width - sc(74),
        top: sc(12),
        right: width - sc(48),
        bottom: sc(38),
    }
}

fn paint_detail_popup(hdc: HDC, hwnd: HWND) {
    let snapshot = {
        let detail_state = lock_detail_state();
        detail_state.clone().unwrap_or_else(|| DetailPopupState {
            title: "AI Usage Monitor".to_string(),
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
    border: Color,
    divider: Color,
    text: Color,
    muted: Color,
    warn: Color,
    track: Color,
}

fn detail_palette(is_dark: bool) -> DetailPalette {
    if is_dark {
        DetailPalette {
            bg: Color::from_hex("#1F1F1F"),
            border: Color::from_hex("#2E2E2E"),
            divider: Color::from_hex("#343434"),
            text: Color::from_hex("#F3F4F6"),
            muted: Color::from_hex("#9CA3AF"),
            warn: Color::from_hex("#F87171"),
            track: Color::from_hex("#3A3A3A"),
        }
    } else {
        DetailPalette {
            bg: Color::from_hex("#F9F9F9"),
            border: Color::from_hex("#D4D4D8"),
            divider: Color::from_hex("#E4E4E7"),
            text: Color::from_hex("#1B1B1F"),
            muted: Color::from_hex("#6B7280"),
            warn: Color::from_hex("#DC2626"),
            track: Color::from_hex("#E4E4E7"),
        }
    }
}

fn provider_accent(kind: tray_icon::TrayIconKind, is_dark: bool) -> Color {
    match kind {
        tray_icon::TrayIconKind::Claude => claude_accent_color(),
        tray_icon::TrayIconKind::Codex => codex_accent_color(is_dark),
        tray_icon::TrayIconKind::Antigravity => antigravity_accent_color(),
    }
}

fn paint_detail_content(hdc: HDC, width: i32, height: i32, snapshot: &DetailPopupState) {
    let is_dark = theme::is_dark_mode();
    let palette = detail_palette(is_dark);

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

        let margin = sc(20);
        draw_detail_text(
            hdc,
            &snapshot.title,
            RECT {
                left: margin,
                top: sc(14),
                right: width - sc(84),
                bottom: sc(40),
            },
            &palette.text,
            20,
            FW_BOLD.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        // Header buttons: refresh + close, with a hover backplate.
        let hover = DETAIL_HOVER.load(Ordering::SeqCst);
        let refresh_rect = detail_refresh_rect(width);
        let close_rect = detail_close_rect(width);
        if hover == DETAIL_HOVER_REFRESH {
            draw_rounded_rect(hdc, &refresh_rect, &palette.divider, sc(4));
        }
        if hover == DETAIL_HOVER_CLOSE {
            draw_rounded_rect(hdc, &close_rect, &palette.divider, sc(4));
        }
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
            draw_detail_text(
                hdc,
                waiting,
                RECT {
                    left: margin,
                    top: y,
                    right: width - margin,
                    bottom: y + sc(DETAIL_EMPTY_H),
                },
                &palette.muted,
                14,
                FW_NORMAL.0 as i32,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        } else {
            for group in &snapshot.providers {
                draw_detail_group(hdc, width, y, group, &palette, is_dark);
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
        draw_detail_text(
            hdc,
            &snapshot.status,
            RECT {
                left: margin,
                top: footer_top + sc(12),
                right: width - sc(74),
                bottom: height - sc(10),
            },
            &palette.muted,
            13,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_text(
            hdc,
            &format!("v{}", snapshot.version),
            RECT {
                left: width - sc(74),
                top: footer_top + sc(12),
                right: width - margin,
                bottom: height - sc(10),
            },
            &palette.muted,
            13,
            FW_NORMAL.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// One provider: a header line (accent dot, provider name, status badge)
/// followed by one indented line pair per quota window.
fn draw_detail_group(
    hdc: HDC,
    width: i32,
    group_y: i32,
    group: &DetailProviderGroup,
    palette: &DetailPalette,
    is_dark: bool,
) {
    let margin = sc(20);
    let indent = margin + sc(18);
    let accent = provider_accent(group.kind, is_dark);

    // Accent dot, vertically centred on the header line.
    let dot = sc(10);
    let dot_top = group_y + (sc(DETAIL_GROUP_HEADER_H) - dot) / 2;
    let dot_rect = RECT {
        left: margin,
        top: dot_top,
        right: margin + dot,
        bottom: dot_top + dot,
    };
    draw_rounded_rect(hdc, &dot_rect, &accent, dot / 2);

    draw_detail_text(
        hdc,
        &group.name,
        RECT {
            left: indent,
            top: group_y,
            right: width / 2 + sc(40),
            bottom: group_y + sc(DETAIL_GROUP_HEADER_H),
        },
        &palette.text,
        15,
        FW_SEMIBOLD.0 as i32,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    if let Some((badge, badge_warn)) = &group.badge {
        draw_detail_text(
            hdc,
            badge,
            RECT {
                left: width / 2,
                top: group_y,
                right: width - margin,
                bottom: group_y + sc(DETAIL_GROUP_HEADER_H),
            },
            if *badge_warn {
                &palette.warn
            } else {
                &palette.muted
            },
            12,
            FW_NORMAL.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    let mut row_y = group_y + sc(DETAIL_GROUP_HEADER_H);
    for row in &group.rows {
        let percent_text = match row.percent {
            Some(percent) => format!("{percent:.0}%"),
            None => "--".to_string(),
        };
        draw_detail_text(
            hdc,
            row.window_label,
            RECT {
                left: indent,
                top: row_y,
                right: indent + sc(30),
                bottom: row_y + sc(20),
            },
            &palette.muted,
            13,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        draw_detail_text(
            hdc,
            &percent_text,
            RECT {
                left: width - margin - sc(52),
                top: row_y,
                right: width - margin,
                bottom: row_y + sc(20),
            },
            if row.warn {
                &palette.warn
            } else {
                &palette.text
            },
            13,
            FW_NORMAL.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_rect = RECT {
            left: indent + sc(34),
            top: row_y + sc(5),
            right: width - margin - sc(58),
            bottom: row_y + sc(15),
        };
        draw_detail_bar(
            hdc,
            &bar_rect,
            row.percent.unwrap_or(0.0),
            if row.warn { &palette.warn } else { &accent },
            &palette.track,
            row.dividers,
            &palette.bg,
        );

        draw_detail_text(
            hdc,
            &row.reset_text,
            RECT {
                left: indent + sc(34),
                top: row_y + sc(21),
                right: width - margin,
                bottom: row_y + sc(40),
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        row_y += sc(DETAIL_WINDOW_ROW_H);
    }
}

fn draw_detail_bar(
    hdc: HDC,
    rect: &RECT,
    percent: f64,
    accent: &Color,
    track: &Color,
    dividers: i32,
    bg: &Color,
) {
    unsafe {
        let radius = sc(2);
        draw_rounded_rect(hdc, rect, track, radius);

        let fill_width =
            ((rect.right - rect.left) as f64 * percent.clamp(0.0, 100.0) / 100.0).round() as i32;
        if fill_width > 0 {
            let fill_rect = RECT {
                left: rect.left,
                top: rect.top,
                right: (rect.left + fill_width).min(rect.right),
                bottom: rect.bottom,
            };
            let rgn = CreateRoundRectRgn(
                rect.left,
                rect.top,
                rect.right + 1,
                rect.bottom + 1,
                radius * 2,
                radius * 2,
            );
            let _ = SelectClipRgn(hdc, rgn);
            fill_rect_color(hdc, &fill_rect, accent);
            let _ = SelectClipRgn(hdc, HRGN::default());
            let _ = DeleteObject(rgn);
        }

        if dividers > 1 {
            let divider_color = *bg;
            for i in 1..dividers {
                let x = rect.left + ((rect.right - rect.left) * i) / dividers;
                fill_rect_color(
                    hdc,
                    &RECT {
                        left: x - sc(1),
                        top: rect.top,
                        right: x,
                        bottom: rect.bottom,
                    },
                    &divider_color,
                );
            }
        }
    }
}

/// Cache of fonts keyed by (face, pixel height, weight), shared by the widget
/// and the detail popup. GDI fonts are cheap but both surfaces repaint on
/// every poll refresh; a handful of cached handles (a few per DPI the window
/// has lived at) beats create/destroy per frame.
static FONT_CACHE: Mutex<Vec<((&'static str, i32, i32), isize)>> = Mutex::new(Vec::new());

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
    let (strings, install_channel) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                show_info_message(
                    hwnd,
                    app_state.language.strings().updates,
                    app_state.language.strings().update_in_progress,
                );
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        (app_state.language.strings(), app_state.install_channel)
    };

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
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
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
        Ok(()) => unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
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

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const TEXT_WIDTH: i32 = 62;
const MODEL_RIGHT_MARGIN: i32 = 3;
const RIGHT_MARGIN: i32 = 1;
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

fn row_bar_segment_count(active_models: i32) -> i32 {
    match active_models {
        1 => SEGMENT_COUNT,
        2 => 5,
        _ => 4,
    }
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let bar_segments = row_bar_segment_count(active_models);
    let model_width = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * bar_segments - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH);

    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + sc(LABEL_WIDTH)
        + sc(LABEL_RIGHT_MARGIN)
        + model_width * active_models
        + sc(MODEL_RIGHT_MARGIN) * (active_models - 1)
        + sc(RIGHT_MARGIN)
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    total_widget_width_for(active_model_count(
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
    ))
}

fn total_widget_width() -> i32 {
    let active_models = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| active_model_count(s.show_claude_code, s.show_codex, s.show_antigravity))
            .unwrap_or(1)
    };
    total_widget_width_for(active_models)
}

fn claude_accent_color() -> Color {
    Color::from_hex("#D97757")
}

fn codex_accent_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_accent_color() -> Color {
    Color::from_hex("#4285F4")
}

fn claude_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F09A7A")
    } else {
        Color::from_hex("#A94F32")
    }
}

fn codex_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#8AB4F8")
    } else {
        Color::from_hex("#1967D2")
    }
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
            diagnose::log(format!("broadcast helper created hwnd={:?}", hwnd));
            Some(hwnd)
        }
        Err(error) => {
            diagnose::log_error("broadcast helper: CreateWindowExW failed", error);
            None
        }
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
            check_theme_change();
            check_language_change();
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            // An open popup follows theme/DPI/work-area changes too.
            refresh_detail_popup_if_open();
            LRESULT(0)
        }
        // Revival ready signal, routed here instead of a thread message so a
        // modal message loop cannot discard it (see post_revive_ready).
        _ if msg == WM_APP_REVIVE_READY => {
            revive_execute();
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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
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

        let settings = load_settings();
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

        // Know when an RDP switch or lock screen is in progress so revival
        // and the watchdog can hold off until the session settles.
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                session_percent: 0.0,
                session_text: "--".to_string(),
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                codex_session_percent: 0.0,
                codex_session_text: "--".to_string(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: "--".to_string(),
                antigravity_session_percent: 0.0,
                antigravity_session_text: "--".to_string(),
                antigravity_weekly_percent: 0.0,
                antigravity_weekly_text: "--".to_string(),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
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
        }

        // Show the previous run's usage numbers immediately (marked as cached
        // in the detail popup) instead of "--" until the first poll lands.
        if let Some((cached_data, saved_unix)) = load_usage_cache() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                apply_usage_percents(s, &cached_data);
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

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            position_fallback_popup(hwnd);
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Registering our icons resizes the notification area asynchronously;
        // wait for its rect to settle so the first visible position is final
        // instead of being corrected (a visible jump) moments after showing.
        wait_for_tray_geometry_stable(Duration::from_secs(3));

        // Position and render first, show last: the widget appears in its
        // final place with real content instead of flashing into view first.
        position_at_taskbar();
        render_layered();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");
        schedule_countdown_timer();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Watch for explorer.exe restarts so we can re-embed and re-add the tray
        // icon (the shell discards tray registrations when it restarts). This
        // runs on a dedicated thread, NOT a window timer: once explorer destroys
        // the taskbar, our embedded child window stops receiving all messages
        // (WM_TIMER included), so a timer would never fire again.
        spawn_taskbar_watchdog();

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

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

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            // Thread messages (no window): revive after external destruction.
            // They cannot go through wnd_proc because the window is gone.
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REVIVE {
                revive_request();
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
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        embedded,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.embedded,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let antigravity_accent = antigravity_accent_color();
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

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

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            &codex_accent,
            &antigravity_accent,
        );

        // Background pixels -> alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels -> fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
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

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    accent: &Color,
    track: &Color,
    strings: Strings,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    antigravity_session_pct: f64,
    antigravity_session_text: &str,
    antigravity_weekly_pct: f64,
    antigravity_weekly_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    codex_accent: &Color,
    antigravity_accent: &Color,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_bottom = divider_top + divider_h;

        let (div_left, div_right) = if is_dark {
            ((80, 80, 80), (40, 40, 40))
        } else {
            ((160, 160, 160), (230, 230, 230))
        };

        let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_left.0, div_left.1, div_left.2,
        )));
        let left_rect = RECT {
            left: 0,
            top: divider_top,
            right: sc(2),
            bottom: divider_bottom,
        };
        FillRect(hdc, &left_rect, left_brush);
        let _ = DeleteObject(left_brush);

        let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_right.0,
            div_right.1,
            div_right.2,
        )));
        let right_rect = RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        };
        FillRect(hdc, &right_rect, right_brush);
        let _ = DeleteObject(right_brush);

        let content_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
        let row2_y = height - sc(5) - sc(SEGMENT_H);
        let row1_y = row2_y - sc(10) - sc(SEGMENT_H);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font = cached_font(sc(12), FW_MEDIUM.0 as i32);
        let old_font = SelectObject(hdc, font);

        draw_row(
            hdc,
            content_x,
            row1_y,
            is_dark,
            text_color,
            strings.session_window,
            session_pct,
            session_text,
            codex_session_pct,
            codex_session_text,
            antigravity_session_pct,
            antigravity_session_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            accent,
            codex_accent,
            antigravity_accent,
            track,
        );
        draw_row(
            hdc,
            content_x,
            row2_y,
            is_dark,
            text_color,
            strings.weekly_window,
            weekly_pct,
            weekly_text,
            codex_weekly_pct,
            codex_weekly_text,
            antigravity_weekly_pct,
            antigravity_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            accent,
            codex_accent,
            antigravity_accent,
            track,
        );

        SelectObject(hdc, old_font);
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let (show_claude_code, show_codex, show_antigravity) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.show_claude_code, s.show_codex, s.show_antigravity))
            .unwrap_or((true, false, false))
    };

    match poller::poll(show_claude_code, show_codex, show_antigravity) {
        Ok(data) => {
            let mut state = lock_state();
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

                apply_usage_percents(s, &merged);

                // Mirror of the arming condition in schedule_countdown_timer:
                // the 5s reset fast poll must stop not only when every window
                // refreshed, but also when the only past-reset windows belong
                // to a failing provider - merge carries its stale section for
                // the whole outage, so app_is_past_reset alone never clears.
                if !healthy_provider_past_reset(&merged) {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(merged);
                s.data_is_cached = false;
                s.last_error = None;
                s.last_poll_ok = true;
                s.last_success_unix = Some(now_unix_secs());
                refresh_usage_texts(s);

                for notification in reset_notifications {
                    diagnose::log(format!("reset notification shown: {}", notification.body));
                    tray_icon::notify_balloon(
                        hwnd,
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
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                        SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                    }
                } else if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
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

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(e) => {
            let auth_watch = match e {
                poller::PollError::AuthRequired | poller::PollError::TokenExpired
                    if show_antigravity && !show_claude_code && !show_codex =>
                {
                    Some((
                        poller::CredentialWatchMode::Antigravity,
                        poller::credential_watch_snapshot(poller::CredentialWatchMode::Antigravity),
                    ))
                }
                poller::PollError::AuthRequired | poller::PollError::TokenExpired => Some((
                    poller::CredentialWatchMode::ActiveSource,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::ActiveSource),
                )),
                poller::PollError::NoCredentials => Some((
                    poller::CredentialWatchMode::AllSources,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::AllSources),
                )),
                poller::PollError::RateLimited(_) | poller::PollError::RequestFailed => None,
            };
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
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
                            s.session_text = "!".to_string();
                            s.weekly_text = "!".to_string();
                            s.codex_session_text = "!".to_string();
                            s.codex_weekly_text = "!".to_string();
                            s.antigravity_session_text = "!".to_string();
                            s.antigravity_weekly_text = "!".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
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
                                s.session_text = "...".to_string();
                                s.weekly_text = "...".to_string();
                                s.codex_session_text = "...".to_string();
                                s.codex_weekly_text = "...".to_string();
                                s.antigravity_session_text = "...".to_string();
                                s.antigravity_weekly_text = "...".to_string();
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
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
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
                    tray_icon::notify_balloon(hwnd, kind, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
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
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
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
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let (changed, hwnd) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                (true, Some(s.hwnd.to_hwnd()))
            } else {
                (false, None)
            }
        } else {
            (false, None)
        }
    };
    if changed {
        render_layered();
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
        // Fallback popup mode: nothing to wait for.
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

/// Default placement for the fallback popup (no taskbar to embed into):
/// bottom-right of the primary work area, instead of wherever the window
/// happened to be created (0,0 - the top-left corner of the screen).
fn position_fallback_popup(hwnd: HWND) {
    refresh_dpi();
    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);
    let mut workarea = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut workarea as *mut RECT as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    if !ok {
        return;
    }
    let x = workarea.right - width - sc(16);
    let y = workarea.bottom - height - sc(16);
    native_interop::move_window(hwnd, x, y, width, height);
    diagnose::log(format!(
        "positioned fallback popup at default x={x} y={y} w={width} h={height}"
    ));
}

fn position_at_taskbar() {
    refresh_dpi();
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
            .map(|h| h == hwnd)
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
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
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
            match wparam.0 {
                WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOCK => {
                    diagnose::log(format!(
                        "session change {}: freezing watchdog/revival",
                        wparam.0
                    ));
                    SESSION_UNSTABLE_UNTIL.store(
                        now_unix_secs() + SESSION_UNSTABLE_MAX_SECS,
                        Ordering::SeqCst,
                    );
                }
                WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT | WTS_SESSION_UNLOCK => {
                    diagnose::log(format!("session change {}: session restored", wparam.0));
                    SESSION_UNSTABLE_UNTIL.store(0, Ordering::SeqCst);
                    // The taskbar may have been rebuilt or rescaled while the
                    // session was away (typical after an RDP switch).
                    refresh_dpi();
                    position_at_taskbar();
                    render_layered();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
                // The popup follows the system theme too; repaint if open.
                refresh_detail_popup_if_open();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
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
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    refresh_detail_popup_if_open();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            refresh_detail_popup_if_open();
            schedule_countdown_timer();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            sync_tray_icons(hwnd);
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
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
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
                    // Deliberate quit: WM_DESTROY (if any) must not revive.
                    QUIT_REQUESTED.store(true, Ordering::SeqCst);
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    if let Some(detail_hwnd) = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.details_hwnd)
                    } {
                        let _ = DestroyWindow(detail_hwnd);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                            s.preferred_tray_offset = 0;
                            s.preferred_taskbar_index = s.taskbar_index;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
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
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
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
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.antigravity_session_text = "...".to_string();
                            s.antigravity_weekly_text = "...".to_string();
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
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
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
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
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let hook = {
                let mut state = lock_state();
                state.as_mut().and_then(|s| s.win_event_hook.take())
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
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

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Update Frequency submenu
        let Ok(freq_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
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
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
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

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );
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

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
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

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
            ),
            None => return,
        }
    };

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let antigravity_accent = antigravity_accent_color();
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
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

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            &codex_accent,
            &antigravity_accent,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_row(
    hdc: HDC,
    x: i32,
    y: i32,
    is_dark: bool,
    text_color: &Color,
    label: &str,
    claude_percent: f64,
    claude_text: &str,
    codex_percent: f64,
    codex_text: &str,
    antigravity_percent: f64,
    antigravity_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    claude_accent: &Color,
    codex_accent: &Color,
    antigravity_accent: &Color,
    track: &Color,
) {
    let seg_h = sc(SEGMENT_H);
    let active_models = active_model_count(show_claude_code, show_codex, show_antigravity);
    let segment_count = row_bar_segment_count(active_models);
    let use_model_text_colors = active_models > 1;
    let claude_value_color = if use_model_text_colors {
        claude_usage_text_color(is_dark)
    } else {
        *text_color
    };
    let codex_value_color = if use_model_text_colors {
        codex_usage_text_color(is_dark)
    } else {
        *text_color
    };
    let antigravity_value_color = if use_model_text_colors {
        antigravity_usage_text_color(is_dark)
    } else {
        *text_color
    };

    unsafe {
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let mut model_x = x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN);
        if show_claude_code {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                claude_percent,
                claude_text,
                claude_accent,
                track,
                &claude_value_color,
            );
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_codex {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                codex_percent,
                codex_text,
                codex_accent,
                track,
                &codex_value_color,
            );
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_antigravity {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                antigravity_percent,
                antigravity_text,
                antigravity_accent,
                track,
                &antigravity_value_color,
            );
        }
    }
}

fn model_usage_width(segment_count: i32) -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * segment_count - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH)
}

fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    text_color: &Color,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let segment_percent = 100.0 / segment_count as f64;

        for i in 0..segment_count {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_percent;
            let seg_end = seg_start + segment_percent;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let text_x = bar_x + segment_count * (seg_w + seg_gap) - seg_gap + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
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

    fn section(resets_at: SystemTime) -> UsageSection {
        UsageSection {
            percentage: 0.0,
            resets_at: Some(resets_at),
        }
    }

    #[test]
    fn reset_window_refreshed_requires_elapsed_and_advanced_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_sub(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(reset_window_refreshed(
            &section(previous_reset),
            &section(next_reset)
        ));
    }

    #[test]
    fn reset_window_refreshed_ignores_predicted_future_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_add(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(!reset_window_refreshed(
            &section(previous_reset),
            &section(next_reset)
        ));
    }
}
